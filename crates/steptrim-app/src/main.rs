//! StepTrim desktop app: open a STEP assembly, see its product tree and a
//! body preview while it is still loading, remove occurrences, and save a
//! trimmed STEP file.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod fullload;
mod loader;
mod viewer;

use eframe::egui;
use loader::{LoadEvent, Loaded};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use steptrim_core::geom::{Aabb, Transform};
use steptrim_core::sample::DefGeometry;
use viewer::{Camera, GeomDraw, InstanceRaw, InstanceUniform, MeshVertexRaw, Scene, SurfelRaw};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BodyMode {
    Splats,
    Mesh,
}

/// Command line: `steptrim [FILE] [--remove NAME]... [--select NAME]... [--save OUT]`.
/// The flags drive the editor without clicks, for tests and screenshots.
#[derive(Default, Clone)]
struct CliArgs {
    path: Option<PathBuf>,
    remove: Vec<String>,
    select: Vec<String>,
    save: Option<PathBuf>,
    fit: bool,
    /// Resample at this density once loaded (tests the resample path).
    samples: Option<f32>,
    /// Start in "mesh" or "splats" mode.
    mode: Option<String>,
    /// Hide every instance whose part name contains this text.
    hide: Vec<String>,
    /// Export each selected part as its own STEP file into this folder.
    export_dir: Option<PathBuf>,
    /// Export the selection as one STEP file keeping the assembly structure.
    export_one: Option<PathBuf>,
    /// Full-load (kernel tessellation) of the selection once loaded.
    full_load: bool,
    /// Isolate the selection (hide everything else) once loaded.
    isolate: bool,
}

fn parse_args() -> CliArgs {
    let mut out = CliArgs::default();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--remove" => {
                if let Some(v) = it.next() {
                    out.remove.push(v);
                }
            }
            "--select" => {
                if let Some(v) = it.next() {
                    out.select.push(v);
                }
            }
            "--save" => out.save = it.next().map(PathBuf::from),
            "--fit" => out.fit = true,
            "--samples" => out.samples = it.next().and_then(|v| v.parse().ok()),
            "--mode" => out.mode = it.next(),
            "--hide" => {
                if let Some(v) = it.next() {
                    out.hide.push(v);
                }
            }
            "--export-dir" => out.export_dir = it.next().map(PathBuf::from),
            "--export-one" => out.export_one = it.next().map(PathBuf::from),
            "--full-load" => out.full_load = true,
            "--isolate" => out.isolate = true,
            _ => out.path = Some(PathBuf::from(a)),
        }
    }
    out
}

fn main() -> eframe::Result {
    env_logger::init();
    let args = parse_args();
    // eframe asks for wgpu's default limits, which cap a single buffer at
    // 256 MiB; a dense sampling of a large assembly needs more, so request
    // whatever the adapter offers.
    let mut wgpu_options = eframe::egui_wgpu::WgpuConfiguration::default();
    if let eframe::egui_wgpu::WgpuSetup::CreateNew(setup) = &mut wgpu_options.wgpu_setup {
        setup.device_descriptor = Arc::new(|adapter: &eframe::wgpu::Adapter| {
            let caps = adapter.limits();
            let base = if adapter.get_info().backend == eframe::wgpu::Backend::Gl {
                eframe::wgpu::Limits::downlevel_webgl2_defaults()
            } else {
                eframe::wgpu::Limits::default()
            };
            eframe::wgpu::DeviceDescriptor {
                label: Some("steptrim device"),
                required_limits: eframe::wgpu::Limits {
                    max_texture_dimension_2d: 8192.min(caps.max_texture_dimension_2d),
                    max_buffer_size: caps.max_buffer_size,
                    ..base
                },
                ..Default::default()
            }
        });
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1440.0, 900.0]).with_title("StepTrim"),
        renderer: eframe::Renderer::Wgpu,
        depth_buffer: 32,
        wgpu_options,
        ..Default::default()
    };
    eframe::run_native("StepTrim", options, Box::new(move |cc| Ok(Box::new(App::new(cc, args)))))
}

const COLOR_SELECTED: [f32; 4] = [1.0, 0.55, 0.1, 1.0];
const COLOR_IN_SELECTION: [f32; 4] = [1.0, 0.76, 0.42, 1.0];
const COLOR_HOVER: [f32; 4] = [1.0, 0.92, 0.35, 1.0];

/// Where a definition's geometry sits in the scene's shared arrays.
#[derive(Clone, Copy, Debug)]
struct GeomRange {
    lines: (u32, u32),
    surfels: (u32, u32),
    /// (first index, index count, base vertex)
    mesh: (u32, u32, u32),
}

struct App {
    tx: Sender<LoadEvent>,
    rx: Receiver<LoadEvent>,
    generation: u64,
    cancel: Arc<AtomicBool>,
    loading: bool,
    saving: bool,
    stage: String,
    error: Option<String>,
    notice: Option<String>,
    file_label: String,

    loaded: Option<Arc<Loaded>>,
    def_aabb: Vec<Option<Aabb>>,
    bounds_done: usize,
    bounds_total: usize,
    bounds_ms: Option<f64>,
    def_geom: Vec<Option<GeomRange>>,
    geom_done: usize,
    geom_ms: Option<f64>,
    total_surfels: usize,
    total_lines: usize,
    total_triangles: usize,
    body_mode: BodyMode,
    /// Per definition: exact kernel mesh range in the scene arrays.
    def_full: Vec<Option<(u32, u32, u32)>>,
    /// Kept so a resample (which clears the scene arrays) can restore them.
    full_store: HashMap<u32, (Vec<MeshVertexRaw>, Vec<u32>)>,
    full_loading: bool,
    full_pending: usize,
    freecad: Option<PathBuf>,

    inst_children: Vec<Vec<u32>>,
    removed_occ: Vec<bool>,
    /// Per instance: hidden by the user (not removed; still saved).
    user_hidden: Vec<bool>,
    /// Per instance: not drawn, because removed or user-hidden, directly or
    /// through an ancestor.
    hidden: Vec<bool>,
    /// Per instance: hidden through removal only (for the tree's colours).
    removed_hidden: Vec<bool>,
    undo: Vec<Vec<u32>>,

    selection: HashSet<u32>,
    anchor: Option<u32>,
    hover_tree: Option<u32>,
    hover_box: Option<u32>,
    scene_hover: Option<u32>,
    last_row_order: Vec<u32>,
    row_order: Vec<u32>,
    scroll_to: Option<u32>,
    expanded_to: HashSet<u32>,

    scene: Arc<Mutex<Scene>>,
    scene_dirty: bool,
    scene_version: u64,
    visible_boxes: usize,
    visible_draws: usize,
    origin: glam::DVec3,
    camera: Camera,
    auto_fit: bool,
    filter: String,
    last_view_proj: glam::Mat4,
    last_rect: egui::Rect,
    show_boxes: bool,
    show_lines: bool,
    show_surfels: bool,

    size_panel: bool,
    size_min: f64,
    size_max: f64,

    /// Splat controls: live size multiplier, and sampling settings that
    /// take effect on Resample.
    splat_panel: bool,
    splat_scale: f32,
    samples_across: f32,
    max_surfels_per_part: u32,
    /// Total surfels the scene may hold (32 bytes each on the GPU).
    surfel_budget: u32,
    budget_skipped: usize,
    sample_run: u64,
    sample_cancel: Arc<AtomicBool>,
    sampling: bool,

