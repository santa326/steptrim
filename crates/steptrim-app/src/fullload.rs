//! Full load: exact B-rep tessellation of selected parts through an Open
//! CASCADE kernel hosted by FreeCAD's command-line binary. Each part is
//! exported as a standalone STEP file (the same pruner as Save), meshed by
//! one FreeCADCmd run, and streamed back as triangles in the part's own
//! frame as each part finishes.

use crate::loader::{LoadEvent, Loaded};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Instant;
use steptrim_core::prune;
use steptrim_core::sample::MeshVertex;

/// Locate FreeCADCmd.exe: the STEPTRIM_FREECAD variable, then the usual
/// install folders, then PATH.
pub fn find_freecad() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("STEPTRIM_FREECAD") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    for root in ["C:\\Program Files", "C:\\Program Files (x86)"] {
        if let Ok(rd) = std::fs::read_dir(root) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.to_lowercase().starts_with("freecad") {
                    candidates.push(e.path().join("bin").join("FreeCADCmd.exe"));
                }
            }
        }
    }
    candidates.sort();
    candidates.reverse(); // newest version first
    for c in candidates {
        if c.is_file() {
            return Some(c);
        }
    }
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(';') {
            let c = Path::new(dir).join("FreeCADCmd.exe");
            if c.is_file() {
                return Some(c);
            }
        }
    }
    None
}

/// Runs inside FreeCADCmd. One line per result on stdout, keyed by the
/// job index: `BBOX k diag`, `OK k facets seconds`, `FAIL k reason`.
const SCRIPT: &str = r#"
import os, json, time, traceback, sys
import FreeCAD as App
import Part, MeshPart, Import
job = json.load(open(os.environ["STEPTRIM_JOB"], encoding="utf-8"))
def say(*a):
    sys.stdout.write(" ".join(str(x) for x in a) + "\n")
    sys.stdout.flush()
for k, item in enumerate(job["items"]):
    t0 = time.time()
    try:
        doc = App.newDocument("full%d" % k)
        Import.insert(item["step"], doc.Name)
        shapes = []
        for o in doc.RootObjects:
            try:
                s = Part.getShape(o)
            except Exception:
                s = getattr(o, "Shape", None)
            if s is None or s.isNull() or len(s.Faces) == 0:
                continue
            shapes.append(s)
        if not shapes:
            say("FAIL", k, "no faces after import")
            App.closeDocument(doc.Name)
            continue
        comp = shapes[0] if len(shapes) == 1 else Part.makeCompound(shapes)
        bb = comp.BoundBox
        say("BBOX", k, bb.DiagonalLength)
        m = MeshPart.meshFromShape(Shape=comp, LinearDeflection=item["deflection"], AngularDeflection=item["angular"], Relative=False)
        m.write(item["stl"])
        say("OK", k, len(m.Facets), round(time.time() - t0, 2))
        App.closeDocument(doc.Name)
    except Exception as e:
        say("FAIL", k, repr(e))
        traceback.print_exc()
"#;

