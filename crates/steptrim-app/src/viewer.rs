//! wgpu renderer drawn inside egui through a paint callback. Three layers:
//! instanced boxes, edge lines, and surfels (oriented discs with a Gaussian
//! footprint, drawn opaque with a depth test so no sorting is needed).
//! Lines and surfels are stored once per definition and drawn once per
//! visible instance with a per-instance uniform selected by dynamic offset.

use eframe::egui_wgpu::{self, wgpu, CallbackResources, CallbackTrait, ScreenDescriptor};
use eframe::wgpu::util::DeviceExt;
use std::sync::{Arc, Mutex};

#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct InstanceRaw {
    /// Column-major model matrix mapping the unit cube [-1, 1]^3 to the box.
    pub model: [f32; 16],
    pub color: [f32; 4],
}

/// Per visible part instance: its placement and colour, read by the line
/// and surfel shaders through a dynamic uniform offset.
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct InstanceUniform {
    pub model: [f32; 16],
    pub color: [f32; 4],
}

/// Uniform stride: one 256-byte slot per instance (the common minimum
/// dynamic-offset alignment).
pub const INSTANCE_STRIDE: u64 = 256;

#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct SurfelRaw {
    pub pos: [f32; 3],
    pub radius: f32,
    pub nrm: [f32; 3],
    pub _pad: f32,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Vertex {
    pos: [f32; 3],
    nrm: [f32; 3],
}

/// Mesh vertex shared by all definitions' triangle meshes.
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct MeshVertexRaw {
    pub pos: [f32; 3],
    pub nrm: [f32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct CameraUniform {
    view_proj: [f32; 16],
    light_dir: [f32; 4],
    /// x = splat size multiplier; the rest is padding.
    params: [f32; 4],
}

/// One draw of a definition's geometry for one placed instance.
#[derive(Clone, Copy, Debug)]
pub struct GeomDraw {
    /// Index into `Scene::instance_uniforms`.
    pub instance: u32,
    /// Range of line vertices (pairs) in `Scene::lines`.
    pub lines: (u32, u32),
    /// Range of surfels in `Scene::surfels`.
    pub surfels: (u32, u32),
    /// Triangle mesh: (first index, index count, base vertex).
    pub mesh: (u32, u32, u32),
    /// The mesh is an exact kernel tessellation: draw it in every mode.
    pub full: bool,
}

/// Everything the UI thread hands to the renderer. Geometry arrays only
/// grow (append per definition) so uploads are incremental.
pub struct Scene {
    pub boxes: Vec<InstanceRaw>,
    pub box_version: u64,
    pub instance_uniforms: Vec<InstanceUniform>,
    pub draws: Vec<GeomDraw>,
    pub draw_version: u64,
    pub lines: Vec<[f32; 3]>,
    pub surfels: Vec<SurfelRaw>,
    pub mesh_vertices: Vec<MeshVertexRaw>,
    pub mesh_indices: Vec<u32>,
    pub geom_version: u64,
    pub show_lines: bool,
    pub show_surfels: bool,
    pub show_mesh: bool,
}

impl Scene {
    pub fn empty() -> Self {
        Self {
            boxes: Vec::new(),
            box_version: 0,
            instance_uniforms: Vec::new(),
            draws: Vec::new(),
            draw_version: 0,
            lines: Vec::new(),
            surfels: Vec::new(),
            mesh_vertices: Vec::new(),
            mesh_indices: Vec::new(),
            geom_version: 0,
            show_lines: true,
            show_surfels: true,
            show_mesh: false,
        }
    }
}

/// A growable GPU buffer mirrored from a CPU vector; uploads only the tail
/// that arrived since the last frame.
struct GrowBuffer {
    buf: Option<wgpu::Buffer>,
    cap_bytes: u64,
    uploaded_bytes: u64,
    usage: wgpu::BufferUsages,
    label: &'static str,
}

impl GrowBuffer {
    fn new(label: &'static str, usage: wgpu::BufferUsages) -> Self {
        Self { buf: None, cap_bytes: 0, uploaded_bytes: 0, usage, label }
    }
    /// Make the buffer hold `bytes`; returns true if it was reallocated
    /// (and therefore needs a full upload).
    fn ensure(&mut self, device: &wgpu::Device, bytes: u64) -> bool {
        if self.buf.is_some() && bytes <= self.cap_bytes {
            return false;
        }
        let cap = bytes.max(1 << 20).next_power_of_two();
        self.buf = Some(device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(self.label),
            size: cap,
            usage: self.usage | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        }));
        self.cap_bytes = cap;
        self.uploaded_bytes = 0;
        true
    }
    /// Upload `data` (whole CPU array); only the unseen tail is written
    /// unless the buffer was reallocated.
    fn sync(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, data: &[u8]) {
        let bytes = data.len() as u64;
        if bytes == 0 {
            return;
        }
        self.ensure(device, bytes);
        if bytes < self.uploaded_bytes {
            self.uploaded_bytes = 0; // shrank: rewrite everything
        }
        if bytes > self.uploaded_bytes {
            let start = (self.uploaded_bytes / 4) * 4;
            queue.write_buffer(self.buf.as_ref().unwrap(), start, &data[start as usize..]);
            self.uploaded_bytes = bytes;
        }
    }
}

