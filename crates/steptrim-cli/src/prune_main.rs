//! stepprune (StepTrim dev tool): remove occurrences from a STEP assembly
//! from the command line and write the trimmed file, then check it.

use anyhow::{bail, Context, Result};
use clap::Parser;
use memmap2::Mmap;
use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;
use std::time::Instant;
use steptrim_core::prune;
use steptrim_core::scan::{scan, ScanConfig};

#[derive(Parser, Debug)]
#[command(name = "stepprune", about = "Remove occurrences from a STEP assembly and write the trimmed file")]
struct Args {
    /// Source STEP file
    file: PathBuf,
    /// Output file (default: <file>-trimmed.stp)
    #[arg(long)]
    out: Option<PathBuf>,
    /// Remove every occurrence whose child product name contains this text (case-insensitive); repeatable
    #[arg(long = "remove-name")]
    remove_name: Vec<String>,
    /// Remove the occurrence with this NEXT_ASSEMBLY_USAGE_OCCURRENCE entity id; repeatable
    #[arg(long = "remove-occurrence")]
    remove_occurrence: Vec<u32>,
    /// Remove every occurrence under this parent definition whose child name contains the text, as "parent::child"
    #[arg(long = "remove-under")]
    remove_under: Vec<String>,
    /// Only plan and report; do not write
    #[arg(long)]
    dry_run: bool,
    /// Re-scan the output and check it
    #[arg(long, default_value_t = true)]
    check: bool,
    /// For orphaned definitions whose shape representations survive, list what still references them
    #[arg(long)]
    explain: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let t0 = Instant::now();
    let file = File::open(&args.file).with_context(|| format!("opening {}", args.file.display()))?;
    let mmap = unsafe { Mmap::map(&file)? };
    let data: &[u8] = &mmap;
    let table = scan(data, &ScanConfig::default())?;
    let tree = steptrim_core::structure::build(data, &table);
    println!("Scanned {} entities, {} occurrences in {:.0} ms", table.len(), tree.occurrences.len(), t0.elapsed().as_secs_f64() * 1000.0);

    // Select occurrences.
    let mut selected: Vec<u32> = Vec::new();
    for (i, occ) in tree.occurrences.iter().enumerate() {
        let child = tree.definition_name(occ.child).to_lowercase();
        let parent = tree.definition_name(occ.parent).to_lowercase();
        let mut hit = args.remove_occurrence.contains(&occ.entity);
        if !hit {
            hit = args.remove_name.iter().any(|n| child.contains(&n.to_lowercase()));
        }
        if !hit {
            hit = args.remove_under.iter().any(|spec| {
                let mut it = spec.splitn(2, "::");
                let p = it.next().unwrap_or("").to_lowercase();
                let c = it.next().unwrap_or("").to_lowercase();
                parent.contains(&p) && child.contains(&c)
            });
        }
        if hit {
            selected.push(i as u32);
        }
    }
    if selected.is_empty() {
        bail!("no occurrences matched");
    }
    println!("Removing {} occurrences:", selected.len());
    for &o in selected.iter().take(12) {
        let occ = &tree.occurrences[o as usize];
        println!(
            "  #{}  {}  (under {}, subtree {} instances)",
            occ.entity,
            tree.definition_name(occ.child),
            tree.definition_name(occ.parent),
            tree.instance_count[occ.child as usize]
        );
    }
    if selected.len() > 12 {
        println!("  ... {} more", selected.len() - 12);
    }

    let t = Instant::now();
    let graph = prune::build_graph(data, &table);
    let graph_ms = t.elapsed().as_secs_f64() * 1000.0;
    let plan = prune::plan(data, &table, &tree, &graph, &selected);
    let s = &plan.stats;
    println!(
        "Plan: graph {:.0} ms ({} refs, {} unresolved), propagate {:.0} ms; delete {} entities, rewrite {}, keep {}; orphaned definitions {}, properties dropped on {} parents: {}",
        graph_ms,
        graph.fwd.len(),
        graph.unresolved,
        s.propagate_ms,
        s.deleted_entities,
        s.rewritten_entities,
        s.kept_entities,
        s.orphaned_definitions,
        s.affected_parents,
        s.deleted_properties
    );
    if args.explain {
        let mut shown = 0;
        for (d, def) in tree.definitions.iter().enumerate() {
            let Some(di) = table.index_of(def.entity) else { continue };
            if !plan.deleted[di] {
                continue;
            }
            for &sr in &def.shape_reps {
                let Some(si) = table.index_of(sr) else { continue };
                if plan.deleted[si] {
                    continue;
                }
                let alive: Vec<String> = graph
                    .referrers(si)
                    .iter()
                    .filter(|&&r| !plan.deleted[r as usize])
                    .take(6)
                    .map(|&r| format!("#{}={}", table.entities[r as usize].id, table.kind_name(table.entities[r as usize].kind)))
                    .collect();
                println!(
                    "  orphaned '{}' (#{}): rep #{} {} survives; owners {} / refs {}; alive referrers: {}",
                    tree.definition_name(d as u32),
                    def.entity,
                    sr,
                    table.kind_name(table.entities[si].kind),
                    graph.owners[si],
                    graph.refcount[si],
                    alive.join(", ")
                );
                shown += 1;
                if shown >= 6 {
                    break;
                }
            }
            if shown >= 6 {
                break;
            }
        }
        if shown == 0 {
            println!("  (all shape representations of orphaned definitions were deleted)");
        }
    }
    if args.dry_run {
        return Ok(());
    }
    let out_path = args.out.clone().unwrap_or_else(|| {
        let stem = args.file.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or("out".into());
        args.file.with_file_name(format!("{stem}-trimmed.stp"))
    });
    let mut w = BufWriter::with_capacity(8 << 20, File::create(&out_path)?);
    let ws = prune::write_pruned(data, &table, &plan, &mut w)?;
    drop(w);
    println!(
        "Wrote {} ({:.1} MB) in {:.0} ms: kept {}, deleted {}, rewritten {}, dangling single refs {}",
        out_path.display(),
        ws.bytes as f64 / 1_048_576.0,
        ws.write_ms,
        ws.kept,
        ws.deleted,
        ws.rewritten,
        ws.dangling_single_refs
    );

    if args.check {
        let t = Instant::now();
        let f2 = File::open(&out_path)?;
        let m2 = unsafe { Mmap::map(&f2)? };
        let d2: &[u8] = &m2;
        let t2 = scan(d2, &ScanConfig::default())?;
        let tree2 = steptrim_core::structure::build(d2, &t2);
        let (refs, bad, examples) = prune::check_refs(d2, &t2);
        println!(
            "Check ({:.0} ms): {} entities, {} refs, {} unresolved{}; products {}, occurrences {}, roots {}, instances {} (was {} / {} / {} / {})",
            t.elapsed().as_secs_f64() * 1000.0,
            t2.len(),
            refs,
            bad,
            if examples.is_empty() { String::new() } else { format!(" e.g. {:?}", examples) },
            tree2.products.len(),
            tree2.occurrences.len(),
            tree2.roots.len(),
            tree2.total_instances(),
            tree.products.len(),
            tree.occurrences.len(),
            tree.roots.len(),
            tree.total_instances()
        );
        if bad > 0 || tree2.roots.len() != tree.roots.len() {
            bail!("output check failed");
        }
    }
    println!("Total {:.1} s", t0.elapsed().as_secs_f64());
    Ok(())
}