/// Triangles with flat normals, read from a binary or ASCII STL.
pub fn read_stl(path: &Path) -> std::io::Result<(Vec<MeshVertex>, Vec<u32>)> {
    let bytes = std::fs::read(path)?;
    let mut verts: Vec<MeshVertex> = Vec::new();
    let mut idx: Vec<u32> = Vec::new();
    let is_binary = bytes.len() >= 84 && {
        let n = u32::from_le_bytes([bytes[80], bytes[81], bytes[82], bytes[83]]) as usize;
        bytes.len() == 84 + n * 50
    };
    let mut push_tri = |n: [f32; 3], a: [f32; 3], b: [f32; 3], c: [f32; 3]| {
        // recompute the normal from the vertices; STL normals are often zero
        let u = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
        let v = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
        let mut nn = [u[1] * v[2] - u[2] * v[1], u[2] * v[0] - u[0] * v[2], u[0] * v[1] - u[1] * v[0]];
        let len = (nn[0] * nn[0] + nn[1] * nn[1] + nn[2] * nn[2]).sqrt();
        if len > 1e-20 {
            nn = [nn[0] / len, nn[1] / len, nn[2] / len];
        } else {
            nn = n;
        }
        let base = verts.len() as u32;
        verts.push(MeshVertex { pos: a, nrm: nn });
        verts.push(MeshVertex { pos: b, nrm: nn });
        verts.push(MeshVertex { pos: c, nrm: nn });
        idx.extend_from_slice(&[base, base + 1, base + 2]);
    };
    if is_binary {
        let n = u32::from_le_bytes([bytes[80], bytes[81], bytes[82], bytes[83]]) as usize;
        let f = |o: usize| f32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]);
        for t in 0..n {
            let o = 84 + t * 50;
            let nrm = [f(o), f(o + 4), f(o + 8)];
            let a = [f(o + 12), f(o + 16), f(o + 20)];
            let b = [f(o + 24), f(o + 28), f(o + 32)];
            let c = [f(o + 36), f(o + 40), f(o + 44)];
            push_tri(nrm, a, b, c);
        }
    } else {
        let text = String::from_utf8_lossy(&bytes);
        let mut cur: Vec<[f32; 3]> = Vec::with_capacity(3);
        let mut nrm = [0.0f32; 3];
        for line in text.lines() {
            let mut it = line.split_whitespace();
            match it.next() {
                Some("facet") => {
                    it.next();
                    let v: Vec<f32> = it.filter_map(|x| x.parse().ok()).collect();
                    if v.len() == 3 {
                        nrm = [v[0], v[1], v[2]];
                    }
                }
                Some("vertex") => {
                    let v: Vec<f32> = it.filter_map(|x| x.parse().ok()).collect();
                    if v.len() == 3 {
                        cur.push([v[0], v[1], v[2]]);
                    }
                }
                Some("endfacet") => {
                    if cur.len() == 3 {
                        push_tri(nrm, cur[0], cur[1], cur[2]);
                    }
                    cur.clear();
                }
                _ => {}
            }
        }
    }
    Ok((verts, idx))
}

/// The kernel reads STEP into millimetres while the viewer shows the file's
/// own coordinates. When the kernel's box diagonal differs from ours by a
/// known unit factor, return the factor that brings the mesh back.
pub fn unit_scale(our_diag: f64, occt_diag: f64) -> Option<f32> {
    if !(our_diag > 0.0) || !(occt_diag > 0.0) {
        return None;
    }
    let ratio = our_diag / occt_diag;
    if (ratio - 1.0).abs() < 0.03 {
        return None;
    }
    const INCH: f64 = 25.4;
    for f in [1.0 / INCH, INCH, 0.001, 1000.0, 0.1, 10.0, 0.01, 100.0, 1.0 / (INCH * 12.0), INCH * 12.0] {
        if (ratio / f - 1.0).abs() < 0.03 {
            return Some(f as f32);
        }
    }
    None
}

/// One part to tessellate.
pub struct FullLoadJob {
    pub def: u32,
    pub name: String,
    /// Box diagonal in file units, to scale the deflection; 0 when unknown.
    pub diag: f64,
}

