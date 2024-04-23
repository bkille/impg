use clap::Parser;
use std::fs::File;
use std::io::{self, BufReader, BufWriter};
use std::num::NonZeroUsize;
use noodles::bgzf;
use impg::impg::{Impg, SerializableImpg, QueryInterval};
use coitrees::IntervalTree;
use impg::paf;
use rayon::ThreadPoolBuilder;
use std::io::BufRead;

/// Command-line tool for querying overlaps in PAF files.
#[derive(Parser, Debug)]
#[clap(author, version, about)]
struct Args {
    /// Path to the PAF file. If specified without an index, the tool will look for or generate an associated index file.
    #[clap(short='p', long, value_parser)]
    paf_file: Option<String>,

    /// Force the regeneration of the index, even if it already exists.
    #[clap(short='I', long, action)]
    force_reindex: bool,

    /// Target range in the format `seq_name:start-end`.
    #[clap(short='r', long, value_parser)]
    target_range: Option<String>,

    /// Path to the BED file containing target regions.
    #[clap(short='b', long, value_parser)]
    target_bed: Option<String>,

    /// Enable transitive overlap requests.
    #[clap(short='x', long, action)]
    transitive: bool,

    /// Output results in PAF format.
    #[clap(short='P', long, action)]
    output_paf: bool,
        
    /// Print stats about the index.
    #[clap(short='s', long, action)]
    stats: bool,

    /// Number of threads for parallel processing.
    #[clap(short='t', long, value_parser, default_value_t = NonZeroUsize::new(1).unwrap())]
    num_threads: NonZeroUsize,
}

fn main() -> io::Result<()> {
    let args = Args::parse();

    // Configure the global thread pool to use the specified number of threads
    ThreadPoolBuilder::new().num_threads(args.num_threads.into()).build_global().unwrap();

    let impg = match args {
        Args { paf_file: Some(paf), force_reindex: false, .. } => load_or_generate_index(&paf, args.num_threads)?,
        Args { paf_file: Some(paf), force_reindex: true, .. } => generate_index(&paf, args.num_threads)?,
        _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, "A PAF file must be provided")),
    };

    if args.stats {
        print_stats(&impg);
    }

    if let Some(target_range) = args.target_range {
        let (target_name, target_range) = parse_target_range(&target_range)?;
        let results = perform_query(&impg, &target_name, target_range, args.transitive);
        if args.output_paf {
            output_results_paf(&impg, results, &target_name, target_range, None);
        } else {
            output_results_bed(&impg, results);
        }
    } else if let Some(target_bed) = args.target_bed {
        let targets = parse_bed_file(&target_bed)?;
        for (target_name, target_range, name) in targets {
            let results = perform_query(&impg, &target_name, target_range, args.transitive);
            if args.output_paf {
                output_results_paf(&impg, results, &target_name, target_range, name);
            } else {
                output_results_bedpe(&impg, results, &target_name, target_range, name);
            }
        }
    }
    Ok(())
}

fn parse_bed_file(bed_file: &str) -> io::Result<Vec<(String, (i32, i32), Option<String>)>> {
    let file = File::open(bed_file)?;
    let reader = BufReader::new(file);
    let mut queries = Vec::new();

    for line in reader.lines() {
        let line = line?;
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 3 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "Invalid BED file format"));
        }

        let start = parts[1].parse::<i32>().map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "Invalid start value"))?;
        let end = parts[2].parse::<i32>().map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "Invalid end value"))?;

        if start >= end {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "Start value must be less than end value"));
        }

        let name = if parts.len() > 3 { Some(parts[3].to_string()) } else { None };
        queries.push((parts[0].to_string(), (start, end), name));
    }

    Ok(queries)
}


fn load_or_generate_index(paf_file: &str, num_threads: NonZeroUsize) -> io::Result<Impg> {
    let index_file = format!("{}.impg", paf_file);
    if std::path::Path::new(&index_file).exists() {
        load_index(paf_file)
    } else {
        generate_index(paf_file, num_threads)
    }
}

fn generate_index(paf_file: &str, num_threads: NonZeroUsize) -> io::Result<Impg> {
    let file = File::open(paf_file)?;
    let reader: Box<dyn io::Read> = if [".gz", ".bgz"].iter().any(|e| paf_file.ends_with(e)) {
        Box::new(bgzf::MultithreadedReader::with_worker_count(num_threads, file))
    } else {
        Box::new(file)
    };
    let reader = BufReader::new(reader);
    let records = paf::parse_paf(reader).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("Failed to parse PAF records: {:?}", e)))?;
    let impg = Impg::from_paf_records(&records, paf_file).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("Failed to create index: {:?}", e)))?;

    let index_file = format!("{}.impg", paf_file);
    let serializable = impg.to_serializable();
    let file = File::create(index_file)?;
    let writer = BufWriter::new(file);
    bincode::serialize_into(writer, &serializable).map_err(|e| io::Error::new(io::ErrorKind::Other, format!("Failed to serialize index: {:?}", e)))?;

    Ok(impg)
}