struct Resources {
    boxes_faces: wgpu::RenderPipeline,
    boxes_edges: wgpu::RenderPipeline,
    lines: wgpu::RenderPipeline,
    surfels: wgpu::RenderPipeline,
    mesh: wgpu::RenderPipeline,
    face_vb: wgpu::Buffer,
    face_ib: wgpu::Buffer,
    edge_vb: wgpu::Buffer,
    edge_ib: wgpu::Buffer,
    quad_vb: wgpu::Buffer,
    quad_ib: wgpu::Buffer,
    camera: wgpu::Buffer,
    camera_bg: wgpu::BindGroup,
    instance_layout: wgpu::BindGroupLayout,
    instance_buf: Option<wgpu::Buffer>,
    instance_bg: Option<wgpu::BindGroup>,
    instance_cap: usize,
    box_buf: GrowBuffer,
    box_count: u32,
    box_version: u64,
    line_buf: GrowBuffer,
    surfel_buf: GrowBuffer,
    mesh_vb: GrowBuffer,
    mesh_ib: GrowBuffer,
    geom_version: u64,
    draws: Vec<GeomDraw>,
    draw_version: u64,
    show_lines: bool,
    show_surfels: bool,
    show_mesh: bool,
}

const SHADER: &str = r#"
struct Camera {
    view_proj: mat4x4<f32>,
    light_dir: vec4<f32>,
    params: vec4<f32>,
};
@group(0) @binding(0) var<uniform> cam: Camera;

struct Inst {
    model: mat4x4<f32>,
    color: vec4<f32>,
};
@group(1) @binding(0) var<uniform> inst: Inst;

// ---- boxes (instanced unit cube) ----
struct BoxIn {
    @location(0) pos: vec3<f32>,
    @location(1) nrm: vec3<f32>,
    @location(2) m0: vec4<f32>,
    @location(3) m1: vec4<f32>,
    @location(4) m2: vec4<f32>,
    @location(5) m3: vec4<f32>,
    @location(6) color: vec4<f32>,
};
struct Shaded {
    @builtin(position) pos: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) nrm: vec3<f32>,
};
fn box_vertex(in: BoxIn) -> Shaded {
    let m = mat4x4<f32>(in.m0, in.m1, in.m2, in.m3);
    let wp = m * vec4<f32>(in.pos, 1.0);
    var o: Shaded;
    o.pos = cam.view_proj * wp;
    o.color = in.color;
    o.nrm = normalize((m * vec4<f32>(in.nrm, 0.0)).xyz);
    return o;
}
@vertex
fn vs_box(in: BoxIn) -> Shaded {
    return box_vertex(in);
}
@fragment
fn fs_faces(in: Shaded) -> @location(0) vec4<f32> {
    let l = max(dot(normalize(in.nrm), normalize(cam.light_dir.xyz)), 0.0);
    return vec4<f32>(in.color.rgb * (0.4 + 0.6 * l), in.color.a);
}
@fragment
fn fs_edges(in: Shaded) -> @location(0) vec4<f32> {
    return vec4<f32>(in.color.rgb * 0.3, 1.0);
}
// Box edges: same as vs_box, nudged toward the camera so they draw over the faces
// (a depth bias is not allowed on line topologies).
@vertex
fn vs_box_edge(in: BoxIn) -> Shaded {
    var o = box_vertex(in);
    o.pos.z = o.pos.z - o.pos.w * 0.0015;
    return o;
}