/// Export, mesh and read back the given definitions on a worker thread.
pub fn spawn_full_load(gen: u64, loaded: Arc<Loaded>, jobs: Vec<FullLoadJob>, freecad: PathBuf, tx: Sender<LoadEvent>, ctx: eframe::egui::Context) {
    std::thread::Builder::new()
        .name(format!("steptrim-full-{gen}"))
        .spawn(move || {
            let send = |ev: LoadEvent| {
                let _ = tx.send(ev);
                ctx.request_repaint();
            };
            let t0 = Instant::now();
            let result = (|| -> anyhow::Result<(usize, usize, String)> {
                let dir = std::env::temp_dir().join(format!("steptrim-full-{}-{}", std::process::id(), gen));
                std::fs::create_dir_all(&dir)?;
                let data: &[u8] = &loaded.mmap;
                send(LoadEvent::Stage { gen, stage: "full load: exporting parts" });
                let graph = prune::build_graph(data, &loaded.table);
                let mut items = Vec::new();
                let mut stls: Vec<PathBuf> = Vec::new();
                for (k, job) in jobs.iter().enumerate() {
                    let step = dir.join(format!("part{k}.stp"));
                    let stl = dir.join(format!("part{k}.stl"));
                    let req = prune::standalone_request(&loaded.tree, job.def);
                    let plan = prune::plan_request(data, &loaded.table, &loaded.tree, &graph, &req);
                    let mut w = std::io::BufWriter::new(std::fs::File::create(&step)?);
                    prune::write_pruned(data, &loaded.table, &plan, &mut w)?;
                    use std::io::Write as _;
                    w.flush()?;
                    let diag = if job.diag > 0.0 { job.diag } else { 100.0 };
                    let deflection = (diag * 0.0008).clamp(0.005, 5.0);
                    items.push(serde_json::json!({
                        "step": step.to_string_lossy().replace('\\', "/"),
                        "stl": stl.to_string_lossy().replace('\\', "/"),
                        "deflection": deflection,
                        "angular": 0.35,
                    }));
                    stls.push(stl);
                }
                let job_path = dir.join("job.json");
                std::fs::write(&job_path, serde_json::to_vec_pretty(&serde_json::json!({ "items": items }))?)?;
                let script_path = dir.join("mesh.py");
                std::fs::write(&script_path, SCRIPT)?;
                send(LoadEvent::Stage { gen, stage: "full load: meshing with OCCT" });
                let script_arg = format!("exec(open(r'{}').read())", script_path.to_string_lossy());
                let mut child = std::process::Command::new(&freecad)
                    .arg("-c")
                    .arg(&script_arg)
                    .env("STEPTRIM_JOB", &job_path)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .map_err(|e| anyhow::anyhow!("could not run {}: {e}", freecad.display()))?;
                // drain stderr on its own thread so neither pipe can block the child
                let stderr_pipe = child.stderr.take();
                let stderr_thread = std::thread::spawn(move || {
                    let mut s = String::new();
                    if let Some(mut p) = stderr_pipe {
                        use std::io::Read as _;
                        let _ = p.read_to_string(&mut s);
                    }
                    s
                });
                let stdout = child.stdout.take().expect("piped stdout");
                let mut ok = 0usize;
                let mut failed = 0usize;
                let mut triangles = 0usize;
                let mut rescaled = 0usize;
                let mut failures: Vec<String> = Vec::new();
                let mut occt_diag: Vec<f64> = vec![0.0; jobs.len()];
                for line in std::io::BufReader::new(stdout).split(b'\n') {
                    let Ok(line) = line else { break };
                    let line = String::from_utf8_lossy(&line);
                    let mut it = line.trim_end().splitn(3, ' ');
                    let (Some(tag), Some(k), rest) = (it.next(), it.next(), it.next()) else { continue };
                    let Ok(k) = k.parse::<usize>() else { continue };
                    if k >= jobs.len() {
                        continue;
                    }
                    match tag {
                        "BBOX" => {
                            occt_diag[k] = rest.and_then(|r| r.trim().parse::<f64>().ok()).unwrap_or(0.0);
                        }
                        "OK" => match read_stl(&stls[k]) {
                            Ok((mut v, i)) if !i.is_empty() => {
                                if let Some(f) = unit_scale(jobs[k].diag, occt_diag[k]) {
                                    for x in v.iter_mut() {
                                        x.pos = [x.pos[0] * f, x.pos[1] * f, x.pos[2] * f];
                                    }
                                    rescaled += 1;
                                }
                                ok += 1;
                                triangles += i.len() / 3;
                                send(LoadEvent::FullMesh { gen, def: jobs[k].def, vertices: v, indices: i });
                            }
                            Ok(_) => {
                                failed += 1;
                                failures.push(format!("{}: empty mesh", jobs[k].name));
                            }
                            Err(e) => {
                                failed += 1;
                                failures.push(format!("{}: {e}", jobs[k].name));
                            }
                        },
                        "FAIL" => {
                            failed += 1;
                            failures.push(format!("{}: {}", jobs[k].name, rest.unwrap_or("").trim()));
                        }
                        _ => {}
                    }
                }
                let status = child.wait()?;
                let stderr = stderr_thread.join().unwrap_or_default();
                if ok == 0 && failed == 0 {
                    anyhow::bail!(
                        "FreeCAD produced no result (exit {:?}). {}",
                        status.code(),
                        stderr.lines().filter(|l| !l.trim().is_empty()).rev().take(3).collect::<Vec<_>>().join(" | ")
                    );
                }
                let _ = std::fs::remove_dir_all(&dir);
                let mut msg = format!(
                    "{ok} part(s) loaded as exact B-rep, {} triangles, in {:.1} s",
                    triangles,
                    t0.elapsed().as_secs_f64()
                );
                if rescaled > 0 {
                    msg.push_str(&format!(" ({rescaled} rescaled from the kernel's millimetres)"));
                }
                if failed > 0 {
                    msg.push_str(&format!("; {failed} failed: {}", failures.join("; ")));
                }
                Ok((ok, failed, msg))
            })();
            match result {
                Ok((ok, failed, message)) => send(LoadEvent::FullLoadDone { gen, ok, failed, message }),
                Err(e) => send(LoadEvent::FullLoadDone { gen, ok: 0, failed: jobs.len(), message: format!("full load failed: {e:#}") }),
            }
        })
        .expect("spawn full-load thread");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_scale_detects_inches_and_leaves_matching_boxes_alone() {
        assert_eq!(unit_scale(100.0, 100.0), None);
        assert_eq!(unit_scale(100.0, 101.0), None);
        assert_eq!(unit_scale(10.0, 254.0), Some(1.0 / 25.4));
        assert_eq!(unit_scale(254.0, 10.0), Some(25.4));
        assert_eq!(unit_scale(1.0, 1000.0), Some(0.001));
        // spline parts over-estimate our box; no known factor, so no rescale
        assert_eq!(unit_scale(130.0, 100.0), None);
        assert_eq!(unit_scale(0.0, 100.0), None);
    }

    #[test]
    fn reads_ascii_and_binary_stl() {
        let dir = std::env::temp_dir().join(format!("steptrim-stl-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ascii = dir.join("a.stl");
        std::fs::write(
            &ascii,
            "solid t\n facet normal 0 0 0\n  outer loop\n   vertex 0 0 0\n   vertex 1 0 0\n   vertex 0 1 0\n  endloop\n endfacet\nendsolid t\n",
        )
        .unwrap();
        let (v, i) = read_stl(&ascii).unwrap();
        assert_eq!(i, vec![0, 1, 2]);
        assert_eq!(v[0].nrm, [0.0, 0.0, 1.0]);
        let mut bin = vec![0u8; 80];
        bin.extend_from_slice(&1u32.to_le_bytes());
        for f in [0.0f32, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0] {
            bin.extend_from_slice(&f.to_le_bytes());
        }
        bin.extend_from_slice(&[0, 0]);
        let binp = dir.join("b.stl");
        std::fs::write(&binp, &bin).unwrap();
        let (v, i) = read_stl(&binp).unwrap();
        assert_eq!(i, vec![0, 1, 2]);
        assert_eq!(v[2].pos, [0.0, 1.0, 0.0]);
        assert_eq!(v[2].nrm, [0.0, 0.0, 1.0]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