    scripted: Option<CliArgs>,
    scripted_pending: bool,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, args: CliArgs) -> Self {
        let path = args.path.clone();
        let body_mode = match args.mode.as_deref() {
            Some("mesh") => BodyMode::Mesh,
            _ => BodyMode::Splats,
        };
        let scripted = if args.remove.is_empty()
            && args.select.is_empty()
            && args.save.is_none()
            && !args.fit
            && args.samples.is_none()
            && args.hide.is_empty()
            && args.export_dir.is_none()
            && args.export_one.is_none()
            && !args.full_load
            && !args.isolate
        {
            None
        } else {
            Some(args)
        };
        let rs = cc.wgpu_render_state.as_ref().expect("StepTrim needs the wgpu renderer");
        viewer::init(rs, Some(eframe::wgpu::TextureFormat::Depth32Float));
        let (tx, rx) = channel();
        let mut app = Self {
            tx,
            rx,
            generation: 0,
            cancel: Arc::new(AtomicBool::new(false)),
            loading: false,
            saving: false,
            stage: String::new(),
            error: None,
            notice: None,
            file_label: "No file".into(),
            loaded: None,
            def_aabb: Vec::new(),
            bounds_done: 0,
            bounds_total: 0,
            bounds_ms: None,
            def_geom: Vec::new(),
            geom_done: 0,
            geom_ms: None,
            total_surfels: 0,
            total_lines: 0,
            total_triangles: 0,
            body_mode,
            def_full: Vec::new(),
            full_store: HashMap::new(),
            full_loading: false,
            full_pending: 0,
            freecad: fullload::find_freecad(),
            inst_children: Vec::new(),
            removed_occ: Vec::new(),
            user_hidden: Vec::new(),
            hidden: Vec::new(),
            removed_hidden: Vec::new(),
            undo: Vec::new(),
            selection: HashSet::new(),
            anchor: None,
            hover_tree: None,
            hover_box: None,
            scene_hover: None,
            last_row_order: Vec::new(),
            row_order: Vec::new(),
            scroll_to: None,
            expanded_to: HashSet::new(),
            scene: Arc::new(Mutex::new(Scene::empty())),
            scene_dirty: false,
            scene_version: 0,
            visible_boxes: 0,
            visible_draws: 0,
            origin: glam::DVec3::ZERO,
            camera: Camera::default(),
            auto_fit: true,
            filter: String::new(),
            last_view_proj: glam::Mat4::IDENTITY,
            last_rect: egui::Rect::NOTHING,
            show_boxes: false,
            show_lines: true,
            show_surfels: true,
            size_panel: false,
            size_min: 0.0,
            size_max: 0.0,
            splat_panel: false,
            splat_scale: 1.0,
            samples_across: 56.0,
            max_surfels_per_part: 150_000,
            surfel_budget: 12_000_000,
            budget_skipped: 0,
            sample_run: 0,
            sample_cancel: Arc::new(AtomicBool::new(false)),
            sampling: false,
            scripted,
            scripted_pending: false,
        };
        if let Some(p) = path {
            app.open(p, &cc.egui_ctx);
        }
        app
    }

    fn run_scripted(&mut self, ctx: &egui::Context) {
        let Some(args) = self.scripted.take() else { return };
        let Some(loaded) = self.loaded.clone() else { return };
        for needle in &args.remove {
            let n = needle.to_lowercase();
            let defs: Vec<u32> = (0..loaded.tree.definitions.len() as u32)
                .filter(|&d| loaded.tree.definition_name(d).to_lowercase().contains(&n))
                .collect();
            for d in defs {
                self.remove_all_of_definition(d);
            }
        }
        for needle in &args.hide {
            let n = needle.to_lowercase();
            for (i, inst) in loaded.instances.iter().enumerate() {
                if loaded.tree.definition_name(inst.definition).to_lowercase().contains(&n) {
                    self.user_hidden[i] = true;
                }
            }
            self.recompute_hidden();
        }
        for needle in &args.select {
            let n = needle.to_lowercase();
            let mut first = None;
            for (i, inst) in loaded.instances.iter().enumerate() {
                if loaded.tree.definition_name(inst.definition).to_lowercase().contains(&n) {
                    self.selection.insert(i as u32);
                    first.get_or_insert(i as u32);
                }
            }
            if let Some(f) = first {
                self.anchor = Some(f);
                self.reveal(f);
            }
            self.scene_dirty = true;
        }
        if args.isolate {
            self.isolate_selected();
        }
        if let Some(dir) = &args.export_dir {
            self.export_selection_separately(dir.clone(), ctx);
        }
        if let Some(out) = &args.export_one {
            self.export_selection_as_one(out.clone(), ctx);
        }
        if args.full_load {
            self.full_load_selection(ctx);
        }
        if args.fit {
            self.fit_to_selection();
        }
        if let Some(n) = args.samples {
            self.samples_across = n;
            self.resample(ctx);
        }
        if let Some(out) = args.save {
            let remove: Vec<u32> = self.removed_occ.iter().enumerate().filter(|(_, &r)| r).map(|(i, _)| i as u32).collect();
            if !remove.is_empty() {
                self.saving = true;
                self.notice = Some(format!("Saving {}…", out.display()));
                loader::spawn_save(self.generation, loaded, remove, out, self.tx.clone(), ctx.clone());
            }
        }
    }

    fn open(&mut self, path: PathBuf, ctx: &egui::Context) {
        self.cancel.store(true, Ordering::Relaxed);
        self.cancel = Arc::new(AtomicBool::new(false));
        self.generation += 1;
        self.loading = true;
        self.saving = false;
        self.error = None;
        self.notice = None;
        self.stage = "opening".into();
        self.file_label = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        self.loaded = None;
        self.def_aabb.clear();
        self.bounds_done = 0;
        self.bounds_total = 0;
        self.bounds_ms = None;
        self.def_geom.clear();
        self.geom_done = 0;
        self.geom_ms = None;
        self.total_surfels = 0;
        self.total_lines = 0;
        self.total_triangles = 0;
        self.def_full.clear();
        self.full_store.clear();
        self.full_loading = false;
        self.full_pending = 0;
        self.inst_children.clear();
        self.removed_occ.clear();
        self.user_hidden.clear();
        self.hidden.clear();
        self.removed_hidden.clear();
        self.undo.clear();
        self.selection.clear();
        self.anchor = None;
        self.hover_tree = None;
        self.hover_box = None;
        self.scene_hover = None;
        self.last_row_order.clear();
        self.scroll_to = None;
        self.expanded_to.clear();
        self.auto_fit = true;
        self.sample_cancel.store(true, Ordering::Relaxed);
        self.sample_cancel = self.cancel.clone();
        self.sample_run = 0;
        self.sampling = true;
        {
            let mut s = self.scene.lock().unwrap();
            *s = Scene::empty();
            self.scene_version += 1;
            s.box_version = self.scene_version;
            s.draw_version = self.scene_version;
            s.geom_version = self.scene_version;
        }
        self.scene_dirty = true;
        loader::spawn(self.generation, path, self.tx.clone(), ctx.clone(), self.cancel.clone());
    }

    fn drain_events(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                LoadEvent::Started { gen, path, size_bytes } if gen == self.generation => {
                    self.file_label = format!(
                        "{}  ({:.1} MB)",
                        path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
                        size_bytes as f64 / 1_048_576.0
                    );
                }
                LoadEvent::Stage { gen, stage } if gen == self.generation => self.stage = stage.to_string(),
                LoadEvent::Loaded { gen, loaded } if gen == self.generation => {
                    let n = loaded.instances.len();
                    let nd = loaded.tree.definitions.len();
                    self.def_aabb = vec![None; nd];
                    self.def_geom = vec![None; nd];
                    self.def_full = vec![None; nd];
                    self.bounds_total = loaded.tree.definitions.iter().filter(|d| d.bodies > 0).count();
                    self.inst_children = vec![Vec::new(); n];
                    for (i, inst) in loaded.instances.iter().enumerate() {
                        if let Some(p) = inst.parent {
                            self.inst_children[p as usize].push(i as u32);
                        }
                    }
                    self.removed_occ = vec![false; loaded.tree.occurrences.len()];
                    self.user_hidden = vec![false; n];
                    self.hidden = vec![false; n];
                    self.removed_hidden = vec![false; n];
                    self.loaded = Some(loaded);
                    self.scene_dirty = true;
                }
                LoadEvent::Bounds { gen, def, bounds } if gen == self.generation => {
                    if let Some(slot) = self.def_aabb.get_mut(def as usize) {
                        if !bounds.aabb.is_empty() {
                            *slot = Some(bounds.aabb);
                        }
                        self.bounds_done += 1;
                        self.scene_dirty = true;
                    }
                }
                LoadEvent::BoundsDone { gen, ms } if gen == self.generation => {
                    self.bounds_ms = Some(ms);
                    self.stage = "sampling bodies".into();
                    self.scene_dirty = true;
                    self.scripted_pending = true;
                    if self.size_max == 0.0 {
                        self.init_size_range();
                    }
                }
                LoadEvent::Geometry { gen, run, def, geom } if gen == self.generation && run == self.sample_run => {
                    self.append_geometry(def, &geom);
                }
                LoadEvent::GeometryDone { gen, run, ms } if gen == self.generation && run == self.sample_run => {
                    self.geom_ms = Some(ms);
                    self.loading = false;
                    self.sampling = false;
                    self.stage = "ready".into();
                    self.scene_dirty = true;
                }
                LoadEvent::Error { gen, message } if gen == self.generation => {
                    self.error = Some(message);
                    self.loading = false;
                    self.stage = "failed".into();
                }
                LoadEvent::Saved { gen, path, summary } if gen == self.generation => {
                    self.saving = false;
                    self.notice = Some(format!("Saved {}: {}", path.display(), summary));
                }
                LoadEvent::SaveFailed { gen, message } if gen == self.generation => {
                    self.saving = false;
                    self.error = Some(format!("Save failed: {message}"));
                }
                LoadEvent::FullMesh { gen, def, vertices, indices } if gen == self.generation => {
                    log::info!("full mesh for definition {def}: {} triangles", indices.len() / 3);
                    let v: Vec<MeshVertexRaw> = vertices.iter().map(|x| MeshVertexRaw { pos: x.pos, nrm: x.nrm }).collect();
                    self.full_store.insert(def, (v, indices));
                    self.append_full_mesh(def);
                    self.full_pending = self.full_pending.saturating_sub(1);
                }
                LoadEvent::FullLoadDone { gen, ok, failed, message } if gen == self.generation => {
                    self.full_loading = false;
                    self.full_pending = 0;
                    self.stage = "ready".into();
                    log::info!("full load done: ok={ok} failed={failed}: {message}");
                    if failed > 0 && ok == 0 {
                        self.error = Some(message);
                    } else {
                        self.notice = Some(message);
                    }
                }
                _ => {}
            }
        }
    }

    /// Put a stored exact mesh into the scene arrays and point the
    /// definition at it.
    fn append_full_mesh(&mut self, def: u32) {
        let Some((v, i)) = self.full_store.get(&def) else { return };
        if (def as usize) >= self.def_full.len() {
            return;
        }
        let mut s = self.scene.lock().unwrap();
        let vbase = s.mesh_vertices.len() as u32;
        s.mesh_vertices.extend_from_slice(v);
        let i0 = s.mesh_indices.len() as u32;
        s.mesh_indices.extend_from_slice(i);
        self.scene_version += 1;
        s.geom_version = self.scene_version;
        drop(s);
        self.def_full[def as usize] = Some((i0, i.len() as u32, vbase));
        self.scene_dirty = true;
    }

    /// Leaf part definitions under the selection, deduplicated.
    fn selected_part_definitions(&self) -> Vec<u32> {
        let Some(loaded) = &self.loaded else { return Vec::new() };
        let mask = self.selection_mask();
        let mut defs: Vec<u32> = Vec::new();
        for (i, inst) in loaded.instances.iter().enumerate() {
            if !mask[i] || self.hidden[i] {
                continue;
            }
            let d = inst.definition;
            if loaded.tree.definitions[d as usize].bodies > 0 && !defs.contains(&d) {
                defs.push(d);
            }
        }
        defs
    }

    /// Tessellate the selected parts exactly with the kernel worker.
    fn full_load_selection(&mut self, ctx: &egui::Context) {
        let Some(loaded) = self.loaded.clone() else { return };
        let Some(freecad) = self.freecad.clone() else {
            self.error = Some("Full load needs FreeCAD (FreeCADCmd.exe) for its Open CASCADE kernel; install FreeCAD 1.0 or set STEPTRIM_FREECAD to the executable.".into());
            return;
        };
        let defs: Vec<u32> = self.selected_part_definitions().into_iter().filter(|d| self.def_full[*d as usize].is_none()).collect();
        if defs.is_empty() {
            self.notice = Some("Select parts that are not fully loaded yet.".into());
            return;
        }
        let jobs: Vec<fullload::FullLoadJob> = defs
            .iter()
            .map(|&d| {
                let diag = self.def_aabb[d as usize]
                    .map(|b| {
                        let s = b.size();
                        (s[0] * s[0] + s[1] * s[1] + s[2] * s[2]).sqrt()
                    })
                    .unwrap_or(100.0);
                fullload::FullLoadJob { def: d, name: loaded.tree.definition_name(d).to_string(), diag }
            })
            .collect();
        self.full_loading = true;
        self.full_pending = jobs.len();
        self.error = None;
        self.notice = Some(format!("Full load: tessellating {} part(s) with Open CASCADE…", jobs.len()));
        fullload::spawn_full_load(self.generation, loaded, jobs, freecad, self.tx.clone(), ctx.clone());
    }

    fn clear_full_loads(&mut self) {
        self.full_store.clear();
        for f in self.def_full.iter_mut() {
            *f = None;
        }
        self.scene_dirty = true;
    }

    /// Append a definition's lines and surfels to the shared scene arrays,
    /// within the GPU budget: a part that would overflow it keeps its box.
    fn append_geometry(&mut self, def: u32, g: &DefGeometry) {
        if (def as usize) >= self.def_geom.len() {
            return;
        }
        // Budget counts surfels and mesh vertices alike (similar bytes each).
        let budget = self.surfel_budget as usize;
        if self.total_surfels + g.surfels.len() + g.mesh_vertices.len() > budget {
            self.budget_skipped += 1;
            self.geom_done += 1;
            self.notice = Some(format!(
                "GPU budget of {} surfels reached: {} parts stay as boxes. Lower the density or raise the budget in Splats…",
                fmt(budget as u64),
                self.budget_skipped
            ));
            return;
        }
        let mut s = self.scene.lock().unwrap();
        let l0 = s.lines.len() as u32;
        s.lines.extend_from_slice(&g.lines);
        let s0 = s.surfels.len() as u32;
        s.surfels.extend(g.surfels.iter().map(|x| SurfelRaw { pos: x.pos, radius: x.radius, nrm: x.nrm, _pad: 0.0 }));
        let vbase = s.mesh_vertices.len() as u32;
        s.mesh_vertices.extend(g.mesh_vertices.iter().map(|v| MeshVertexRaw { pos: v.pos, nrm: v.nrm }));
        let i0 = s.mesh_indices.len() as u32;
        s.mesh_indices.extend_from_slice(&g.mesh_indices);
        self.scene_version += 1;
        s.geom_version = self.scene_version;
        drop(s);
        self.def_geom[def as usize] = Some(GeomRange {
            lines: (l0, g.lines.len() as u32),
            surfels: (s0, g.surfels.len() as u32),
            mesh: (i0, g.mesh_indices.len() as u32, vbase),
        });
        self.geom_done += 1;
        self.total_lines += g.lines.len() / 2;
        self.total_surfels += g.surfels.len() + g.mesh_vertices.len();
        self.total_triangles += g.mesh_indices.len() / 3;
        self.scene_dirty = true;
    }

    /// Throw away the sampled bodies and sample again with the current
    /// density settings; boxes stand in while the new samples stream in.
    fn resample(&mut self, ctx: &egui::Context) {
        let Some(loaded) = self.loaded.clone() else { return };
        self.sample_cancel.store(true, Ordering::Relaxed);
        self.sample_cancel = Arc::new(AtomicBool::new(false));
        self.sample_run += 1;
        self.sampling = true;
        for g in self.def_geom.iter_mut() {
            *g = None;
        }
        self.geom_done = 0;
        self.geom_ms = None;
        self.total_lines = 0;
        self.total_surfels = 0;
        self.total_triangles = 0;
        self.budget_skipped = 0;
        {
            let mut s = self.scene.lock().unwrap();
            s.lines.clear();
            s.surfels.clear();
            s.mesh_vertices.clear();
            s.mesh_indices.clear();
            s.draws.clear();
            self.scene_version += 1;
            s.geom_version = self.scene_version;
            s.draw_version = self.scene_version;
        }
        // Exact meshes live in the same arrays; put them back.
        for f in self.def_full.iter_mut() {
            *f = None;
        }
        let keys: Vec<u32> = self.full_store.keys().copied().collect();
        for d in keys {
            self.append_full_mesh(d);
        }
        self.scene_dirty = true;
        let bounds: Vec<Option<steptrim_core::geom::DefBounds>> =
            self.def_aabb.iter().map(|b| b.map(|aabb| steptrim_core::geom::DefBounds { aabb, points: 0, entities: 0 })).collect();
        let cfg = steptrim_core::sample::SampleConfig {
            samples_across: self.samples_across.max(4.0) as f64,
            max_surfels_per_def: self.max_surfels_per_part.max(1000) as usize,
            max_surfels_per_face: (self.max_surfels_per_part / 20).clamp(500, 20_000) as usize,
            // B-spline faces are sampled on a fixed grid; scale it with the density too.
            spline_surface_grid: ((self.samples_across / 4.0) as usize).clamp(6, 60),
            spline_curve_samples: ((self.samples_across / 3.5) as usize).clamp(8, 64),
            ..Default::default()
        };
        loader::spawn_resample(self.generation, self.sample_run, loaded, bounds, cfg, self.tx.clone(), ctx.clone(), self.sample_cancel.clone());
    }

    // ----- removal -----

    fn recompute_hidden(&mut self) {
        let Some(loaded) = &self.loaded else { return };
        for (i, inst) in loaded.instances.iter().enumerate() {
            let removed = inst.occurrence.map(|o| self.removed_occ[o as usize]).unwrap_or(false);
            let parent_removed = inst.parent.map(|p| self.removed_hidden[p as usize]).unwrap_or(false);
            self.removed_hidden[i] = removed || parent_removed;
            let parent_hidden = inst.parent.map(|p| self.hidden[p as usize]).unwrap_or(false);
            self.hidden[i] = self.removed_hidden[i] || self.user_hidden[i] || parent_hidden;
        }
        self.scene_dirty = true;
    }

    // ----- visibility -----

    fn hide_instances(&mut self, insts: &[u32]) {
        for &i in insts {
            self.user_hidden[i as usize] = true;
        }
        self.recompute_hidden();
    }

    fn hide_selected(&mut self) {
        let sel: Vec<u32> = self.selection.iter().copied().collect();
        self.hide_instances(&sel);
    }

    /// Hide everything that is not in the selection (or under it).
    fn isolate_selected(&mut self) {
        let Some(loaded) = self.loaded.clone() else { return };
        if self.selection.is_empty() {
            return;
        }
        let mask = self.selection_mask();
        // ancestors of selected instances must stay visible for their subtrees to show
        let mut keep = mask.clone();
        for &s in &self.selection {
            let mut p = loaded.instances[s as usize].parent;
            while let Some(a) = p {
                keep[a as usize] = true;
                p = loaded.instances[a as usize].parent;
            }
        }
        for i in 0..loaded.instances.len() {
            self.user_hidden[i] = !keep[i];
        }
        self.recompute_hidden();
    }

    /// Make an instance visible again, including the ancestors that hide it.
    fn show_instances(&mut self, insts: &[u32]) {
        let Some(loaded) = self.loaded.clone() else { return };
        for &i in insts {
            self.user_hidden[i as usize] = false;
            let mut p = loaded.instances[i as usize].parent;
            while let Some(a) = p {
                self.user_hidden[a as usize] = false;
                p = loaded.instances[a as usize].parent;
            }
        }
        self.recompute_hidden();
    }

    fn show_selected(&mut self) {
        let sel: Vec<u32> = self.selection.iter().copied().collect();
        self.show_instances(&sel);
    }

    fn show_all(&mut self) {
        for h in self.user_hidden.iter_mut() {
            *h = false;
        }
        self.recompute_hidden();
    }

    fn user_hidden_count(&self) -> usize {
        self.user_hidden.iter().filter(|&&h| h).count()
    }

    /// Select every visible part that is not selected now, and drop the rest.
    fn invert_selection(&mut self) {
        let Some(loaded) = self.loaded.clone() else { return };
        let mask = self.selection_mask();
        let mut next = HashSet::new();
        for (i, inst) in loaded.instances.iter().enumerate() {
            if self.hidden[i] || loaded.tree.definitions[inst.definition as usize].bodies == 0 {
                continue;
            }
            if !mask[i] {
                next.insert(i as u32);
            }
        }
        self.selection = next;
        self.scene_dirty = true;
    }

    // ----- export -----

    /// Occurrences to remove so that only the selection (with its subtrees
    /// and the path down to it) survives.
    fn keep_selection_removals(&self) -> Vec<u32> {
        let Some(loaded) = &self.loaded else { return Vec::new() };
        let mask = self.selection_mask();
        let mut keep_inst = mask.clone();
        for &s in &self.selection {
            let mut p = loaded.instances[s as usize].parent;
            while let Some(a) = p {
                keep_inst[a as usize] = true;
                p = loaded.instances[a as usize].parent;
            }
        }
        let mut keep_occ = vec![false; loaded.tree.occurrences.len()];
        for (i, inst) in loaded.instances.iter().enumerate() {
            if keep_inst[i] {
                if let Some(o) = inst.occurrence {
                    keep_occ[o as usize] = true;
                }
            }
        }
        (0..keep_occ.len() as u32).filter(|&o| !keep_occ[o as usize]).collect()
    }

    fn export_selection_as_one(&mut self, out: PathBuf, ctx: &egui::Context) {
        let Some(loaded) = self.loaded.clone() else { return };
        if self.selection.is_empty() {
            self.notice = Some("Select something to export first.".into());
            return;
        }
        let remove = self.keep_selection_removals();
        self.saving = true;
        self.error = None;
        self.notice = Some(format!("Exporting selection to {}…", out.display()));
        loader::spawn_save_many(self.generation, loaded, vec![(out, steptrim_core::prune::PruneRequest { remove_occurrences: remove, ..Default::default() })], self.tx.clone(), ctx.clone());
    }

    /// One standalone file per selected part (one per definition).
    fn export_selection_separately(&mut self, dir: PathBuf, ctx: &egui::Context) {
        let Some(loaded) = self.loaded.clone() else { return };
        let mut defs: Vec<u32> = Vec::new();
        for &s in &self.selection {
            let d = loaded.instances[s as usize].definition;
            if !defs.contains(&d) {
                defs.push(d);
            }
        }
        if defs.is_empty() {
            self.notice = Some("Select parts to export first.".into());
            return;
        }
        let stem = loaded.path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or("part".into());
        let mut used: HashSet<String> = HashSet::new();
        let mut jobs = Vec::new();
        for d in defs {
            let raw = loaded.tree.definition_name(d);
            let mut name: String = raw.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '_' }).collect();
            name.truncate(60);
            if name.is_empty() {
                name = format!("part{}", loaded.tree.definitions[d as usize].entity);
            }
            let mut file = format!("{stem}-{name}.stp");
            let mut k = 2;
            while !used.insert(file.clone()) {
                file = format!("{stem}-{name}-{k}.stp");
                k += 1;
            }
            jobs.push((dir.join(file), steptrim_core::prune::standalone_request(&loaded.tree, d)));
        }
        self.saving = true;
        self.error = None;
        self.notice = Some(format!("Exporting {} part file(s) into {}…", jobs.len(), dir.display()));
        loader::spawn_save_many(self.generation, loaded, jobs, self.tx.clone(), ctx.clone());
    }

    fn export_one_dialog(&mut self, ctx: &egui::Context) {
        let Some(loaded) = self.loaded.clone() else { return };
        let stem = loaded.path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or("selection".into());
        let dialog = rfd::FileDialog::new().add_filter("STEP", &["stp", "step"]).set_file_name(format!("{stem}-selection.stp"));
        let dialog = match loaded.path.parent() {
            Some(dir) => dialog.set_directory(dir),
            None => dialog,
        };
        if let Some(out) = dialog.save_file() {
            self.export_selection_as_one(out, ctx);
        }
    }

    fn export_separate_dialog(&mut self, ctx: &egui::Context) {
        let Some(loaded) = self.loaded.clone() else { return };
        let dialog = rfd::FileDialog::new().set_title("Folder for the exported part files");
        let dialog = match loaded.path.parent() {
            Some(dir) => dialog.set_directory(dir),
            None => dialog,
        };
        if let Some(dir) = dialog.pick_folder() {
            self.export_selection_separately(dir, ctx);
        }
    }

    fn remove_occurrences(&mut self, occs: Vec<u32>) {
        let step: Vec<u32> = occs.into_iter().filter(|&o| !self.removed_occ[o as usize]).collect();
        if step.is_empty() {
            return;
        }
        for &o in &step {
            self.removed_occ[o as usize] = true;
        }
        self.undo.push(step);
        self.recompute_hidden();
    }

    fn remove_instances(&mut self, insts: &[u32]) {
        let Some(loaded) = &self.loaded else { return };
        let mut occs = Vec::new();
        let mut root_hit = false;
        for &i in insts {
            match loaded.instances[i as usize].occurrence {
                Some(o) => occs.push(o),
                None => root_hit = true,
            }
        }
        if root_hit {
            self.notice = Some("The root assembly cannot be removed.".into());
        }
        self.remove_occurrences(occs);
    }

    fn remove_selected(&mut self) {
        let sel: Vec<u32> = self.selection.iter().copied().collect();
        self.remove_instances(&sel);
    }

    fn remove_all_of_definition(&mut self, def: u32) {
        let Some(loaded) = &self.loaded else { return };
        let occs: Vec<u32> = loaded.tree.occurrences.iter().enumerate().filter(|(_, o)| o.child == def).map(|(i, _)| i as u32).collect();
        self.remove_occurrences(occs);
    }

    fn restore_instance(&mut self, inst: u32) {
        let Some(loaded) = &self.loaded else { return };
        if let Some(o) = loaded.instances[inst as usize].occurrence {
            if self.removed_occ[o as usize] {
                self.removed_occ[o as usize] = false;
                for step in self.undo.iter_mut() {
                    step.retain(|&x| x != o);
                }
                self.undo.retain(|s| !s.is_empty());
                self.recompute_hidden();
            }
        }
    }

    fn undo_last(&mut self) {
        if let Some(step) = self.undo.pop() {
            for o in step {
                self.removed_occ[o as usize] = false;
            }
            self.recompute_hidden();
        }
    }

    fn removed_count(&self) -> usize {
        self.removed_occ.iter().filter(|&&r| r).count()
    }

    // ----- selection -----

    fn click_select(&mut self, inst: u32, mods: egui::Modifiers) {
        if mods.shift {
            if let Some(a) = self.anchor {
                let order = &self.last_row_order;
                let ia = order.iter().position(|&x| x == a);
                let ib = order.iter().position(|&x| x == inst);
                if let (Some(ia), Some(ib)) = (ia, ib) {
                    let (lo, hi) = if ia <= ib { (ia, ib) } else { (ib, ia) };
                    if !mods.ctrl {
                        self.selection.clear();
                    }
                    for &x in &order[lo..=hi] {
                        self.selection.insert(x);
                    }
                } else {
                    self.selection.insert(inst);
                }
            } else {
                self.selection.insert(inst);
                self.anchor = Some(inst);
            }
        } else if mods.ctrl {
            if !self.selection.remove(&inst) {
                self.selection.insert(inst);
            }
            self.anchor = Some(inst);
        } else {
            self.selection.clear();
            self.selection.insert(inst);
            self.anchor = Some(inst);
        }
        self.reveal(inst);
        self.scene_dirty = true;
    }

    fn clear_selection(&mut self) {
        if !self.selection.is_empty() {
            self.selection.clear();
            self.scene_dirty = true;
        }
    }

    fn reveal(&mut self, inst: u32) {
        if let Some(loaded) = &self.loaded {
            let mut p = loaded.instances[inst as usize].parent;
            while let Some(a) = p {
                self.expanded_to.insert(a);
                p = loaded.instances[a as usize].parent;
            }
        }
        self.scroll_to = Some(inst);
    }

    fn selection_mask(&self) -> Vec<bool> {
        let Some(loaded) = &self.loaded else { return Vec::new() };
        let mut m = vec![false; loaded.instances.len()];
        for (i, inst) in loaded.instances.iter().enumerate() {
            m[i] = self.selection.contains(&(i as u32)) || inst.parent.map(|p| m[p as usize]).unwrap_or(false);
        }
        m
    }

    /// Box diagonal of an instance's part, in file units.
    fn instance_size(&self, inst: u32) -> Option<f64> {
        let loaded = self.loaded.as_ref()?;
        let b = self.def_aabb[loaded.instances[inst as usize].definition as usize]?;
        let s = b.size();
        Some((s[0] * s[0] + s[1] * s[1] + s[2] * s[2]).sqrt())
    }

    fn init_size_range(&mut self) {
        let Some(loaded) = &self.loaded else { return };
        let mut max = 0.0f64;
        for (i, _) in loaded.instances.iter().enumerate() {
            if let Some(s) = self.instance_size(i as u32) {
                max = max.max(s);
            }
        }
        self.size_min = 0.0;
        self.size_max = (max * 0.05).max(1.0);
    }

    /// Visible part instances whose box diagonal lies in [size_min, size_max].
    fn instances_in_size_range(&self) -> Vec<u32> {
        let Some(loaded) = &self.loaded else { return Vec::new() };
        let mut out = Vec::new();
        for (i, inst) in loaded.instances.iter().enumerate() {
            if self.hidden[i] || loaded.tree.definitions[inst.definition as usize].bodies == 0 {
                continue;
            }
            if let Some(s) = self.instance_size(i as u32) {
                if s >= self.size_min && s <= self.size_max {
                    out.push(i as u32);
                }
            }
        }
        out
    }

    // ----- scene -----

    fn rebuild_scene(&mut self) {
        self.scene_dirty = false;
        let hover = self.hover_tree.or(self.hover_box);
        self.scene_hover = hover;
        let Some(loaded) = self.loaded.clone() else {
            let mut s = self.scene.lock().unwrap();
            s.boxes.clear();
            s.draws.clear();
            s.instance_uniforms.clear();
            self.scene_version += 1;
            s.box_version = self.scene_version;
            s.draw_version = self.scene_version;
            return;
        };
        let mut world = Aabb::EMPTY;
        let mut visible: Vec<(u32, Transform, Option<Aabb>)> = Vec::with_capacity(loaded.instances.len());
        for (i, inst) in loaded.instances.iter().enumerate() {
            if self.hidden[i] || loaded.tree.definitions[inst.definition as usize].bodies == 0 {
                continue;
            }
            let local = self.def_aabb[inst.definition as usize];
            if let Some(b) = local {
                world.union(&b.transformed(&inst.world));
            }
            if local.is_some() || self.def_geom[inst.definition as usize].is_some() {
                visible.push((i as u32, inst.world, local));
            }
        }
        if self.auto_fit && !world.is_empty() {
            let c = world.center();
            self.origin = glam::DVec3::new(c[0], c[1], c[2]);
            let s = world.size();
            let radius = 0.5 * (s[0] * s[0] + s[1] * s[1] + s[2] * s[2]).sqrt();
            self.camera.fit(glam::DVec3::ZERO, radius);
        }
        let origin = self.origin;
        let in_sel = self.selection_mask();
        let hover_mask: Vec<bool> = match hover {
            Some(h) => {
                let mut m = vec![false; loaded.instances.len()];
                for (i, inst) in loaded.instances.iter().enumerate() {
                    m[i] = i as u32 == h || inst.parent.map(|p| m[p as usize]).unwrap_or(false);
                }
                m
            }
            None => Vec::new(),
        };
        let mut boxes = Vec::new();
        let mut uniforms = Vec::new();
        let mut draws = Vec::new();
        for (i, w, local) in visible {
            let def = loaded.instances[i as usize].definition;
            let color = if self.selection.contains(&i) {
                COLOR_SELECTED
            } else if in_sel[i as usize] {
                COLOR_IN_SELECTION
            } else if hover_mask.get(i as usize).copied().unwrap_or(false) {
                COLOR_HOVER
            } else {
                def_color(def)
            };
            let m = w.m;
            let geom = self.def_geom[def as usize];
            let full = self.def_full[def as usize];
            let has_geom = geom.map(|g| g.lines.1 > 0 || g.surfels.1 > 0 || g.mesh.1 > 0).unwrap_or(false) || full.is_some();
            if let Some(b) = local {
                if self.show_boxes || !has_geom {
                    let c = b.center();
                    let h = b.size();
                    let half = [h[0] * 0.5, h[1] * 0.5, h[2] * 0.5];
                    let t = w.apply(c);
                    let model = [
                        (m[0][0] * half[0]) as f32, (m[1][0] * half[0]) as f32, (m[2][0] * half[0]) as f32, 0.0,
                        (m[0][1] * half[1]) as f32, (m[1][1] * half[1]) as f32, (m[2][1] * half[1]) as f32, 0.0,
                        (m[0][2] * half[2]) as f32, (m[1][2] * half[2]) as f32, (m[2][2] * half[2]) as f32, 0.0,
                        (t[0] - origin.x) as f32, (t[1] - origin.y) as f32, (t[2] - origin.z) as f32, 1.0,
                    ];
                    boxes.push(InstanceRaw { model, color });
                }
            }
            if has_geom && (self.show_lines || self.show_surfels || self.body_mode == BodyMode::Mesh || full.is_some()) {
                let model = [
                    m[0][0] as f32, m[1][0] as f32, m[2][0] as f32, 0.0,
                    m[0][1] as f32, m[1][1] as f32, m[2][1] as f32, 0.0,
                    m[0][2] as f32, m[1][2] as f32, m[2][2] as f32, 0.0,
                    (w.t[0] - origin.x) as f32, (w.t[1] - origin.y) as f32, (w.t[2] - origin.z) as f32, 1.0,
                ];
                let idx = uniforms.len() as u32;
                uniforms.push(InstanceUniform { model, color });
                let lines = geom.map(|g| g.lines).unwrap_or((0, 0));
                match full {
                    // Exact kernel mesh: drawn in every body mode, sampled surfels and grid mesh suppressed.
                    Some(fm) => draws.push(GeomDraw { instance: idx, lines, surfels: (0, 0), mesh: fm, full: true }),
                    None => {
                        let g = geom.unwrap_or(GeomRange { lines: (0, 0), surfels: (0, 0), mesh: (0, 0, 0) });
                        draws.push(GeomDraw { instance: idx, lines: g.lines, surfels: g.surfels, mesh: g.mesh, full: false });
                    }
                }
            }
        }
        self.visible_boxes = boxes.len();
        self.visible_draws = draws.len();
        let mut s = self.scene.lock().unwrap();
        s.boxes = boxes;
        s.instance_uniforms = uniforms;
        s.draws = draws;
        s.show_lines = self.show_lines;
        s.show_surfels = self.show_surfels && self.body_mode == BodyMode::Splats;
        s.show_mesh = self.body_mode == BodyMode::Mesh;
        self.scene_version += 1;
        s.box_version = self.scene_version;
        s.draw_version = self.scene_version;
    }

    fn fit_view(&mut self) {
        self.auto_fit = true;
        self.scene_dirty = true;
    }

    fn subtree_bounds(&self, roots: &[u32]) -> Aabb {
        let Some(loaded) = &self.loaded else { return Aabb::EMPTY };
        let mut b = Aabb::EMPTY;
        let mut stack: Vec<u32> = roots.to_vec();
        while let Some(i) = stack.pop() {
            let it = &loaded.instances[i as usize];
            if self.hidden[i as usize] {
                continue;
            }
            if let Some(local) = self.def_aabb[it.definition as usize] {
                b.union(&local.transformed(&it.world));
            }
            stack.extend_from_slice(&self.inst_children[i as usize]);
        }
        b
    }

    fn fit_to(&mut self, roots: &[u32]) {
        let b = self.subtree_bounds(roots);
        if !b.is_empty() {
            let c = b.center();
            let s = b.size();
            let radius = 0.5 * (s[0] * s[0] + s[1] * s[1] + s[2] * s[2]).sqrt();
            self.auto_fit = false;
            self.camera.fit(glam::DVec3::new(c[0], c[1], c[2]) - self.origin, radius.max(1.0));
            self.scene_dirty = true;
        }
    }

    fn fit_to_selection(&mut self) {
        let sel: Vec<u32> = self.selection.iter().copied().collect();
        if sel.is_empty() {
            self.fit_view();
        } else {
            self.fit_to(&sel);
        }
    }

    // ----- dialogs -----

    fn open_dialog(&mut self, ctx: &egui::Context) {
        if let Some(p) = rfd::FileDialog::new().add_filter("STEP", &["stp", "step", "STP", "STEP", "p21"]).pick_file() {
            self.open(p, ctx);
        }
    }

    fn save_dialog(&mut self, ctx: &egui::Context) {
        let Some(loaded) = self.loaded.clone() else { return };
        let stem = loaded.path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or("trimmed".into());
        let dialog = rfd::FileDialog::new().add_filter("STEP", &["stp", "step"]).set_file_name(format!("{stem}-trimmed.stp"));
        let dialog = match loaded.path.parent() {
            Some(dir) => dialog.set_directory(dir),
            None => dialog,
        };
        if let Some(out) = dialog.save_file() {
            let remove: Vec<u32> = self.removed_occ.iter().enumerate().filter(|(_, &r)| r).map(|(i, _)| i as u32).collect();
            self.saving = true;
            self.error = None;
            self.notice = Some(format!("Saving {}…", out.display()));
            loader::spawn_save(self.generation, loaded, remove, out, self.tx.clone(), ctx.clone());
        }
    }

    // ----- tree -----

    fn row_tint(&self, ui: &egui::Ui, rect: egui::Rect, inst: u32, in_sel: bool) {
        if self.hover_box == Some(inst) {
            ui.painter().rect_filled(rect, 2.0, egui::Color32::from_rgba_unmultiplied(255, 230, 80, 60));
        } else if in_sel && !self.selection.contains(&inst) {
            ui.painter().rect_filled(rect, 2.0, egui::Color32::from_rgba_unmultiplied(255, 160, 60, 35));
        }
    }

    fn tree_node(&mut self, ui: &mut egui::Ui, inst: u32, depth: usize, ancestor_selected: bool) {
        let loaded = self.loaded.clone().unwrap();
        let instance = &loaded.instances[inst as usize];
        let def = instance.definition;
        let name = loaded.tree.definition_name(def).to_string();
        let nchildren = self.inst_children[inst as usize].len();
        let removed = instance.occurrence.map(|o| self.removed_occ[o as usize]).unwrap_or(false);
        let hidden = self.hidden[inst as usize];
        let user_hidden = self.user_hidden[inst as usize];
        let bodies = loaded.tree.definitions[def as usize].bodies;
        let mut label = if nchildren == 0 { name.clone() } else { format!("{}  ({})", name, loaded.tree.instance_count[def as usize] - 1) };
        if bodies > 1 {
            label.push_str(&format!("  [{} bodies]", bodies));
        }
        if user_hidden {
            label.push_str("  · hidden");
        }
        if self.def_full.get(def as usize).map(|f| f.is_some()).unwrap_or(false) {
            label.push_str("  · B-rep");
        }
        let mut text = egui::RichText::new(label);
        if removed {
            text = text.strikethrough().color(egui::Color32::from_rgb(200, 80, 80));
        } else if self.removed_hidden[inst as usize] {
            text = text.color(egui::Color32::GRAY);
        } else if hidden {
            text = text.italics().color(egui::Color32::from_gray(120));
        } else if self.def_geom.get(def as usize).map(|g| g.is_none()).unwrap_or(true) && bodies > 0 {
            text = text.color(egui::Color32::from_gray(150));
        }
        let selected = self.selection.contains(&inst);
        let in_sel = selected || ancestor_selected;
        self.row_order.push(inst);

        let response = if nchildren == 0 {
            let r = ui.selectable_label(selected, text);
            self.row_tint(ui, r.rect, inst, in_sel);
            r
        } else {
            let open = depth == 0 || self.expanded_to.contains(&inst);
            let header = egui::CollapsingHeader::new(text)
                .id_salt(inst)
                .default_open(open)
                .open(if open { Some(true) } else { None })
                .show(ui, |ui| {
                    let kids: Vec<u32> = self.inst_children[inst as usize].clone();
                    for k in kids {
                        self.tree_node(ui, k, depth + 1, in_sel);
                    }
                });
            if open {
                self.expanded_to.remove(&inst);
            }
            let r = header.header_response;
            self.row_tint(ui, r.rect, inst, in_sel);
            if selected {
                ui.painter().rect_stroke(r.rect, 2.0, egui::Stroke::new(1.5, egui::Color32::from_rgb(255, 140, 26)), egui::StrokeKind::Outside);
            }
            r
        };
        if self.scroll_to == Some(inst) {
            response.scroll_to_me(Some(egui::Align::Center));
            self.scroll_to = None;
        }
        if response.hovered() {
            self.hover_tree = Some(inst);
        }
        if response.clicked() {
            let mods = ui.input(|i| i.modifiers);
            self.click_select(inst, mods);
        }
        let occurrence = instance.occurrence;
        let same_def_count = loaded.tree.occurrences.iter().filter(|o| o.child == def).count();
        let parent_shared = instance.parent.map(|p| {
            let pd = loaded.instances[p as usize].definition;
            loaded.instances.iter().filter(|x| x.definition == pd).count()
        });
        response.context_menu(|ui| {
            if !selected {
                self.selection.clear();
                self.selection.insert(inst);
                self.anchor = Some(inst);
                self.scene_dirty = true;
            }
            let nsel = self.selection.len();
            if removed {
                if ui.button("Restore").clicked() {
                    self.restore_instance(inst);
                    ui.close();
                }
            } else if occurrence.is_some() {
                let label = if nsel > 1 {
                    format!("Remove {} selected", nsel)
                } else {
                    match parent_shared {
                        Some(n) if n > 1 => format!("Remove (from all {} instances of its parent)", n),
                        _ => "Remove".to_string(),
                    }
                };
                if ui.button(label).clicked() {
                    self.remove_selected();
                    ui.close();
                }
                if same_def_count > 1 && ui.button(format!("Remove all {} occurrences of this part", same_def_count)).clicked() {
                    self.remove_all_of_definition(def);
                    ui.close();
                }
            }
            if ui.button(if nsel > 1 { "Fit view to selection" } else { "Fit view to this" }).clicked() {
                self.fit_to_selection();
                ui.close();
            }
            ui.separator();
            if hidden && !self.removed_hidden[inst as usize] {
                if ui.button("Show").clicked() {
                    self.show_selected();
                    ui.close();
                }
            } else if ui.button(if nsel > 1 { "Hide selected  (H)" } else { "Hide  (H)" }).clicked() {
                self.hide_selected();
                ui.close();
            }
            if ui.button("Isolate  (Shift+H)").on_hover_text("hide everything else").clicked() {
                self.isolate_selected();
                ui.close();
            }
            if self.user_hidden_count() > 0 && ui.button("Show all  (U)").clicked() {
                self.show_all();
                ui.close();
            }
            ui.separator();
            if ui.button("Full load  (exact B-rep)").on_hover_text("tessellate the selected parts with Open CASCADE through FreeCAD; replaces the sampled bodies").clicked() {
                self.full_load_selection(ctx_of(ui));
                ui.close();
            }
            ui.separator();
            if ui.button("Export selection as one STEP…").on_hover_text("keeps the assembly structure down to the selection").clicked() {
                self.export_one_dialog(ctx_of(ui));
                ui.close();
            }
            if ui.button("Export each selected part as a STEP…").on_hover_text("one standalone file per part, the part as its root").clicked() {
                self.export_separate_dialog(ctx_of(ui));
                ui.close();
            }
        });
    }

    fn pick(&self, pos: egui::Pos2) -> Option<u32> {
        let loaded = self.loaded.as_ref()?;
        let rect = self.last_rect;
        if !rect.contains(pos) || rect.width() < 1.0 || rect.height() < 1.0 {
            return None;
        }
        let nx = (pos.x - rect.left()) / rect.width() * 2.0 - 1.0;
        let ny = 1.0 - (pos.y - rect.top()) / rect.height() * 2.0;
        let inv = self.last_view_proj.inverse();
        let unproject = |z: f32| {
            let p = inv * glam::Vec4::new(nx, ny, z, 1.0);
            (p.truncate() / p.w).as_dvec3()
        };
        let near = unproject(0.0);
        let far = unproject(1.0);
        let dir = (far - near).normalize();
        let origin = self.origin;
        let mut best: Option<(f64, u32)> = None;
        for (i, inst) in loaded.instances.iter().enumerate() {
            if self.hidden[i] {
                continue;
            }
            let Some(b) = self.def_aabb[inst.definition as usize] else { continue };
            let Some(inv_w) = inst.world.inverse() else { continue };
            let o_world = [near.x + origin.x, near.y + origin.y, near.z + origin.z];
            let o = inv_w.apply(o_world);
            let d = [
                inv_w.m[0][0] * dir.x + inv_w.m[0][1] * dir.y + inv_w.m[0][2] * dir.z,
                inv_w.m[1][0] * dir.x + inv_w.m[1][1] * dir.y + inv_w.m[1][2] * dir.z,
                inv_w.m[2][0] * dir.x + inv_w.m[2][1] * dir.y + inv_w.m[2][2] * dir.z,
            ];
            let mut tmin = 0.0f64;
            let mut tmax = f64::INFINITY;
            let mut hit = true;
            for k in 0..3 {
                if d[k].abs() < 1e-12 {
                    if o[k] < b.min[k] || o[k] > b.max[k] {
                        hit = false;
                        break;
                    }
                } else {
                    let t1 = (b.min[k] - o[k]) / d[k];
                    let t2 = (b.max[k] - o[k]) / d[k];
                    tmin = tmin.max(t1.min(t2));
                    tmax = tmax.min(t1.max(t2));
                    if tmin > tmax {
                        hit = false;
                        break;
                    }
                }
            }
            if hit && best.map(|(t, _)| tmin < t).unwrap_or(true) {
                best = Some((tmin, i as u32));
            }
        }
        best.map(|(_, i)| i)
    }

    fn instance_path(&self, inst: u32) -> String {
        let Some(loaded) = &self.loaded else { return String::new() };
        let it = &loaded.instances[inst as usize];
        let mut path = vec![loaded.tree.definition_name(it.definition).to_string()];
        let mut p = it.parent;
        while let Some(a) = p {
            path.push(loaded.tree.definition_name(loaded.instances[a as usize].definition).to_string());
            p = loaded.instances[a as usize].parent;
        }
        path.reverse();
        path.join("  ›  ")
    }

    fn viewport(&mut self, ui: &mut egui::Ui) {
        let size = ui.available_size();
        let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
        let dragging = resp.dragged();
        if resp.dragged_by(egui::PointerButton::Primary) {
            let d = resp.drag_delta();
            self.camera.yaw -= d.x * 0.01;
            self.camera.pitch = (self.camera.pitch + d.y * 0.01).clamp(-1.55, 1.55);
            self.auto_fit = false;
        }
        if resp.dragged_by(egui::PointerButton::Secondary) || resp.dragged_by(egui::PointerButton::Middle) {
            let d = resp.drag_delta();
            self.camera.pan(d.x, d.y, rect.height());
            self.auto_fit = false;
        }
        if resp.hovered() {
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll.abs() > 0.0 {
                self.camera.distance *= (-scroll as f64 * 0.0025).exp();
                self.auto_fit = false;
            }
        }
        self.hover_box = None;
        if resp.hovered() && !dragging {
            if let Some(pos) = resp.hover_pos() {
                self.hover_box = self.pick(pos);
            }
        }
        if resp.clicked() {
            let mods = ui.input(|i| i.modifiers);
            if let Some(pos) = resp.interact_pointer_pos() {
                match self.pick(pos) {
                    Some(hit) => self.click_select(hit, mods),
                    None if !mods.ctrl && !mods.shift => self.clear_selection(),
                    None => {}
                }
            }
        }
        if resp.double_clicked() {
            if let Some(pos) = resp.interact_pointer_pos() {
                if let Some(hit) = self.pick(pos) {
                    self.fit_to(&[hit]);
                }
            }
        }
        ui.painter().rect_filled(rect, 0.0, egui::Color32::from_rgb(28, 30, 36));
        let aspect = rect.width() / rect.height().max(1.0);
        let vp = self.camera.view_proj(aspect);
        self.last_view_proj = vp;
        self.last_rect = rect;
        let cb = viewer::DrawCallback {
            scene: self.scene.clone(),
            view_proj: vp.to_cols_array(),
            light_dir: self.camera.light_dir(),
            splat_scale: self.splat_scale,
        };
        ui.painter().add(eframe::egui_wgpu::Callback::new_paint_callback(rect, cb));
        let hint = "click: select   Ctrl: toggle   Shift: range   Ctrl+I: invert   Ctrl+A: all   H: hide   Shift+H: isolate   U: show all   Del: remove   Ctrl+Z: undo   F: fit   double-click: fit   drag: orbit   right-drag: pan   wheel: zoom";
        ui.painter().text(rect.left_bottom() + egui::vec2(8.0, -8.0), egui::Align2::LEFT_BOTTOM, hint, egui::FontId::proportional(12.0), egui::Color32::from_gray(150));
        let header = match self.selection.len() {
            0 => String::new(),
            1 => self.instance_path(*self.selection.iter().next().unwrap()),
            n => format!("{} selected", n),
        };
        if !header.is_empty() {
            ui.painter().text(rect.left_top() + egui::vec2(8.0, 8.0), egui::Align2::LEFT_TOP, header, egui::FontId::proportional(13.0), egui::Color32::from_gray(220));
        }
        if let (Some(h), Some(pos)) = (self.hover_box, resp.hover_pos()) {
            if let Some(loaded) = &self.loaded {
                let name = loaded.tree.definition_name(loaded.instances[h as usize].definition).to_string();
                let galley = ui.painter().layout_no_wrap(name, egui::FontId::proportional(12.5), egui::Color32::WHITE);
                let p = pos + egui::vec2(14.0, 12.0);
                let bg = egui::Rect::from_min_size(p, galley.size() + egui::vec2(8.0, 4.0));
                ui.painter().rect_filled(bg, 3.0, egui::Color32::from_rgba_unmultiplied(0, 0, 0, 180));
                ui.painter().galley(p + egui::vec2(4.0, 2.0), galley, egui::Color32::WHITE);
            }
        }
    }

    fn filtered_list(&mut self, ui: &mut egui::Ui) {
        let Some(loaded) = self.loaded.clone() else { return };
        let needle = self.filter.to_lowercase();
        let mut shown = 0usize;
        let mut total = 0usize;
        let in_sel = self.selection_mask();
        for (i, inst) in loaded.instances.iter().enumerate() {
            let name = loaded.tree.definition_name(inst.definition);
            if !name.to_lowercase().contains(&needle) {
                continue;
            }
            total += 1;
            if shown >= 400 {
                continue;
            }
            shown += 1;
            let removed = inst.occurrence.map(|o| self.removed_occ[o as usize]).unwrap_or(false);
            let mut text = egui::RichText::new(format!("{}   (depth {})", name, inst.depth));
            if removed || self.hidden[i] {
                text = text.color(egui::Color32::GRAY);
            }
            let inst_id = i as u32;
            self.row_order.push(inst_id);
            let r = ui.selectable_label(self.selection.contains(&inst_id), text);
            self.row_tint(ui, r.rect, inst_id, in_sel[i]);
            if r.hovered() {
                self.hover_tree = Some(inst_id);
            }
            if r.clicked() {
                let mods = ui.input(|i| i.modifiers);
                self.click_select(inst_id, mods);
                if !mods.ctrl && !mods.shift {
                    self.fit_to(&[inst_id]);
                }
            }
        }
        if total > shown {
            ui.label(format!("{} more matches not shown", total - shown));
        }
        if total == 0 {
            ui.label("no matches");
        }
    }

    /// Splat size (live) and density (on Resample).
    fn splat_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.label("Disc size, applied live:");
        ui.add(egui::Slider::new(&mut self.splat_scale, 0.2..=4.0).logarithmic(true).text("× spacing"));
        ui.separator();
        ui.label("Density, applied on Resample:");
        ui.horizontal(|ui| {
            ui.label("samples across a part");
            ui.add(egui::DragValue::new(&mut self.samples_across).range(8.0..=400.0).speed(1.0));
        });
        ui.horizontal(|ui| {
            ui.label("max surfels per part");
            ui.add(egui::DragValue::new(&mut self.max_surfels_per_part).range(5_000..=2_000_000).speed(1000.0));
        });
        ui.horizontal(|ui| {
            ui.label("GPU budget, surfels");
            ui.add(egui::DragValue::new(&mut self.surfel_budget).range(500_000..=60_000_000).speed(100_000.0));
            ui.label(format!("≈ {} MB", self.surfel_budget as u64 * 32 / 1_048_576));
        });
        ui.horizontal(|ui| {
            if ui.add_enabled(self.loaded.is_some() && !self.sampling, egui::Button::new("Resample")).clicked() {
                self.resample(ctx);
            }
            if self.sampling {
                ui.spinner();
                ui.label(format!("{}/{}", self.geom_done, self.bounds_total));
            }
            ui.label(format!("{} surfels now", fmt(self.total_surfels as u64)));
        });
        ui.label(egui::RichText::new("Denser sampling costs time and GPU memory roughly with the square of the samples-across value; 56 is the default.").small().color(egui::Color32::GRAY));
        ui.separator();
        let nfull = self.def_full.iter().filter(|f| f.is_some()).count();
        ui.horizontal(|ui| {
            ui.label(format!("Exact B-rep meshes: {} part(s)", nfull));
            if ui.add_enabled(nfull > 0, egui::Button::new("Clear")).clicked() {
                self.clear_full_loads();
            }
        });
        match &self.freecad {
            Some(p) => ui.label(egui::RichText::new(format!("kernel host: {}", p.display())).small().color(egui::Color32::GRAY)),
            None => ui.label(egui::RichText::new("kernel host: FreeCAD not found (set STEPTRIM_FREECAD)").small().color(egui::Color32::from_rgb(230, 150, 90))),
        };
    }

    /// Selection by part size: a range of box diagonals.
    fn size_panel(&mut self, ui: &mut egui::Ui) {
        let Some(loaded) = self.loaded.clone() else { return };
        let mut max_all = 0.0f64;
        for (i, _) in loaded.instances.iter().enumerate() {
            if let Some(s) = self.instance_size(i as u32) {
                max_all = max_all.max(s);
            }
        }
        ui.label("Part box diagonal, file units. Visible parts only.");
        ui.horizontal(|ui| {
            ui.label("from");
            ui.add(egui::DragValue::new(&mut self.size_min).range(0.0..=max_all).speed(max_all * 0.002));
            ui.label("to");
            ui.add(egui::DragValue::new(&mut self.size_max).range(0.0..=max_all).speed(max_all * 0.002));
        });
        if self.size_max < self.size_min {
            self.size_max = self.size_min;
        }
        ui.horizontal(|ui| {
            ui.label("quick:");
            for (label, frac) in [("< 1%", 0.01), ("< 2%", 0.02), ("< 5%", 0.05), ("< 10%", 0.10)] {
                if ui.small_button(label).on_hover_text("of the largest part").clicked() {
                    self.size_min = 0.0;
                    self.size_max = max_all * frac;
                }
            }
        });
        let matches = self.instances_in_size_range();
        ui.label(format!("{} parts match (largest part: {:.1})", matches.len(), max_all));
        ui.horizontal(|ui| {
            if ui.add_enabled(!matches.is_empty(), egui::Button::new("Select")).clicked() {
                self.selection.clear();
                for &i in &matches {
                    self.selection.insert(i);
                }
                self.scene_dirty = true;
            }
            if ui.add_enabled(!matches.is_empty(), egui::Button::new("Add to selection")).clicked() {
                for &i in &matches {
                    self.selection.insert(i);
                }
                self.scene_dirty = true;
            }
            if ui.add_enabled(!matches.is_empty(), egui::Button::new("Remove them")).clicked() {
                self.remove_instances(&matches);
            }
        });
    }
}