// ---- edge lines (per definition, per instance uniform) ----
struct LineIn { @location(0) pos: vec3<f32> };
struct LineOut { @builtin(position) pos: vec4<f32>, @location(0) color: vec4<f32> };
@vertex
fn vs_line(in: LineIn) -> LineOut {
    var o: LineOut;
    o.pos = cam.view_proj * (inst.model * vec4<f32>(in.pos, 1.0));
    o.pos.z = o.pos.z - o.pos.w * 0.0015;
    o.color = vec4<f32>(inst.color.rgb * 0.22, 1.0);
    return o;
}
@fragment
fn fs_line(in: LineOut) -> @location(0) vec4<f32> {
    return in.color;
}

// ---- surfels: oriented discs with a Gaussian footprint ----
struct SurfelIn {
    @location(0) corner: vec2<f32>,
    @location(1) center: vec3<f32>,
    @location(2) radius: f32,
    @location(3) nrm: vec3<f32>,
};
struct SurfelOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) nrm: vec3<f32>,
    @location(2) color: vec4<f32>,
};
@vertex
fn vs_surfel(in: SurfelIn) -> SurfelOut {
    let n = normalize(in.nrm);
    var up = vec3<f32>(0.0, 0.0, 1.0);
    if (abs(n.z) > 0.9) { up = vec3<f32>(1.0, 0.0, 0.0); }
    let t = normalize(cross(n, up));
    let b = cross(n, t);
    let local = in.center + (t * in.corner.x + b * in.corner.y) * (in.radius * cam.params.x);
    let wp = inst.model * vec4<f32>(local, 1.0);
    var o: SurfelOut;
    o.pos = cam.view_proj * wp;
    o.uv = in.corner;
    o.nrm = normalize((inst.model * vec4<f32>(n, 0.0)).xyz);
    o.color = inst.color;
    return o;
}
// ---- triangle mesh (per definition, per instance uniform) ----
struct MeshIn { @location(0) pos: vec3<f32>, @location(1) nrm: vec3<f32> };
@vertex
fn vs_mesh(in: MeshIn) -> Shaded {
    var o: Shaded;
    o.pos = cam.view_proj * (inst.model * vec4<f32>(in.pos, 1.0));
    o.color = inst.color;
    o.nrm = normalize((inst.model * vec4<f32>(in.nrm, 0.0)).xyz);
    return o;
}
@fragment
fn fs_mesh(in: Shaded) -> @location(0) vec4<f32> {
    let l = abs(dot(normalize(in.nrm), normalize(cam.light_dir.xyz)));
    return vec4<f32>(in.color.rgb * (0.35 + 0.65 * l), 1.0);
}

@fragment
fn fs_surfel(in: SurfelOut) -> @location(0) vec4<f32> {
    let r2 = dot(in.uv, in.uv);
    if (r2 > 1.0) { discard; }
    let l = abs(dot(normalize(in.nrm), normalize(cam.light_dir.xyz)));
    let shade = 0.35 + 0.65 * l;
    // Gaussian footprint darkens the rim slightly so overlapping discs read as a surface.
    let g = exp(-1.2 * r2);
    return vec4<f32>(in.color.rgb * shade * (0.85 + 0.15 * g), 1.0);
}
"#;

