//! stepinfo (StepTrim dev tool): scan a STEP file, print header, statistics, timings and the
//! product tree. Phase 1 spike tool.

use anyhow::{Context, Result};
use clap::Parser;
use memmap2::Mmap;
use serde_json::json;
use std::fs::File;
use std::path::PathBuf;
use std::time::Instant;
use steptrim_core::scan::{scan, ScanConfig};
use steptrim_core::structure::ProductTree;

#[derive(Parser, Debug)]
#[command(name = "stepinfo", about = "Scan a STEP (ISO 10303-21) file and print its product structure")]
struct Args {
    /// STEP file to read
    file: PathBuf,
    /// Tree depth to print (0 = no tree)
    #[arg(long, default_value_t = 3)]
    tree_depth: usize,
    /// Show every occurrence instead of grouping identical siblings
    #[arg(long)]
    expand: bool,
    /// Number of entity types to list in the histogram
    #[arg(long, default_value_t = 12)]
    top_types: usize,
    /// Scan chunk size in MB
    #[arg(long, default_value_t = 4)]
    chunk_mb: usize,
    /// Worker threads (default: all logical cores)
    #[arg(long)]
    threads: Option<usize>,
    /// Write the expanded tree as JSON to this file
    #[arg(long)]
    json: Option<PathBuf>,
    /// Depth limit for the JSON tree (default: unlimited)
    #[arg(long)]
    json_depth: Option<usize>,
}

fn fmt_count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn main() -> Result<()> {
    let args = Args::parse();
    if let Some(t) = args.threads {
        rayon::ThreadPoolBuilder::new().num_threads(t).build_global().ok();
    }

    let t_start = Instant::now();
    let file = File::open(&args.file).with_context(|| format!("opening {}", args.file.display()))?;
    let mmap = unsafe { Mmap::map(&file)? };
    let data: &[u8] = &mmap;
    let mb = data.len() as f64 / 1_048_576.0;
    let t_mmap = t_start.elapsed();

    let t0 = Instant::now();
    let header = steptrim_core::header::parse_header(data);
    let t_header = t0.elapsed();

    let t0 = Instant::now();
    let table = scan(data, &ScanConfig { chunk_bytes: args.chunk_mb.max(1) << 20 })?;
    let t_scan = t0.elapsed();

    let t0 = Instant::now();
    let tree = steptrim_core::structure::build(data, &table);
    let t_tree = t0.elapsed();
    let t_total = t_start.elapsed();

    println!("File: {}  ({:.1} MB)", args.file.display(), mb);
    if let Some(h) = &header {
        println!(
            "Schema: {}\nOriginating system: {}   Preprocessor: {}   Level: {}",
            h.schemas.join(", "),
            h.originating_system,
            h.preprocessor_version,
            h.implementation_level
        );
        if let Some(d) = h.description.first() {
            println!("Description: {}", d);
        }
    } else {
        println!("Header: not found");
    }

    let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
    println!();
    println!("{:<12}{:>10}{:>12}", "Stage", "ms", "MB/s");
    println!("{:<12}{:>10.1}", "mmap", ms(t_mmap));
    println!("{:<12}{:>10.1}", "header", ms(t_header));
    println!(
        "{:<12}{:>10.1}{:>12.0}   ({} chunks, {} threads)",
        "scan",
        ms(t_scan),
        mb / t_scan.as_secs_f64(),
        table.stats.chunks,
        rayon::current_num_threads()
    );
    println!("{:<12}{:>10.1}", "structure", ms(t_tree));
    println!("{:<12}{:>10.1}{:>12.0}", "total", ms(t_total), mb / t_total.as_secs_f64());

    println!();
    println!(
        "Entities: {}   types: {}   max id: {}   duplicate ids: {}   skipped lines: {}",
        fmt_count(table.len() as u64),
        table.type_names.len(),
        fmt_count(table.max_id as u64),
        table.stats.duplicate_ids,
        table.stats.skipped_lines
    );
    println!(
        "Products: {}   definitions: {} ({} used in assembly)   occurrences: {}   roots: {}   instances (expanded): {}   depth: {}",
        fmt_count(tree.products.len() as u64),
        fmt_count(tree.definitions.len() as u64),
        fmt_count(tree.used_definitions() as u64),
        fmt_count(tree.occurrences.len() as u64),
        tree.roots.len(),
        fmt_count(tree.total_instances()),
        tree.max_depth()
    );
    let w = &tree.warnings;
    if w.nauo_unresolved + w.nauo_malformed + w.definition_without_product + w.cycles_broken > 0 {
        println!(
            "Warnings: unresolved occurrences {}  malformed occurrences {}  definitions without product {}  cycles broken {}",
            w.nauo_unresolved, w.nauo_malformed, w.definition_without_product, w.cycles_broken
        );
    }

    if args.top_types > 0 {
        println!();
        println!("Top entity types:");
        for (k, c) in table.histogram().into_iter().take(args.top_types) {
            println!("  {:>12}  {}", fmt_count(c as u64), table.kind_name(k));
        }
    }

    if args.tree_depth > 0 {
        println!();
        println!("Product tree (depth {}):", args.tree_depth);
        let mut shown = 0usize;
        for &root in &tree.roots {
            if tree.children[root as usize].is_empty() && !tree.definitions[root as usize].has_shape {
                continue; // definitions that are neither assemblies nor parts with shape
            }
            print_node(&tree, root, 0, args.tree_depth, args.expand, "");
            shown += 1;
            if shown >= 20 {
                println!("  ... {} more roots", tree.roots.len() - shown);
                break;
            }
        }
    }

    if let Some(path) = &args.json {
        let v = json!({
            "file": args.file.display().to_string(),
            "schema": header.as_ref().map(|h| h.schemas.clone()).unwrap_or_default(),
            "entities": table.len(),
            "products": tree.products.len(),
            "definitions": tree.definitions.len(),
            "occurrences": tree.occurrences.len(),
            "instances": tree.total_instances(),
            "roots": tree.roots.iter().map(|&r| json_node(&tree, r, None, 0, args.json_depth)).collect::<Vec<_>>(),
        });
        std::fs::write(path, serde_json::to_vec_pretty(&v)?)?;
        println!("\nWrote {}", path.display());
    }
    Ok(())
}