fn ctx_of(ui: &egui::Ui) -> &egui::Context {
    ui.ctx()
}

fn def_color(def: u32) -> [f32; 4] {
    let h = (def as f32 * 0.618_034) % 1.0;
    let (r, g, b) = hsv(h, 0.45, 0.85);
    [r, g, b, 1.0]
}

fn hsv(h: f32, s: f32, v: f32) -> (f32, f32, f32) {
    let i = (h * 6.0).floor();
    let f = h * 6.0 - i;
    let p = v * (1.0 - s);
    let q = v * (1.0 - f * s);
    let t = v * (1.0 - (1.0 - f) * s);
    match (i as i32).rem_euclid(6) {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    }
}

impl eframe::App for App {
    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = root.ctx().clone();
        let ctx = &ctx;
        self.drain_events();
        if self.scripted_pending {
            self.scripted_pending = false;
            self.run_scripted(ctx);
        }
        if let Some(path) = ctx.input(|i| i.raw.dropped_files.first().map(|f| f.path().to_path_buf())) {
            self.open(path, ctx);
        }
        let (del, undo, fit, select_all, invert, hide, isolate, unhide) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::Delete),
                i.modifiers.ctrl && i.key_pressed(egui::Key::Z),
                i.key_pressed(egui::Key::F) && !i.modifiers.any(),
                i.modifiers.ctrl && i.key_pressed(egui::Key::A),
                i.modifiers.ctrl && i.key_pressed(egui::Key::I),
                i.key_pressed(egui::Key::H) && !i.modifiers.any(),
                i.modifiers.shift && !i.modifiers.ctrl && i.key_pressed(egui::Key::H),
                i.key_pressed(egui::Key::U) && !i.modifiers.any(),
            )
        });
        let text_focused = ctx.memory(|m| m.focused().is_some());
        if !text_focused {
            if del {
                self.remove_selected();
            }
            if undo {
                self.undo_last();
            }
            if fit {
                self.fit_to_selection();
            }
            if invert {
                self.invert_selection();
            }
            if hide {
                self.hide_selected();
            }
            if isolate {
                self.isolate_selected();
            }
            if unhide {
                self.show_all();
            }
            if select_all {
                if let Some(loaded) = &self.loaded {
                    for (i, _) in loaded.instances.iter().enumerate() {
                        if !self.hidden[i] {
                            self.selection.insert(i as u32);
                        }
                    }
                    self.scene_dirty = true;
                }
            }
        }
        self.hover_tree = None;
        self.last_row_order = std::mem::take(&mut self.row_order);
        if self.loading || self.saving || self.sampling || self.full_loading {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }

        egui::Panel::top("toolbar").show(root, |ui| {
            ui.horizontal(|ui| {
                if ui.button("Open…").clicked() {
                    self.open_dialog(ctx);
                }
                ui.separator();
                let can_edit = self.loaded.is_some() && !self.saving;
                let nsel = self.selection.len();
                let remove_label = if nsel > 1 { format!("Remove {} selected", nsel) } else { "Remove selected".to_string() };
                if ui.add_enabled(can_edit && nsel > 0, egui::Button::new(remove_label)).clicked() {
                    self.remove_selected();
                }
                if ui.add_enabled(can_edit && !self.undo.is_empty(), egui::Button::new(format!("Undo ({})", self.undo.len()))).clicked() {
                    self.undo_last();
                }
                if ui.add_enabled(self.loaded.is_some(), egui::Button::new("Fit view")).on_hover_text("F: fit to the selection, or everything").clicked() {
                    self.fit_to_selection();
                }
                let removed = self.removed_count();
                let save_label = if removed > 0 { format!("Save trimmed… ({} removed)", removed) } else { "Save trimmed…".to_string() };
                if ui
                    .add_enabled(can_edit && removed > 0 && !self.loading, egui::Button::new(save_label))
                    .on_disabled_hover_text("Remove something first; saving writes the file without those occurrences")
                    .clicked()
                {
                    self.save_dialog(ctx);
                }
                let has_sel = nsel > 0;
                if ui
                    .add_enabled(can_edit && has_sel && !self.loading, egui::Button::new("Export selection…"))
                    .on_hover_text("one STEP file containing only the selection, assembly structure kept")
                    .clicked()
                {
                    self.export_one_dialog(ctx);
                }
                if ui
                    .add_enabled(can_edit && has_sel && !self.loading, egui::Button::new("Export parts…"))
                    .on_hover_text("one standalone STEP file per selected part, into a folder")
                    .clicked()
                {
                    self.export_separate_dialog(ctx);
                }
                ui.separator();
                let full_hint = match &self.freecad {
                    Some(p) => format!("exact B-rep tessellation of the selected parts via Open CASCADE ({})", p.display()),
                    None => "needs FreeCAD (FreeCADCmd.exe); set STEPTRIM_FREECAD if it is installed elsewhere".to_string(),
                };
                if ui
                    .add_enabled(can_edit && has_sel && !self.full_loading && self.freecad.is_some(), egui::Button::new("Full load"))
                    .on_hover_text(&full_hint)
                    .on_disabled_hover_text(&full_hint)
                    .clicked()
                {
                    self.full_load_selection(ctx);
                }
                if self.full_loading {
                    ui.spinner();
                    ui.label(format!("B-rep {} left", self.full_pending));
                }
                ui.separator();
                if ui.add_enabled(can_edit && has_sel, egui::Button::new("Hide")).on_hover_text("H").clicked() {
                    self.hide_selected();
                }
                if ui.add_enabled(can_edit && has_sel, egui::Button::new("Isolate")).on_hover_text("Shift+H: hide everything else").clicked() {
                    self.isolate_selected();
                }
                let nh = self.user_hidden_count();
                if ui.add_enabled(can_edit && nh > 0, egui::Button::new(format!("Show all ({nh})"))).on_hover_text("U").clicked() {
                    self.show_all();
                }
                ui.separator();
                let mut changed = false;
                ui.label("bodies:");
                changed |= ui.selectable_value(&mut self.body_mode, BodyMode::Splats, "splats").on_hover_text("surface samples drawn as oriented discs").changed();
                changed |= ui.selectable_value(&mut self.body_mode, BodyMode::Mesh, "mesh").on_hover_text("triangles: planar faces exact, curved faces on their parameter grid").changed();
                if self.body_mode == BodyMode::Splats {
                    changed |= ui.checkbox(&mut self.show_surfels, "on").changed();
                }
                changed |= ui.checkbox(&mut self.show_lines, "edges").changed();
                changed |= ui.checkbox(&mut self.show_boxes, "boxes").on_hover_text("boxes are always drawn for parts whose bodies are not sampled yet").changed();
                if changed {
                    self.scene_dirty = true;
                }
                ui.toggle_value(&mut self.splat_panel, "Splats…").on_hover_text("disc size and sampling density");
                ui.separator();
                ui.toggle_value(&mut self.size_panel, "Select by size…");
                ui.separator();
                ui.label(&self.file_label);
                if self.loading {
                    ui.spinner();
                    ui.label(&self.stage);
                }
                if self.saving {
                    ui.spinner();
                    ui.label("saving");
                }
                if let Some(e) = &self.error {
                    ui.colored_label(egui::Color32::from_rgb(230, 90, 90), e);
                }
            });
        });

        egui::Panel::bottom("status").show(root, |ui| {
            ui.horizontal_wrapped(|ui| {
                if let Some(l) = &self.loaded {
                    let t = &l.timings;
                    ui.label(format!(
                        "{} entities · {} products · {} occurrences · {} instances · scan {:.0} ms · tree {:.1} ms · transforms {:.1} ms",
                        fmt(l.table.len() as u64),
                        fmt(l.tree.products.len() as u64),
                        fmt(l.tree.occurrences.len() as u64),
                        fmt(l.instances.len() as u64),
                        t.scan_ms,
                        t.tree_ms,
                        t.transforms_ms,
                    ));
                    ui.separator();
                    let frac_b = if self.bounds_total > 0 { self.bounds_done as f32 / self.bounds_total as f32 } else { 0.0 };
                    ui.add(egui::ProgressBar::new(frac_b).desired_width(130.0).text(format!("boxes {}/{}", self.bounds_done, self.bounds_total)));
                    if let Some(ms) = self.bounds_ms {
                        ui.label(format!("{:.0} ms", ms));
                    }
                    let frac_g = if self.bounds_total > 0 { self.geom_done as f32 / self.bounds_total as f32 } else { 0.0 };
                    ui.add(egui::ProgressBar::new(frac_g).desired_width(130.0).text(format!("bodies {}/{}", self.geom_done, self.bounds_total)));
                    if let Some(ms) = self.geom_ms {
                        ui.label(format!("{:.0} ms", ms));
                    }
                    ui.separator();
                    ui.label(format!(
                        "{} surfels+vertices · {} triangles · {} edge segments · {} placed bodies · {} boxes",
                        fmt(self.total_surfels as u64),
                        fmt(self.total_triangles as u64),
                        fmt(self.total_lines as u64),
                        fmt(self.visible_draws as u64),
                        fmt(self.visible_boxes as u64)
                    ));
                    if !self.selection.is_empty() {
                        ui.label(format!("· {} selected", self.selection.len()));
                    }
                    let removed = self.removed_count();
                    if removed > 0 {
                        ui.label(format!("· {} occurrences removed ({} instances)", removed, self.removed_hidden.iter().filter(|&&h| h).count()));
                    }
                    let nh = self.user_hidden_count();
                    if nh > 0 {
                        ui.label(format!("· {} hidden", nh));
                    }
                } else {
                    ui.label("Open a STEP file (button, command line, or drag and drop).");
                }
                if let Some(n) = &self.notice {
                    ui.separator();
                    ui.label(egui::RichText::new(n).color(egui::Color32::from_rgb(120, 200, 140)));
                }
            });
        });

        egui::Panel::left("tree").default_size(380.0).min_size(240.0).show(root, |ui| {
            ui.horizontal(|ui| {
                ui.label("Product tree");
                ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("filter by name").desired_width(160.0));
                if !self.filter.is_empty() && ui.small_button("×").clicked() {
                    self.filter.clear();
                }
            });
            if self.splat_panel && self.loaded.is_some() {
                ui.separator();
                egui::CollapsingHeader::new("Splats").default_open(true).show(ui, |ui| self.splat_panel(ui, ctx));
            }
            if self.size_panel && self.loaded.is_some() {
                ui.separator();
                egui::CollapsingHeader::new("Select by size").default_open(true).show(ui, |ui| self.size_panel(ui));
            }
            ui.separator();
            egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
                if self.loaded.is_some() && !self.filter.is_empty() {
                    self.filtered_list(ui);
                } else if let Some(loaded) = self.loaded.clone() {
                    let roots: Vec<u32> = loaded.instances.iter().enumerate().filter(|(_, i)| i.parent.is_none()).map(|(i, _)| i as u32).collect();
                    for r in roots {
                        self.tree_node(ui, r, 0, false);
                    }
                } else if self.loading {
                    ui.label(format!("{}…", self.stage));
                }
            });
        });

        egui::CentralPanel::no_frame().show(root, |ui| {
            self.viewport(ui);
        });

        let hover = self.hover_tree.or(self.hover_box);
        if hover != self.scene_hover {
            self.scene_dirty = true;
        }
        if self.scene_dirty {
            self.rebuild_scene();
            ctx.request_repaint();
        }
    }
}

fn fmt(n: u64) -> String {
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