fn cube_faces() -> (Vec<Vertex>, Vec<u16>) {
    let faces: [([f32; 3], [[f32; 3]; 4]); 6] = [
        ([1.0, 0.0, 0.0], [[1.0, -1.0, -1.0], [1.0, 1.0, -1.0], [1.0, 1.0, 1.0], [1.0, -1.0, 1.0]]),
        ([-1.0, 0.0, 0.0], [[-1.0, 1.0, -1.0], [-1.0, -1.0, -1.0], [-1.0, -1.0, 1.0], [-1.0, 1.0, 1.0]]),
        ([0.0, 1.0, 0.0], [[1.0, 1.0, -1.0], [-1.0, 1.0, -1.0], [-1.0, 1.0, 1.0], [1.0, 1.0, 1.0]]),
        ([0.0, -1.0, 0.0], [[-1.0, -1.0, -1.0], [1.0, -1.0, -1.0], [1.0, -1.0, 1.0], [-1.0, -1.0, 1.0]]),
        ([0.0, 0.0, 1.0], [[-1.0, -1.0, 1.0], [1.0, -1.0, 1.0], [1.0, 1.0, 1.0], [-1.0, 1.0, 1.0]]),
        ([0.0, 0.0, -1.0], [[1.0, -1.0, -1.0], [-1.0, -1.0, -1.0], [-1.0, 1.0, -1.0], [1.0, 1.0, -1.0]]),
    ];
    let mut v = Vec::with_capacity(24);
    let mut idx = Vec::with_capacity(36);
    for (f, (nrm, quad)) in faces.iter().enumerate() {
        let b = (f * 4) as u16;
        for p in quad {
            v.push(Vertex { pos: *p, nrm: *nrm });
        }
        idx.extend_from_slice(&[b, b + 1, b + 2, b, b + 2, b + 3]);
    }
    (v, idx)
}

fn cube_edges() -> (Vec<Vertex>, Vec<u16>) {
    let mut v = Vec::with_capacity(8);
    for i in 0..8u16 {
        v.push(Vertex {
            pos: [
                if i & 1 == 0 { -1.0 } else { 1.0 },
                if i & 2 == 0 { -1.0 } else { 1.0 },
                if i & 4 == 0 { -1.0 } else { 1.0 },
            ],
            nrm: [0.0, 0.0, 1.0],
        });
    }
    let idx: Vec<u16> = vec![0, 1, 1, 3, 3, 2, 2, 0, 4, 5, 5, 7, 7, 6, 6, 4, 0, 4, 1, 5, 2, 6, 3, 7];
    (v, idx)
}

const VERTEX_LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
    array_stride: std::mem::size_of::<Vertex>() as u64,
    step_mode: wgpu::VertexStepMode::Vertex,
    attributes: &[
        wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x3, offset: 0, shader_location: 0 },
        wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x3, offset: 12, shader_location: 1 },
    ],
};

const BOX_INSTANCE_LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
    array_stride: std::mem::size_of::<InstanceRaw>() as u64,
    step_mode: wgpu::VertexStepMode::Instance,
    attributes: &[
        wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 0, shader_location: 2 },
        wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 16, shader_location: 3 },
        wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 32, shader_location: 4 },
        wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 48, shader_location: 5 },
        wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 64, shader_location: 6 },
    ],
};

const LINE_LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
    array_stride: 12,
    step_mode: wgpu::VertexStepMode::Vertex,
    attributes: &[wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x3, offset: 0, shader_location: 0 }],
};

const MESH_LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
    array_stride: std::mem::size_of::<MeshVertexRaw>() as u64,
    step_mode: wgpu::VertexStepMode::Vertex,
    attributes: &[
        wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x3, offset: 0, shader_location: 0 },
        wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x3, offset: 12, shader_location: 1 },
    ],
};

const QUAD_LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
    array_stride: 8,
    step_mode: wgpu::VertexStepMode::Vertex,
    attributes: &[wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x2, offset: 0, shader_location: 0 }],
};

const SURFEL_LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
    array_stride: std::mem::size_of::<SurfelRaw>() as u64,
    step_mode: wgpu::VertexStepMode::Instance,
    attributes: &[
        wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x3, offset: 0, shader_location: 1 },
        wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32, offset: 12, shader_location: 2 },
        wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x3, offset: 16, shader_location: 3 },
    ],
};