fn load_index(paf_file: &str) -> io::Result<Impg> {
    let index_file = format!("{}.impg", paf_file);
    let file = File::open(index_file)?;
    let reader = BufReader::new(file);
    let serializable: SerializableImpg = bincode::deserialize_from(reader).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("Failed to deserialize index: {:?}", e)))?;
    Ok(Impg::from_serializable(paf_file, serializable))
}

fn parse_target_range(target_range: &str) -> io::Result<(String, (i32, i32))> {
    let parts: Vec<&str> = target_range.split(':').collect();
    if parts.len() != 2 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Target range format should be `seq_name:start-end`"));
    }
    let range_parts: Vec<&str> = parts[1].split('-').collect();
    if range_parts.len() != 2 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Target range format should be `start-end`"));
    }

    let start = range_parts[0].parse::<i32>().map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "Invalid start value"))?;
    let end = range_parts[1].parse::<i32>().map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "Invalid end value"))?;

    if start >= end {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Start value must be less than end value"));
    }

    Ok((parts[0].to_string(), (start, end)))
}

fn perform_query(impg: &Impg, target_name: &str, target_range: (i32, i32), transitive: bool) -> Vec<QueryInterval> {
    let (target_start, target_end) = target_range;
    let target_id = impg.seq_index.get_id(target_name).expect("Target name not found in index");
    if transitive {
        impg.query_transitive(target_id, target_start, target_end)
    } else {
        impg.query(target_id, target_start, target_end)
    }
}

fn output_results_bed(impg: &Impg, results: Vec<QueryInterval>) {
    for (overlap, _) in results {
        let overlap_name = impg.seq_index.get_name(overlap.metadata).unwrap();
        let (first, last, strand) = if overlap.first <= overlap.last {
            (overlap.first, overlap.last, '+')
        } else {
            (overlap.last, overlap.first, '-')
        };
        println!("{}\t{}\t{}\t.\t{}", overlap_name, first, last, strand);
    }
}

fn output_results_bedpe(impg: &Impg, results: Vec<QueryInterval>, target_name: &str, target_range: (i32, i32), name: Option<String>) {
    for (overlap, _) in results {
        let overlap_name = impg.seq_index.get_name(overlap.metadata).unwrap();
        let (first, last, strand) = if overlap.first <= overlap.last {
            (overlap.first, overlap.last, '+')
        } else {
            (overlap.last, overlap.first, '-')
        };
        println!("{}\t{}\t{}\t{}\t{}\t{}\t{}\t0\t{}\t+",
                 overlap_name, first, last,
                 target_name, target_range.0, target_range.1,
                 name.as_deref().unwrap_or("."), strand);
    }
}

fn output_results_paf(impg: &Impg, results: Vec<QueryInterval>, target_name: &str, target_range: (i32, i32), name: Option<String>) { 
    let target_length = impg.seq_index.get_len_from_id(impg.seq_index.get_id(target_name).unwrap()).unwrap();  
    for (overlap, cigar) in results {
        let overlap_name = impg.seq_index.get_name(overlap.metadata).unwrap();
        let (first, last, strand) = if overlap.first <= overlap.last {
            (overlap.first, overlap.last, '+')
        } else {
            (overlap.last, overlap.first, '-')
        };

        let query_length = impg.seq_index.get_len_from_id(overlap.metadata).unwrap();  

        let has_m_operation = cigar.iter().any(|op| op.op() == 'M');
        let (matches, block_len) = if has_m_operation {
            // We overestimate the number of matches by counting all M operations
            cigar.iter().fold((0, 0), |(matches, block_len), op| {
                let len = op.len();
                match op.op() {
                    'M' => (matches + len, block_len + len),
                    'I' | 'D' => (matches, block_len + len),
                    _ => (matches, block_len),
                }
            })
        } else {
            cigar.iter().fold((0, 0), |(matches, block_len), op| {
                let len = op.len();
                match op.op() {
                    '=' => (matches + len, block_len + len),
                    'X' | 'I' | 'D' => (matches, block_len + len),
                    _ => (matches, block_len),
                }
            })
        };

        let cigar_str : String = cigar.iter().map(|op| format!("{}{}", op.len(), op.op())).collect();

        match name {
            Some(ref name) => println!("{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\tcg:Z:{}\tan:Z:{}",
                                    overlap_name, query_length, first, last, strand,
                                    target_name, target_length, target_range.0, target_range.1,
                                    matches, block_len, 255, cigar_str, name),
            None => println!("{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\tcg:Z:{}",
                                overlap_name, query_length, overlap.first, overlap.last, strand,
                                target_name, target_length, target_range.0, target_range.1,
                                matches, block_len, 255, cigar_str),
        }
    }
}


fn print_stats(impg: &Impg) {
    println!("Number of sequences: {}", impg.seq_index.len());
    println!("Number of overlaps: {}", impg.trees.values().map(|tree| tree.len()).sum::<usize>());
}
