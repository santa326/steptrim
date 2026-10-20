//! Background work: loading (scan, tree, transforms, instances, then bounds
//! and body geometry per definition) and saving (plan, prune, write), each
//! reported to the UI as events.

use memmap2::Mmap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Instant;
use steptrim_core::geom::{self, DefBounds, Instance, Transform, TransformStats};
use steptrim_core::header::Header;
use steptrim_core::prune;
use steptrim_core::sample::{self, DefGeometry, SampleConfig};
use steptrim_core::scan::{scan, EntityTable, ScanConfig};
use steptrim_core::structure::ProductTree;

#[derive(Clone, Debug, Default)]
#[allow(dead_code)]
pub struct Timings {
    pub mmap_ms: f64,
    pub scan_ms: f64,
    pub tree_ms: f64,
    pub transforms_ms: f64,
}

#[allow(dead_code)]
pub struct Loaded {
    pub path: PathBuf,
    pub size_bytes: u64,
    pub mmap: Mmap,
    pub header: Option<Header>,
    pub table: EntityTable,
    pub tree: ProductTree,
    pub transforms: Vec<Option<Transform>>,
    pub tstats: TransformStats,
    pub instances: Vec<Instance>,
    pub timings: Timings,
}

pub enum LoadEvent {
    Started { gen: u64, path: PathBuf, size_bytes: u64 },
    Stage { gen: u64, stage: &'static str },
    Loaded { gen: u64, loaded: Arc<Loaded> },
    Bounds { gen: u64, def: u32, bounds: DefBounds },
    BoundsDone { gen: u64, ms: f64 },
    /// `run` numbers sampling runs: 0 at load, then one per resample.
    Geometry { gen: u64, run: u64, def: u32, geom: Arc<DefGeometry> },
    GeometryDone { gen: u64, run: u64, ms: f64 },
    Error { gen: u64, message: String },
    Saved { gen: u64, path: PathBuf, summary: String },
    SaveFailed { gen: u64, message: String },
}

pub fn spawn(gen: u64, path: PathBuf, tx: Sender<LoadEvent>, ctx: eframe::egui::Context, cancel: Arc<AtomicBool>) {
    std::thread::Builder::new()
        .name(format!("steptrim-load-{gen}"))
        .spawn(move || {
            let send = |ev: LoadEvent| {
                let _ = tx.send(ev);
                ctx.request_repaint();
            };
            if let Err(e) = run(gen, path, &send, &cancel) {
                send(LoadEvent::Error { gen, message: format!("{e:#}") });
            }
        })
        .expect("spawn loader thread");
}

fn run(gen: u64, path: PathBuf, send: &(dyn Fn(LoadEvent) + Sync), cancel: &AtomicBool) -> anyhow::Result<()> {
    let t0 = Instant::now();
    let file = std::fs::File::open(&path)?;
    let size_bytes = file.metadata()?.len();
    let mmap = unsafe { Mmap::map(&file)? };
    let mut timings = Timings { mmap_ms: t0.elapsed().as_secs_f64() * 1000.0, ..Default::default() };
    send(LoadEvent::Started { gen, path: path.clone(), size_bytes });
    send(LoadEvent::Stage { gen, stage: "scanning entities" });

    let data: &[u8] = &mmap;
    let header = steptrim_core::header::parse_header(data);
    let t = Instant::now();
    let table = scan(data, &ScanConfig::default())?;
    timings.scan_ms = t.elapsed().as_secs_f64() * 1000.0;
    if cancel.load(Ordering::Relaxed) {
        return Ok(());
    }
    send(LoadEvent::Stage { gen, stage: "building product tree" });
    let t = Instant::now();
    let tree = steptrim_core::structure::build(data, &table);
    timings.tree_ms = t.elapsed().as_secs_f64() * 1000.0;
    let t = Instant::now();
    let (transforms, tstats) = geom::occurrence_transforms(data, &table, &tree);
    let instances = geom::expand_instances(&tree, &transforms);
    timings.transforms_ms = t.elapsed().as_secs_f64() * 1000.0;

    let loaded = Arc::new(Loaded { path, size_bytes, mmap, header, table, tree, transforms, tstats, instances, timings });
    send(LoadEvent::Loaded { gen, loaded: loaded.clone() });
    if cancel.load(Ordering::Relaxed) {
        return Ok(());
    }
    send(LoadEvent::Stage { gen, stage: "computing part boxes" });
    let t = Instant::now();
    let data: &[u8] = &loaded.mmap;
    let bounds = geom::definition_bounds(data, &loaded.table, &loaded.tree, |def, b| {
        if !cancel.load(Ordering::Relaxed) {
            send(LoadEvent::Bounds { gen, def, bounds: *b });
        }
    });
    send(LoadEvent::BoundsDone { gen, ms: t.elapsed().as_secs_f64() * 1000.0 });
    if cancel.load(Ordering::Relaxed) {
        return Ok(());
    }
    send(LoadEvent::Stage { gen, stage: "sampling bodies" });
    let t = Instant::now();
    let cfg = SampleConfig::default();
    sample::definition_geometry(data, &loaded.table, &loaded.tree, &bounds, &cfg, |def, g| {
        if !cancel.load(Ordering::Relaxed) {
            send(LoadEvent::Geometry { gen, run: 0, def, geom: Arc::new(g.clone()) });
        }
    });
    send(LoadEvent::GeometryDone { gen, run: 0, ms: t.elapsed().as_secs_f64() * 1000.0 });
    Ok(())
}

/// Re-run body sampling with a new configuration on a worker thread.
pub fn spawn_resample(
    gen: u64,
    run: u64,
    loaded: Arc<Loaded>,
    bounds: Vec<Option<DefBounds>>,
    cfg: SampleConfig,
    tx: Sender<LoadEvent>,
    ctx: eframe::egui::Context,
    cancel: Arc<AtomicBool>,
) {
    std::thread::Builder::new()
        .name(format!("steptrim-sample-{gen}-{run}"))
        .spawn(move || {
            let send = |ev: LoadEvent| {
                let _ = tx.send(ev);
                ctx.request_repaint();
            };
            let t = Instant::now();
            let data: &[u8] = &loaded.mmap;
            sample::definition_geometry(data, &loaded.table, &loaded.tree, &bounds, &cfg, |def, g| {
                if !cancel.load(Ordering::Relaxed) {
                    send(LoadEvent::Geometry { gen, run, def, geom: Arc::new(g.clone()) });
                }
            });
            if !cancel.load(Ordering::Relaxed) {
                send(LoadEvent::GeometryDone { gen, run, ms: t.elapsed().as_secs_f64() * 1000.0 });
            }
        })
        .expect("spawn sampling thread");
}

/// Write several pruned files from one loaded model (one graph build),
/// reporting a single summary when done.
pub fn spawn_save_many(gen: u64, loaded: Arc<Loaded>, jobs: Vec<(PathBuf, prune::PruneRequest)>, tx: Sender<LoadEvent>, ctx: eframe::egui::Context) {
    std::thread::Builder::new()
        .name(format!("steptrim-export-{gen}"))
        .spawn(move || {
            let send = |ev: LoadEvent| {
                let _ = tx.send(ev);
                ctx.request_repaint();
            };
            let result = (|| -> anyhow::Result<(PathBuf, String)> {
                let t0 = Instant::now();
                let data: &[u8] = &loaded.mmap;
                let graph = prune::build_graph(data, &loaded.table);
                let mut total_bytes = 0u64;
                let mut written = 0usize;
                let mut first = PathBuf::new();
                for (out, req) in &jobs {
                    let plan = prune::plan_request(data, &loaded.table, &loaded.tree, &graph, req);
                    let file = std::fs::File::create(out)?;
                    let mut w = std::io::BufWriter::with_capacity(8 << 20, file);
                    let ws = prune::write_pruned(data, &loaded.table, &plan, &mut w)?;
                    use std::io::Write as _;
                    w.flush()?;
                    total_bytes += ws.bytes;
                    written += 1;
                    if written == 1 {
                        first = out.clone();
                    }
                }
                let dir = first.parent().map(|p| p.display().to_string()).unwrap_or_default();
                Ok((
                    first,
                    format!("{} file(s), {:.1} MB, in {:.1} s into {}", written, total_bytes as f64 / 1_048_576.0, t0.elapsed().as_secs_f64(), dir),
                ))
            })();
            match result {
                Ok((path, summary)) => send(LoadEvent::Saved { gen, path, summary }),
                Err(e) => send(LoadEvent::SaveFailed { gen, message: format!("{e:#}") }),
            }
        })
        .expect("spawn export thread");
}

/// Prune the removed occurrences and write the result to `out` on a worker
/// thread.
pub fn spawn_save(gen: u64, loaded: Arc<Loaded>, remove: Vec<u32>, out: PathBuf, tx: Sender<LoadEvent>, ctx: eframe::egui::Context) {
    std::thread::Builder::new()
        .name(format!("steptrim-save-{gen}"))
        .spawn(move || {
            let send = |ev: LoadEvent| {
                let _ = tx.send(ev);
                ctx.request_repaint();
            };
            let result = (|| -> anyhow::Result<String> {
                let t0 = Instant::now();
                let data: &[u8] = &loaded.mmap;
                let graph = prune::build_graph(data, &loaded.table);
                let plan = prune::plan(data, &loaded.table, &loaded.tree, &graph, &remove);
                let file = std::fs::File::create(&out)?;
                let mut w = std::io::BufWriter::with_capacity(8 << 20, file);
                let ws = prune::write_pruned(data, &loaded.table, &plan, &mut w)?;
                use std::io::Write as _;
                w.flush()?;
                Ok(format!(
                    "{:.1} MB written in {:.1} s: removed {} occurrences, {} products no longer used, {} of {} entities deleted, {} rewritten{}",
                    ws.bytes as f64 / 1_048_576.0,
                    t0.elapsed().as_secs_f64(),
                    plan.stats.removed_occurrences,
                    plan.stats.orphaned_definitions,
                    ws.deleted,
                    loaded.table.len(),
                    ws.rewritten,
                    if ws.dangling_single_refs > 0 { format!(", {} dangling refs (!)", ws.dangling_single_refs) } else { String::new() }
                ))
            })();
            match result {
                Ok(summary) => send(LoadEvent::Saved { gen, path: out, summary }),
                Err(e) => send(LoadEvent::SaveFailed { gen, message: format!("{e:#}") }),
            }
        })
        .expect("spawn save thread");
}