/// Create pipelines and static buffers and store them in egui's callback
/// resources. Call once from the app constructor.
pub fn init(render_state: &egui_wgpu::RenderState, depth_format: Option<wgpu::TextureFormat>) {
    let device = &render_state.device;
    // A GPU validation error (for example a buffer beyond the device limit)
    // must not take the editor down: log it and keep going.
    device.on_uncaptured_error(Arc::new(|e: wgpu::Error| {
        log::error!("wgpu error: {e}");
        eprintln!("wgpu error: {e}");
    }));
    log::info!("wgpu max_buffer_size = {} MiB", device.limits().max_buffer_size >> 20);
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("steptrim"),
        source: wgpu::ShaderSource::Wgsl(SHADER.into()),
    });
    let camera_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("camera"),
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
            ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
            count: None,
        }],
    });
    let instance_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("instance"),
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: true,
                min_binding_size: wgpu::BufferSize::new(std::mem::size_of::<InstanceUniform>() as u64),
            },
            count: None,
        }],
    });
    let layout_boxes = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("boxes"),
        bind_group_layouts: &[Some(&camera_layout)],
        immediate_size: 0,
    });
    let layout_geom = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("geometry"),
        bind_group_layouts: &[Some(&camera_layout), Some(&instance_layout)],
        immediate_size: 0,
    });
    let depth = |bias: i32, slope: f32| {
        depth_format.map(|format| wgpu::DepthStencilState {
            format,
            depth_write_enabled: Some(true),
            depth_compare: Some(wgpu::CompareFunction::Less),
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState { constant: bias, slope_scale: slope, clamp: 0.0 },
        })
    };
    let target = wgpu::ColorTargetState {
        format: render_state.target_format,
        blend: Some(wgpu::BlendState::REPLACE),
        write_mask: wgpu::ColorWrites::ALL,
    };
    let make = |label: &str,
                layout: &wgpu::PipelineLayout,
                vs: &str,
                fs: &str,
                buffers: &[Option<wgpu::VertexBufferLayout<'static>>],
                topology: wgpu::PrimitiveTopology,
                depth: Option<wgpu::DepthStencilState>| {
        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(label),
            layout: Some(layout),
            vertex: wgpu::VertexState { module: &shader, entry_point: Some(vs), compilation_options: Default::default(), buffers },
            primitive: wgpu::PrimitiveState { topology, cull_mode: None, ..Default::default() },
            depth_stencil: depth,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some(fs),
                compilation_options: Default::default(),
                targets: &[Some(target.clone())],
            }),
            multiview_mask: None,
            cache: None,
        })
    };
    let boxes_faces = make("box faces", &layout_boxes, "vs_box", "fs_faces", &[Some(VERTEX_LAYOUT), Some(BOX_INSTANCE_LAYOUT)], wgpu::PrimitiveTopology::TriangleList, depth(0, 0.0));
    let boxes_edges = make("box edges", &layout_boxes, "vs_box_edge", "fs_edges", &[Some(VERTEX_LAYOUT), Some(BOX_INSTANCE_LAYOUT)], wgpu::PrimitiveTopology::LineList, depth(0, 0.0));
    let lines = make("edge lines", &layout_geom, "vs_line", "fs_line", &[Some(LINE_LAYOUT)], wgpu::PrimitiveTopology::LineList, depth(0, 0.0));
    let surfels = make("surfels", &layout_geom, "vs_surfel", "fs_surfel", &[Some(QUAD_LAYOUT), Some(SURFEL_LAYOUT)], wgpu::PrimitiveTopology::TriangleList, depth(0, 0.0));
    let mesh = make("mesh", &layout_geom, "vs_mesh", "fs_mesh", &[Some(MESH_LAYOUT)], wgpu::PrimitiveTopology::TriangleList, depth(0, 0.0));

    let (fv, fi) = cube_faces();
    let (ev, ei) = cube_edges();
    let quad: [[f32; 2]; 4] = [[-1.0, -1.0], [1.0, -1.0], [1.0, 1.0], [-1.0, 1.0]];
    let quad_idx: [u16; 6] = [0, 1, 2, 0, 2, 3];
    let mk = |label: &str, bytes: &[u8], usage: wgpu::BufferUsages| {
        device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some(label), contents: bytes, usage })
    };
    let camera = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("camera"),
        size: std::mem::size_of::<CameraUniform>() as u64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let camera_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("camera bg"),
        layout: &camera_layout,
        entries: &[wgpu::BindGroupEntry { binding: 0, resource: camera.as_entire_binding() }],
    });
    render_state.renderer.write().callback_resources.insert(Resources {
        boxes_faces,
        boxes_edges,
        lines,
        surfels,
        mesh,
        face_vb: mk("face vb", bytemuck::cast_slice(&fv), wgpu::BufferUsages::VERTEX),
        face_ib: mk("face ib", bytemuck::cast_slice(&fi), wgpu::BufferUsages::INDEX),
        edge_vb: mk("edge vb", bytemuck::cast_slice(&ev), wgpu::BufferUsages::VERTEX),
        edge_ib: mk("edge ib", bytemuck::cast_slice(&ei), wgpu::BufferUsages::INDEX),
        quad_vb: mk("quad vb", bytemuck::cast_slice(&quad), wgpu::BufferUsages::VERTEX),
        quad_ib: mk("quad ib", bytemuck::cast_slice(&quad_idx), wgpu::BufferUsages::INDEX),
        camera,
        camera_bg,
        instance_layout,
        instance_buf: None,
        instance_bg: None,
        instance_cap: 0,
        box_buf: GrowBuffer::new("boxes", wgpu::BufferUsages::VERTEX),
        box_count: 0,
        box_version: u64::MAX,
        line_buf: GrowBuffer::new("lines", wgpu::BufferUsages::VERTEX),
        surfel_buf: GrowBuffer::new("surfels", wgpu::BufferUsages::VERTEX),
        mesh_vb: GrowBuffer::new("mesh vertices", wgpu::BufferUsages::VERTEX),
        mesh_ib: GrowBuffer::new("mesh indices", wgpu::BufferUsages::INDEX),
        geom_version: u64::MAX,
        draws: Vec::new(),
        draw_version: u64::MAX,
        show_lines: true,
        show_surfels: true,
        show_mesh: false,
    });
}