fn print_node(tree: &ProductTree, d: u32, level: usize, max: usize, expand: bool, occ_label: &str) {
    let indent = "  ".repeat(level);
    let name = tree.definition_name(d);
    let pid = tree.definition_pid(d);
    let nchildren = tree.children[d as usize].len();
    let kind = if nchildren > 0 { "asm" } else if tree.definitions[d as usize].has_shape { "part" } else { "def" };
    let pid_s = if !pid.is_empty() && pid != name { format!(" [{}]", pid) } else { String::new() };
    let inst = tree.instance_count[d as usize];
    let inst_s = if nchildren > 0 { format!("  ({} instances)", fmt_count(inst)) } else { String::new() };
    let bodies = tree.definitions[d as usize].bodies;
    let bodies_s = if bodies != 1 && (nchildren == 0 || bodies > 0) { format!(", {} bodies", bodies) } else { String::new() };
    println!("{}{}{}{}  <{} #{}{}>{}", indent, occ_label, name, pid_s, kind, tree.definitions[d as usize].entity, bodies_s, inst_s);
    if level + 1 >= max {
        if nchildren > 0 {
            println!("{}  ... {} children", indent, nchildren);
        }
        return;
    }
    if expand {
        for &o in &tree.children[d as usize] {
            let occ = &tree.occurrences[o as usize];
            let label = if !occ.name.is_empty() { format!("{}: ", occ.name) } else { String::new() };
            print_node(tree, occ.child, level + 1, max, expand, &label);
        }
    } else {
        // Group consecutive siblings with the same child definition.
        let list = &tree.children[d as usize];
        let mut i = 0;
        while i < list.len() {
            let child = tree.occurrences[list[i] as usize].child;
            let mut j = i;
            while j < list.len() && tree.occurrences[list[j] as usize].child == child {
                j += 1;
            }
            let n = j - i;
            let label = if n > 1 { format!("{}x ", n) } else { String::new() };
            print_node(tree, child, level + 1, max, expand, &label);
            i = j;
        }
    }
}

fn json_node(tree: &ProductTree, d: u32, occ: Option<u32>, level: usize, max: Option<usize>) -> serde_json::Value {
    let def = &tree.definitions[d as usize];
    let mut v = json!({
        "name": tree.definition_name(d),
        "pid": tree.definition_pid(d),
        "definition": def.entity,
        "has_shape": def.has_shape,
        "bodies": def.bodies,
    });
    if let Some(o) = occ {
        let occ = &tree.occurrences[o as usize];
        v["occurrence"] = json!(occ.entity);
        if !occ.name.is_empty() {
            v["occurrence_name"] = json!(occ.name);
        }
        if !occ.refdes.is_empty() {
            v["refdes"] = json!(occ.refdes);
        }
    }
    let truncated = max.map(|m| level + 1 >= m).unwrap_or(false);
    if !tree.children[d as usize].is_empty() {
        if truncated {
            v["children_count"] = json!(tree.children[d as usize].len());
        } else {
            v["children"] = serde_json::Value::Array(
                tree.children[d as usize]
                    .iter()
                    .map(|&o| json_node(tree, tree.occurrences[o as usize].child, Some(o), level + 1, max))
                    .collect(),
            );
        }
    }
    v
}