pub struct DrawCallback {
    pub scene: Arc<Mutex<Scene>>,
    pub view_proj: [f32; 16],
    pub light_dir: [f32; 4],
    /// Multiplier on every surfel's radius, applied in the shader.
    pub splat_scale: f32,
}

impl CallbackTrait for DrawCallback {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen: &ScreenDescriptor,
        _encoder: &mut wgpu::CommandEncoder,
        resources: &mut CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let Some(r) = resources.get_mut::<Resources>() else { return Vec::new() };
        queue.write_buffer(
            &r.camera,
            0,
            bytemuck::bytes_of(&CameraUniform { view_proj: self.view_proj, light_dir: self.light_dir, params: [self.splat_scale, 0.0, 0.0, 0.0] }),
        );
        let scene = self.scene.lock().unwrap();
        r.show_lines = scene.show_lines;
        r.show_surfels = scene.show_surfels;
        r.show_mesh = scene.show_mesh;

        if scene.box_version != r.box_version {
            r.box_buf.uploaded_bytes = 0; // boxes are rewritten whole
            r.box_buf.sync(device, queue, bytemuck::cast_slice(&scene.boxes));
            r.box_count = scene.boxes.len() as u32;
            r.box_version = scene.box_version;
        }
        if scene.draw_version != r.draw_version {
            let n = scene.instance_uniforms.len().max(1);
            if r.instance_buf.is_none() || n > r.instance_cap {
                let cap = n.max(256).next_power_of_two();
                let buf = device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("instance uniforms"),
                    size: cap as u64 * INSTANCE_STRIDE,
                    usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                r.instance_bg = Some(device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("instance bg"),
                    layout: &r.instance_layout,
                    entries: &[wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                            buffer: &buf,
                            offset: 0,
                            size: wgpu::BufferSize::new(std::mem::size_of::<InstanceUniform>() as u64),
                        }),
                    }],
                }));
                r.instance_buf = Some(buf);
                r.instance_cap = cap;
            }
            if !scene.instance_uniforms.is_empty() {
                let mut bytes = vec![0u8; scene.instance_uniforms.len() * INSTANCE_STRIDE as usize];
                for (i, u) in scene.instance_uniforms.iter().enumerate() {
                    let off = i * INSTANCE_STRIDE as usize;
                    bytes[off..off + std::mem::size_of::<InstanceUniform>()].copy_from_slice(bytemuck::bytes_of(u));
                }
                queue.write_buffer(r.instance_buf.as_ref().unwrap(), 0, &bytes);
            }
            r.draws = scene.draws.clone();
            r.draw_version = scene.draw_version;
        }
        if scene.geom_version != r.geom_version {
            r.line_buf.sync(device, queue, bytemuck::cast_slice(&scene.lines));
            r.surfel_buf.sync(device, queue, bytemuck::cast_slice(&scene.surfels));
            r.mesh_vb.sync(device, queue, bytemuck::cast_slice(&scene.mesh_vertices));
            r.mesh_ib.sync(device, queue, bytemuck::cast_slice(&scene.mesh_indices));
            r.geom_version = scene.geom_version;
        }
        Vec::new()
    }

    fn paint(&self, _info: eframe::egui::PaintCallbackInfo, pass: &mut wgpu::RenderPass<'static>, resources: &CallbackResources) {
        let Some(r) = resources.get::<Resources>() else { return };
        pass.set_bind_group(0, &r.camera_bg, &[]);
        if r.box_count > 0 {
            if let Some(bb) = &r.box_buf.buf {
                pass.set_vertex_buffer(1, bb.slice(..));
                pass.set_pipeline(&r.boxes_faces);
                pass.set_vertex_buffer(0, r.face_vb.slice(..));
                pass.set_index_buffer(r.face_ib.slice(..), wgpu::IndexFormat::Uint16);
                pass.draw_indexed(0..36, 0, 0..r.box_count);
                pass.set_pipeline(&r.boxes_edges);
                pass.set_vertex_buffer(0, r.edge_vb.slice(..));
                pass.set_index_buffer(r.edge_ib.slice(..), wgpu::IndexFormat::Uint16);
                pass.draw_indexed(0..24, 0, 0..r.box_count);
            }
        }
        let Some(ibg) = &r.instance_bg else { return };
        if r.show_mesh || r.draws.iter().any(|d| d.full) {
            if let (Some(vb), Some(ib)) = (&r.mesh_vb.buf, &r.mesh_ib.buf) {
                pass.set_pipeline(&r.mesh);
                pass.set_vertex_buffer(0, vb.slice(..));
                pass.set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
                let vstride = std::mem::size_of::<MeshVertexRaw>() as u64;
                for d in &r.draws {
                    if d.mesh.1 == 0 || !(r.show_mesh || d.full) {
                        continue;
                    }
                    let last_index_byte = (d.mesh.0 + d.mesh.1) as u64 * 4;
                    if last_index_byte > r.mesh_ib.uploaded_bytes || (d.mesh.2 as u64 + 1) * vstride > r.mesh_vb.uploaded_bytes {
                        continue;
                    }
                    pass.set_bind_group(1, ibg, &[d.instance * INSTANCE_STRIDE as u32]);
                    pass.draw_indexed(d.mesh.0..d.mesh.0 + d.mesh.1, d.mesh.2 as i32, 0..1);
                }
            }
        }
        let (Some(lb), Some(sb)) = (&r.line_buf.buf, &r.surfel_buf.buf) else { return };
        let surfel_bytes = std::mem::size_of::<SurfelRaw>() as u64;
        if r.show_surfels {
            pass.set_pipeline(&r.surfels);
            pass.set_vertex_buffer(0, r.quad_vb.slice(..));
            pass.set_index_buffer(r.quad_ib.slice(..), wgpu::IndexFormat::Uint16);
            for d in &r.draws {
                if d.surfels.1 == 0 || d.full {
                    continue;
                }
                let end = (d.surfels.0 + d.surfels.1) as u64 * surfel_bytes;
                if end > r.surfel_buf.uploaded_bytes {
                    continue;
                }
                pass.set_bind_group(1, ibg, &[d.instance * INSTANCE_STRIDE as u32]);
                pass.set_vertex_buffer(1, sb.slice(d.surfels.0 as u64 * surfel_bytes..end));
                pass.draw_indexed(0..6, 0, 0..d.surfels.1);
            }
        }
        if r.show_lines {
            pass.set_pipeline(&r.lines);
            pass.set_vertex_buffer(0, lb.slice(..));
            for d in &r.draws {
                if d.lines.1 == 0 {
                    continue;
                }
                if (d.lines.0 + d.lines.1) as u64 * 12 > r.line_buf.uploaded_bytes {
                    continue;
                }
                pass.set_bind_group(1, ibg, &[d.instance * INSTANCE_STRIDE as u32]);
                pass.draw(d.lines.0..d.lines.0 + d.lines.1, 0..1);
            }
        }
    }
}

/// Orbit camera, Z up, looking at `target` from `distance` away. All values
/// are relative to the scene origin chosen by the app (for f32 precision).
#[derive(Clone, Debug)]
pub struct Camera {
    pub target: glam::DVec3,
    pub distance: f64,
    pub yaw: f32,
    pub pitch: f32,
    pub fov_y: f32,
}

impl Default for Camera {
    fn default() -> Self {
        Self { target: glam::DVec3::ZERO, distance: 1000.0, yaw: 0.8, pitch: 0.5, fov_y: 45f32.to_radians() }
    }
}

impl Camera {
    pub fn eye(&self) -> glam::DVec3 {
        let (sy, cy) = (self.yaw as f64).sin_cos();
        let (sp, cp) = (self.pitch as f64).sin_cos();
        self.target + glam::DVec3::new(cp * cy, cp * sy, sp) * self.distance
    }
    pub fn view_proj(&self, aspect: f32) -> glam::Mat4 {
        let eye = self.eye().as_vec3();
        let target = self.target.as_vec3();
        let f = (target - eye).normalize();
        let s = f.cross(glam::Vec3::Z).normalize();
        let u = s.cross(f);
        let view = glam::Mat4::from_cols_array(&[
            s.x, u.x, -f.x, 0.0,
            s.y, u.y, -f.y, 0.0,
            s.z, u.z, -f.z, 0.0,
            -s.dot(eye), -u.dot(eye), f.dot(eye), 1.0,
        ]);
        let near = ((self.distance * 0.002) as f32).max(1e-3);
        let far = (self.distance * 20.0) as f32;
        let fy = 1.0 / (self.fov_y * 0.5).tan();
        let a = aspect.max(0.01);
        let proj = glam::Mat4::from_cols_array(&[
            fy / a, 0.0, 0.0, 0.0,
            0.0, fy, 0.0, 0.0,
            0.0, 0.0, far / (near - far), -1.0,
            0.0, 0.0, near * far / (near - far), 0.0,
        ]);
        proj * view
    }
    pub fn light_dir(&self) -> [f32; 4] {
        let d = (self.eye() - self.target).normalize() + glam::DVec3::Z * 0.6 + glam::DVec3::X * 0.3;
        let d = d.normalize().as_vec3();
        [d.x, d.y, d.z, 0.0]
    }
    pub fn pan(&mut self, dx: f32, dy: f32, viewport_height: f32) {
        let eye = self.eye();
        let fwd = (self.target - eye).normalize();
        let right = fwd.cross(glam::DVec3::Z).normalize();
        let up = right.cross(fwd).normalize();
        let world_per_point = 2.0 * self.distance * (self.fov_y as f64 * 0.5).tan() / viewport_height.max(1.0) as f64;
        self.target -= right * (dx as f64 * world_per_point);
        self.target += up * (dy as f64 * world_per_point);
    }
    pub fn fit(&mut self, center: glam::DVec3, radius: f64) {
        self.target = center;
        let r = radius.max(1e-6);
        self.distance = r / (self.fov_y as f64 * 0.5).tan() * 1.15;
    }
}
