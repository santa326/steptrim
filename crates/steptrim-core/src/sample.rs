//! Body sampling without a kernel: edge polylines and surface splats
//! (surfels) per definition, read straight from the B-rep entities.
//!
//! Edges are sampled exactly (lines from their vertices, arcs and splines
//! by evaluation). Faces are sampled on a grid in the surface's own
//! parameter space and trimmed by an even-odd test against the face loops
//! projected into that space, so no triangulation is needed; a surfel is an
//! oriented disc the renderer draws with a Gaussian footprint. Analytic
//! surfaces (plane, cylinder, cone, sphere, torus) are trimmed this way;
//! B-spline surfaces are sampled untrimmed over their knot range.

use crate::attr::{parse_attrs, parse_complex, Value};
use crate::geom::{axis2_placement, parse_coords, DefBounds, Transform};
use crate::scan::{Entity, EntityTable};
use crate::structure::ProductTree;
use rayon::prelude::*;
use std::collections::HashMap;
use std::f64::consts::PI;

#[derive(Clone, Copy, Debug)]
pub struct Surfel {
    pub pos: [f32; 3],
    pub nrm: [f32; 3],
    pub radius: f32,
}

#[derive(Clone, Copy, Debug)]
pub struct MeshVertex {
    pub pos: [f32; 3],
    pub nrm: [f32; 3],
}

#[derive(Clone, Debug, Default)]
pub struct DefGeometry {
    /// Line segments as consecutive point pairs, in the definition's frame.
    pub lines: Vec<[f32; 3]>,
    pub surfels: Vec<Surfel>,
    /// Triangle mesh: planar faces triangulated exactly (holes included),
    /// curved faces as grid quads inside their loops, B-spline faces as
    /// untrimmed grids. Indices are into `mesh_vertices`.
    pub mesh_vertices: Vec<MeshVertex>,
    pub mesh_indices: Vec<u32>,
    pub faces: u32,
    pub faces_sampled: u32,
    pub faces_meshed: u32,
    pub edges: u32,
    /// Sample spacing used, in file units.
    pub spacing: f32,
}

#[derive(Clone, Debug)]
pub struct SampleConfig {
    /// Surfel spacing = part box diagonal / this.
    pub samples_across: f64,
    pub max_surfels_per_face: usize,
    pub max_surfels_per_def: usize,
    pub max_lines_per_def: usize,
    /// Segments for a full circle.
    pub arc_segments_full: usize,
    pub spline_curve_samples: usize,
    pub spline_surface_grid: usize,
}

impl Default for SampleConfig {
    fn default() -> Self {
        Self {
            samples_across: 56.0,
            max_surfels_per_face: 3000,
            max_surfels_per_def: 150_000,
            max_lines_per_def: 400_000,
            arc_segments_full: 24,
            spline_curve_samples: 16,
            spline_surface_grid: 14,
        }
    }
}

// ---------- small vector helpers ----------

type V3 = [f64; 3];

fn sub(a: V3, b: V3) -> V3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
fn add(a: V3, b: V3) -> V3 {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}
fn scale(a: V3, s: f64) -> V3 {
    [a[0] * s, a[1] * s, a[2] * s]
}
fn dot(a: V3, b: V3) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
fn cross(a: V3, b: V3) -> V3 {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}
fn norm(a: V3) -> f64 {
    dot(a, a).sqrt()
}
fn normalize(a: V3) -> V3 {
    let n = norm(a);
    if n < 1e-300 {
        [0.0, 0.0, 1.0]
    } else {
        scale(a, 1.0 / n)
    }
}
fn f32v(a: V3) -> [f32; 3] {
    [a[0] as f32, a[1] as f32, a[2] as f32]
}

/// Frame of a placement: origin and unit x, y, z axes.
#[derive(Clone, Copy, Debug)]
struct Frame {
    c: V3,
    x: V3,
    y: V3,
    z: V3,
}

impl Frame {
    fn from_transform(t: &Transform) -> Frame {
        Frame {
            c: t.t,
            x: [t.m[0][0], t.m[1][0], t.m[2][0]],
            y: [t.m[0][1], t.m[1][1], t.m[2][1]],
            z: [t.m[0][2], t.m[1][2], t.m[2][2]],
        }
    }
    fn local(&self, p: V3) -> V3 {
        let d = sub(p, self.c);
        [dot(d, self.x), dot(d, self.y), dot(d, self.z)]
    }
    fn world(&self, u: f64, v: f64, w: f64) -> V3 {
        add(add(add(self.c, scale(self.x, u)), scale(self.y, v)), scale(self.z, w))
    }
}

// ---------- entity access ----------

struct Ctx<'a> {
    data: &'a [u8],
    table: &'a EntityTable,
    cfg: &'a SampleConfig,
    kinds: &'a Kinds,
}

/// Per-kind classification by whole token, computed once per file.
#[allow(dead_code)]
struct Kinds {
    face: Vec<bool>,
    bound: Vec<bool>,
    edge_loop: Vec<bool>,
    vertex_loop: Vec<bool>,
    poly_loop: Vec<bool>,
    oriented_edge: Vec<bool>,
    edge_curve: Vec<bool>,
    line: Vec<bool>,
    circle: Vec<bool>,
    ellipse: Vec<bool>,
    bspline_curve: Vec<bool>,
    polyline: Vec<bool>,
    trimmed: Vec<bool>,
    surface_curve: Vec<bool>,
    plane: Vec<bool>,
    cylinder: Vec<bool>,
    cone: Vec<bool>,
    sphere: Vec<bool>,
    torus: Vec<bool>,
    bspline_surface: Vec<bool>,
    stop: Vec<bool>,
}

fn classify(table: &EntityTable) -> Kinds {
    let names = &table.type_names;
    let has = |n: &str, tok: &str| n.split(['_', '+']).any(|t| t == tok);
    let exact_any = |n: &str, names_: &[&str]| n.split('+').any(|p| names_.contains(&p));
    let mk = |f: &dyn Fn(&str) -> bool| names.iter().map(|n| f(n)).collect::<Vec<bool>>();
    const STOP: &[&str] = &[
        "CONTEXT", "UNIT", "STYLE", "STYLED", "PRESENTATION", "COLOUR", "LAYER", "PCURVE", "DEFINITIONAL", "PRODUCT",
        "PROPERTY", "DOCUMENT", "APPLICATION", "RELATIONSHIP", "MEASURE", "ORGANIZATION", "PERSON", "DATE", "APPROVAL",
        "SECURITY", "CATEGORY", "MAPPED", "MAP", "ANNOTATION", "DRAUGHTING", "CAMERA", "TEXT",
    ];
    Kinds {
        face: mk(&|n| exact_any(n, &["ADVANCED_FACE", "FACE_SURFACE"])),
        bound: mk(&|n| exact_any(n, &["FACE_BOUND", "FACE_OUTER_BOUND"])),
        edge_loop: mk(&|n| exact_any(n, &["EDGE_LOOP"])),
        vertex_loop: mk(&|n| exact_any(n, &["VERTEX_LOOP"])),
        poly_loop: mk(&|n| exact_any(n, &["POLY_LOOP"])),
        oriented_edge: mk(&|n| exact_any(n, &["ORIENTED_EDGE"])),
        edge_curve: mk(&|n| exact_any(n, &["EDGE_CURVE"])),
        line: mk(&|n| exact_any(n, &["LINE"])),
        circle: mk(&|n| exact_any(n, &["CIRCLE"])),
        ellipse: mk(&|n| exact_any(n, &["ELLIPSE"])),
        bspline_curve: mk(&|n| exact_any(n, &["B_SPLINE_CURVE_WITH_KNOTS", "B_SPLINE_CURVE", "BEZIER_CURVE", "QUASI_UNIFORM_CURVE", "UNIFORM_CURVE"])),
        polyline: mk(&|n| exact_any(n, &["POLYLINE"])),
        trimmed: mk(&|n| exact_any(n, &["TRIMMED_CURVE"])),
        surface_curve: mk(&|n| exact_any(n, &["SURFACE_CURVE", "SEAM_CURVE", "INTERSECTION_CURVE"])),
        plane: mk(&|n| exact_any(n, &["PLANE"])),
        cylinder: mk(&|n| exact_any(n, &["CYLINDRICAL_SURFACE"])),
        cone: mk(&|n| exact_any(n, &["CONICAL_SURFACE"])),
        sphere: mk(&|n| exact_any(n, &["SPHERICAL_SURFACE"])),
        torus: mk(&|n| exact_any(n, &["TOROIDAL_SURFACE", "DEGENERATE_TOROIDAL_SURFACE"])),
        bspline_surface: mk(&|n| exact_any(n, &["B_SPLINE_SURFACE_WITH_KNOTS", "B_SPLINE_SURFACE", "BEZIER_SURFACE", "QUASI_UNIFORM_SURFACE", "UNIFORM_SURFACE"])),
        stop: mk(&|n| STOP.iter().any(|s| has(n, s))),
    }
}

impl<'a> Ctx<'a> {
    fn entity(&self, id: u32) -> Option<&'a Entity> {
        self.table.get(id)
    }
    /// Attributes of a simple instance, or of the named partial record of
    /// a complex one (`None` partial: the first record that matches any of
    /// the given names).
    fn attrs(&self, id: u32, partials: &[&[u8]]) -> Option<Vec<Value<'a>>> {
        let e = self.entity(id)?;
        if !e.complex {
            return parse_attrs(e.attrs(self.data)).ok();
        }
        let parts = parse_complex(e.attrs(self.data)).ok()?;
        parts.into_iter().find(|(n, _)| partials.contains(n)).map(|(_, a)| a)
    }
    fn complex_parts(&self, id: u32) -> Option<Vec<(&'a [u8], Vec<Value<'a>>)>> {
        let e = self.entity(id)?;
        if e.complex {
            parse_complex(e.attrs(self.data)).ok()
        } else {
            Some(vec![(type_bytes(self.table.kind_name(e.kind)), parse_attrs(e.attrs(self.data)).ok()?)])
        }
    }
    fn is(&self, id: u32, class: &[bool]) -> bool {
        self.entity(id).map(|e| class[e.kind as usize]).unwrap_or(false)
    }
    fn point(&self, id: u32) -> Option<V3> {
        let e = self.entity(id)?;
        let (p, d) = parse_coords(e.attrs(self.data))?;
        if d == 3 {
            Some(p)
        } else {
            None
        }
    }
    fn vertex(&self, id: u32) -> Option<V3> {
        let a = self.attrs(id, &[b"VERTEX_POINT"])?;
        self.point(a.get(1)?.as_id()?)
    }
    fn frame(&self, placement: u32) -> Option<Frame> {
        axis2_placement(self.data, self.table, placement).map(|t| Frame::from_transform(&t))
    }
}

fn type_bytes(s: &str) -> &[u8] {
    s.as_bytes()
}

fn num(v: Option<&Value>) -> Option<f64> {
    v.and_then(|x| x.as_f64())
}

fn flag(v: Option<&Value>) -> bool {
    matches!(v, Some(Value::Enum(b"T")))
}

fn ids(v: Option<&Value>) -> Vec<u32> {
    v.and_then(|x| x.as_list()).map(|l| l.iter().filter_map(|x| x.as_id()).collect()).unwrap_or_default()
}

fn nums(v: Option<&Value>) -> Vec<f64> {
    v.and_then(|x| x.as_list()).map(|l| l.iter().filter_map(|x| x.as_f64()).collect()).unwrap_or_default()
}

// ---------- B-splines ----------

struct BsplineCurve {
    degree: usize,
    knots: Vec<f64>,
    /// Homogeneous control points (x w, y w, z w, w).
    ctrl: Vec<[f64; 4]>,
}

fn expand_knots(knots: &[f64], mults: &[f64]) -> Vec<f64> {
    let mut out = Vec::new();
    for (k, m) in knots.iter().zip(mults) {
        for _ in 0..(*m as usize).max(1) {
            out.push(*k);
        }
    }
    out
}

/// De Boor evaluation of a homogeneous control polygon.
fn de_boor(degree: usize, knots: &[f64], ctrl: &[[f64; 4]], u: f64) -> [f64; 4] {
    let n = ctrl.len();
    if n == 0 {
        return [0.0; 4];
    }
    if knots.len() < n + degree + 1 || degree == 0 {
        return ctrl[0];
    }
    let lo = knots[degree];
    let hi = knots[n];
    let u = u.clamp(lo.min(hi), hi.max(lo));
    // knot span k with knots[k] <= u < knots[k+1], within [degree, n-1]
    let mut k = degree;
    while k < n - 1 && u >= knots[k + 1] {
        k += 1;
    }
    let mut d: Vec<[f64; 4]> = (0..=degree).map(|j| ctrl[j + k - degree]).collect();
    for r in 1..=degree {
        for j in (r..=degree).rev() {
            let i = j + k - degree;
            let denom = knots[i + degree - r + 1] - knots[i];
            let alpha = if denom.abs() < 1e-300 { 0.0 } else { (u - knots[i]) / denom };
            for c in 0..4 {
                d[j][c] = (1.0 - alpha) * d[j - 1][c] + alpha * d[j][c];
            }
        }
    }
    d[degree]
}

fn dehomog(h: [f64; 4]) -> V3 {
    if h[3].abs() < 1e-300 {
        [h[0], h[1], h[2]]
    } else {
        [h[0] / h[3], h[1] / h[3], h[2] / h[3]]
    }
}

impl BsplineCurve {
    fn parse(ctx: &Ctx, id: u32) -> Option<BsplineCurve> {
        let parts = ctx.complex_parts(id)?;
        let find = |name: &[u8]| parts.iter().find(|(n, _)| *n == name).map(|(_, a)| a);
        // Simple B_SPLINE_CURVE_WITH_KNOTS has all attributes inline:
        // (name, degree, control_points, form, closed, selfint, mults, knots, spec)
        let (degree, ctrl_ids, mults, knots, weights) = if let Some(a) = find(b"B_SPLINE_CURVE_WITH_KNOTS").filter(|a| a.len() >= 9) {
            (num(a.get(1))? as usize, ids(a.get(2)), nums(a.get(6)), nums(a.get(7)), Vec::new())
        } else {
            // complex: B_SPLINE_CURVE(degree, ctrl, form, closed, selfint) +
            // B_SPLINE_CURVE_WITH_KNOTS(mults, knots, spec) + RATIONAL_B_SPLINE_CURVE(weights)
            let base = find(b"B_SPLINE_CURVE")?;
            let wk = find(b"B_SPLINE_CURVE_WITH_KNOTS");
            let degree = num(base.first())? as usize;
            let ctrl_ids = ids(base.get(1));
            let (mults, knots) = match wk {
                Some(w) => (nums(w.first()), nums(w.get(1))),
                None => {
                    // Bezier / uniform forms: build a uniform clamped knot vector.
                    let n = ctrl_ids.len();
                    let inner = n.saturating_sub(degree + 1);
                    let mut knots = vec![0.0];
                    let mut mults = vec![(degree + 1) as f64];
                    for i in 0..inner {
                        knots.push((i + 1) as f64);
                        mults.push(1.0);
                    }
                    knots.push((inner + 1) as f64);
                    mults.push((degree + 1) as f64);
                    (mults, knots)
                }
            };
            let weights = find(b"RATIONAL_B_SPLINE_CURVE").map(|w| nums(w.first())).unwrap_or_default();
            (degree, ctrl_ids, mults, knots, weights)
        };
        let mut ctrl = Vec::with_capacity(ctrl_ids.len());
        for (i, cid) in ctrl_ids.iter().enumerate() {
            let p = ctx.point(*cid)?;
            let w = weights.get(i).copied().unwrap_or(1.0);
            ctrl.push([p[0] * w, p[1] * w, p[2] * w, w]);
        }
        if ctrl.is_empty() {
            return None;
        }
        Some(BsplineCurve { degree, knots: expand_knots(&knots, &mults), ctrl })
    }
    fn range(&self) -> (f64, f64) {
        let n = self.ctrl.len();
        if self.knots.len() >= n + self.degree + 1 {
            (self.knots[self.degree], self.knots[n])
        } else {
            (0.0, 1.0)
        }
    }
    fn eval(&self, u: f64) -> V3 {
        dehomog(de_boor(self.degree, &self.knots, &self.ctrl, u))
    }
}

struct BsplineSurface {
    du: usize,
    dv: usize,
    uknots: Vec<f64>,
    vknots: Vec<f64>,
    /// ctrl[i][j], i along u.
    ctrl: Vec<Vec<[f64; 4]>>,
}

impl BsplineSurface {
    fn parse(ctx: &Ctx, id: u32) -> Option<BsplineSurface> {
        let parts = ctx.complex_parts(id)?;
        let find = |name: &[u8]| parts.iter().find(|(n, _)| *n == name).map(|(_, a)| a);
        let grid_ids = |v: Option<&Value>| -> Vec<Vec<u32>> {
            v.and_then(|x| x.as_list()).map(|rows| rows.iter().map(|r| ids(Some(r))).collect()).unwrap_or_default()
        };
        let grid_nums = |v: Option<&Value>| -> Vec<Vec<f64>> {
            v.and_then(|x| x.as_list()).map(|rows| rows.iter().map(|r| nums(Some(r))).collect()).unwrap_or_default()
        };
        // Simple: (name, u_degree, v_degree, control_points, form, u_closed, v_closed, selfint, u_mults, v_mults, u_knots, v_knots, spec)
        let (du, dv, cids, um, vm, uk, vk, weights) = if let Some(a) = find(b"B_SPLINE_SURFACE_WITH_KNOTS").filter(|a| a.len() >= 13) {
            (
                num(a.get(1))? as usize,
                num(a.get(2))? as usize,
                grid_ids(a.get(3)),
                nums(a.get(8)),
                nums(a.get(9)),
                nums(a.get(10)),
                nums(a.get(11)),
                Vec::new(),
            )
        } else {
            let base = find(b"B_SPLINE_SURFACE")?;
            let wk = find(b"B_SPLINE_SURFACE_WITH_KNOTS");
            let du = num(base.first())? as usize;
            let dv = num(base.get(1))? as usize;
            let cids = grid_ids(base.get(2));
            let (um, vm, uk, vk) = match wk {
                Some(w) => (nums(w.first()), nums(w.get(1)), nums(w.get(2)), nums(w.get(3))),
                None => {
                    let uniform = |n: usize, d: usize| {
                        let inner = n.saturating_sub(d + 1);
                        let mut knots = vec![0.0];
                        let mut mults = vec![(d + 1) as f64];
                        for i in 0..inner {
                            knots.push((i + 1) as f64);
                            mults.push(1.0);
                        }
                        knots.push((inner + 1) as f64);
                        mults.push((d + 1) as f64);
                        (mults, knots)
                    };
                    let nu = cids.len();
                    let nv = cids.first().map(|r| r.len()).unwrap_or(0);
                    let (um, uk) = uniform(nu, du);
                    let (vm, vk) = uniform(nv, dv);
                    (um, vm, uk, vk)
                }
            };
            let weights = find(b"RATIONAL_B_SPLINE_SURFACE").map(|w| grid_nums(w.first())).unwrap_or_default();
            (du, dv, cids, um, vm, uk, vk, weights)
        };
        let mut ctrl = Vec::with_capacity(cids.len());
        for (i, row) in cids.iter().enumerate() {
            let mut r = Vec::with_capacity(row.len());
            for (j, cid) in row.iter().enumerate() {
                let p = ctx.point(*cid)?;
                let w = weights.get(i).and_then(|wr| wr.get(j)).copied().unwrap_or(1.0);
                r.push([p[0] * w, p[1] * w, p[2] * w, w]);
            }
            ctrl.push(r);
        }
        if ctrl.is_empty() || ctrl[0].is_empty() {
            return None;
        }
        Some(BsplineSurface { du, dv, uknots: expand_knots(&uk, &um), vknots: expand_knots(&vk, &vm), ctrl })
    }
    fn range(&self) -> ((f64, f64), (f64, f64)) {
        let nu = self.ctrl.len();
        let nv = self.ctrl[0].len();
        let ur = if self.uknots.len() >= nu + self.du + 1 { (self.uknots[self.du], self.uknots[nu]) } else { (0.0, 1.0) };
        let vr = if self.vknots.len() >= nv + self.dv + 1 { (self.vknots[self.dv], self.vknots[nv]) } else { (0.0, 1.0) };
        (ur, vr)
    }
    fn eval(&self, u: f64, v: f64) -> V3 {
        // evaluate along v for each u-row, then along u
        let col: Vec<[f64; 4]> = self.ctrl.iter().map(|row| de_boor(self.dv, &self.vknots, row, v)).collect();
        dehomog(de_boor(self.du, &self.uknots, &col, u))
    }
}

// ---------- edges ----------

fn angle_in(f: &Frame, p: V3) -> f64 {
    let l = f.local(p);
    l[1].atan2(l[0])
}

/// Points along an arc of a circle/ellipse from angle a0 to a1 (signed sweep).
fn conic_points(f: &Frame, r1: f64, r2: f64, a0: f64, sweep: f64, n: usize, out: &mut Vec<V3>) {
    for i in 0..=n {
        let t = a0 + sweep * (i as f64 / n as f64);
        out.push(f.world(r1 * t.cos(), r2 * t.sin(), 0.0));
    }
}

impl<'a> Ctx<'a> {
    /// Polyline of an edge curve from its start vertex to its end vertex.
    fn edge_polyline(&self, edge: u32) -> Option<Vec<V3>> {
        let a = self.attrs(edge, &[b"EDGE_CURVE"])?;
        let sv = a.get(1)?.as_id()?;
        let ev = a.get(2)?.as_id()?;
        let geom = a.get(3)?.as_id()?;
        let same_sense = flag(a.get(4));
        let s = self.vertex(sv);
        let e = self.vertex(ev);
        let mut pts = self.curve_polyline(geom, s, e, same_sense, sv == ev, 0)?;
        if pts.len() < 2 {
            if let (Some(s), Some(e)) = (s, e) {
                pts = vec![s, e];
            }
        }
        Some(pts)
    }

    fn curve_polyline(&self, geom: u32, s: Option<V3>, e: Option<V3>, same_sense: bool, closed: bool, depth: u32) -> Option<Vec<V3>> {
        if depth > 4 {
            return None;
        }
        let ent = self.entity(geom)?;
        let k = ent.kind as usize;
        let kinds = self.kinds;
        if kinds.line[k] {
            return Some(vec![s?, e?]);
        }
        if kinds.circle[k] || kinds.ellipse[k] {
            let a = self.attrs(geom, &[b"CIRCLE", b"ELLIPSE"])?;
            let f = self.frame(a.get(1)?.as_id()?)?;
            let r1 = num(a.get(2))?.abs();
            let r2 = if kinds.ellipse[k] { num(a.get(3)).unwrap_or(r1).abs() } else { r1 };
            let full = self.cfg.arc_segments_full.max(8);
            let mut out = Vec::new();
            match (s, e) {
                (Some(s), Some(e)) if !closed => {
                    let a0 = angle_in(&f, s);
                    let a1 = angle_in(&f, e);
                    let mut sweep = a1 - a0;
                    if same_sense {
                        if sweep <= 1e-9 {
                            sweep += 2.0 * PI;
                        }
                    } else if sweep >= -1e-9 {
                        sweep -= 2.0 * PI;
                    }
                    let n = ((sweep.abs() / (2.0 * PI)) * full as f64).ceil().max(2.0) as usize;
                    conic_points(&f, r1, r2, a0, sweep, n, &mut out);
                    // snap the ends to the exact vertices
                    if let Some(first) = out.first_mut() {
                        *first = s;
                    }
                    if let Some(last) = out.last_mut() {
                        *last = e;
                    }
                }
                _ => {
                    let a0 = s.map(|p| angle_in(&f, p)).unwrap_or(0.0);
                    conic_points(&f, r1, r2, a0, 2.0 * PI, full, &mut out);
                }
            }
            return Some(out);
        }
        if kinds.bspline_curve[k] {
            let c = BsplineCurve::parse(self, geom)?;
            let (u0, u1) = c.range();
            let n = self.cfg.spline_curve_samples.max(4).max(c.ctrl.len().min(48));
            let mut out = Vec::with_capacity(n + 1);
            for i in 0..=n {
                out.push(c.eval(u0 + (u1 - u0) * i as f64 / n as f64));
            }
            if let (Some(s), Some(e)) = (s, e) {
                // orient the spline from start to end vertex
                let d_direct = norm(sub(out[0], s)) + norm(sub(*out.last().unwrap(), e));
                let d_rev = norm(sub(out[0], e)) + norm(sub(*out.last().unwrap(), s));
                if d_rev < d_direct {
                    out.reverse();
                }
            }
            return Some(out);
        }
        if kinds.polyline[k] {
            let a = self.attrs(geom, &[b"POLYLINE"])?;
            let pts: Vec<V3> = ids(a.get(1)).iter().filter_map(|&p| self.point(p)).collect();
            return Some(pts);
        }
        if kinds.surface_curve[k] {
            let a = self.attrs(geom, &[b"SURFACE_CURVE", b"SEAM_CURVE", b"INTERSECTION_CURVE"])?;
            let basis = a.get(1)?.as_id()?;
            return self.curve_polyline(basis, s, e, same_sense, closed, depth + 1);
        }
        if kinds.trimmed[k] {
            // TRIMMED_CURVE(name, basis, trim_1, trim_2, sense_agreement, master)
            let a = self.attrs(geom, &[b"TRIMMED_CURVE"])?;
            let basis = a.get(1)?.as_id()?;
            let trim_point = |v: Option<&Value>| -> Option<V3> {
                let l = v?.as_list()?;
                l.iter().find_map(|x| x.as_id()).and_then(|id| self.point(id))
            };
            let trim_param = |v: Option<&Value>| -> Option<f64> {
                let l = v?.as_list()?;
                l.iter().find_map(|x| match x {
                    Value::Typed(_, inner) => inner.as_f64(),
                    other => other.as_f64(),
                })
            };
            let bs = self.entity(basis)?;
            let bk = bs.kind as usize;
            if kinds.circle[bk] || kinds.ellipse[bk] {
                if let (Some(t0), Some(t1)) = (trim_param(a.get(2)), trim_param(a.get(3))) {
                    let ba = self.attrs(basis, &[b"CIRCLE", b"ELLIPSE"])?;
                    let f = self.frame(ba.get(1)?.as_id()?)?;
                    let r1 = num(ba.get(2))?.abs();
                    let r2 = if kinds.ellipse[bk] { num(ba.get(3)).unwrap_or(r1).abs() } else { r1 };
                    // parameters are angles, possibly in degrees; detect
                    let (mut a0, mut a1) = (t0, t1);
                    if a0.abs() > 2.0 * PI + 1e-6 || a1.abs() > 2.0 * PI + 1e-6 {
                        a0 = a0.to_radians();
                        a1 = a1.to_radians();
                    }
                    let mut sweep = a1 - a0;
                    if !flag(a.get(4)) {
                        sweep = -sweep;
                    }
                    let n = ((sweep.abs() / (2.0 * PI)) * self.cfg.arc_segments_full as f64).ceil().max(2.0) as usize;
                    let mut out = Vec::new();
                    conic_points(&f, r1, r2, a0, sweep, n, &mut out);
                    return Some(out);
                }
            }
            let s2 = trim_point(a.get(2)).or(s);
            let e2 = trim_point(a.get(3)).or(e);
            return self.curve_polyline(basis, s2, e2, same_sense, closed, depth + 1);
        }
        None
    }
}

// ---------- faces ----------

/// A loop as a 3D polyline (closed; last point joins the first).
struct Loop3 {
    pts: Vec<V3>,
}

impl<'a> Ctx<'a> {
    /// Loops of a face, with edge lines collected once per edge curve.
    fn face_loops(&self, face: u32, edge_cache: &mut HashMap<u32, Vec<V3>>, lines: &mut Vec<[f32; 3]>, edge_count: &mut u32) -> (Vec<Loop3>, Option<u32>, bool) {
        let mut loops = Vec::new();
        let Some(a) = self.attrs(face, &[b"ADVANCED_FACE", b"FACE_SURFACE"]) else { return (loops, None, true) };
        let surface = a.get(2).and_then(|v| v.as_id());
        let same_sense = flag(a.get(3));
        for bound in ids(a.get(1)) {
            let Some(b) = self.attrs(bound, &[b"FACE_OUTER_BOUND", b"FACE_BOUND"]) else { continue };
            let Some(lp) = b.get(1).and_then(|v| v.as_id()) else { continue };
            let orientation = flag(b.get(2));
            let mut pts: Vec<V3> = Vec::new();
            if self.is(lp, &self.kinds.edge_loop) {
                let Some(la) = self.attrs(lp, &[b"EDGE_LOOP"]) else { continue };
                for oe in ids(la.get(1)) {
                    let Some(oa) = self.attrs(oe, &[b"ORIENTED_EDGE"]) else { continue };
                    let Some(edge) = oa.get(3).and_then(|v| v.as_id()) else { continue };
                    let fwd = flag(oa.get(4));
                    let poly = match edge_cache.get(&edge) {
                        Some(p) => p.clone(),
                        None => {
                            let p = self.edge_polyline(edge).unwrap_or_default();
                            if p.len() >= 2 && lines.len() < self.cfg.max_lines_per_def {
                                for w in p.windows(2) {
                                    lines.push(f32v(w[0]));
                                    lines.push(f32v(w[1]));
                                }
                                *edge_count += 1;
                            }
                            edge_cache.insert(edge, p.clone());
                            p
                        }
                    };
                    if poly.is_empty() {
                        continue;
                    }
                    let iter: Box<dyn Iterator<Item = &V3>> = if fwd { Box::new(poly.iter()) } else { Box::new(poly.iter().rev()) };
                    for (i, p) in iter.enumerate() {
                        if i == 0 && !pts.is_empty() {
                            continue; // shared vertex with the previous edge
                        }
                        pts.push(*p);
                    }
                }
            } else if self.is(lp, &self.kinds.poly_loop) {
                if let Some(la) = self.attrs(lp, &[b"POLY_LOOP"]) {
                    pts = ids(la.get(1)).iter().filter_map(|&p| self.point(p)).collect();
                    if pts.len() >= 2 && lines.len() < self.cfg.max_lines_per_def {
                        for i in 0..pts.len() {
                            lines.push(f32v(pts[i]));
                            lines.push(f32v(pts[(i + 1) % pts.len()]));
                        }
                    }
                }
            } else if self.is(lp, &self.kinds.vertex_loop) {
                if let Some(la) = self.attrs(lp, &[b"VERTEX_LOOP"]) {
                    if let Some(p) = la.get(1).and_then(|v| v.as_id()).and_then(|v| self.vertex(v)) {
                        pts.push(p);
                    }
                }
            }
            if pts.len() >= 2 && norm(sub(pts[0], *pts.last().unwrap())) < 1e-9 {
                pts.pop();
            }
            let _ = orientation;
            if !pts.is_empty() {
                loops.push(Loop3 { pts });
            }
        }
        (loops, surface, same_sense)
    }
}

/// Even-odd test: is (u, v) inside the union of loop polygons, counting
/// crossings of the ray v' > v. With `period`, u is periodic.
fn inside(loops: &[Vec<[f64; 2]>], u: f64, v: f64, period: Option<f64>) -> bool {
    let mut crossings = 0u32;
    let shifts: [f64; 3] = match period {
        Some(p) => [0.0, p, -p],
        None => [0.0, 0.0, 0.0],
    };
    let nshift = if period.is_some() { 3 } else { 1 };
    for lp in loops {
        let n = lp.len();
        if n < 2 {
            continue;
        }
        // A loop that winds around the periodic axis is open in the unrolled
        // plane; close it through the seam to its shifted first point so
        // each crossing is counted exactly once.
        let closing_shift = match period {
            Some(p) => ((lp[n - 1][0] - lp[0][0]) / p).round() * p,
            None => 0.0,
        };
        for i in 0..n {
            let a = lp[i];
            let mut b = lp[(i + 1) % n];
            if i == n - 1 {
                b[0] += closing_shift;
            }
            for &sh in &shifts[..nshift] {
                let (au, av) = (a[0] + sh, a[1]);
                let (bu, bv) = (b[0] + sh, b[1]);
                if (au <= u) != (bu <= u) {
                    let t = (u - au) / (bu - au);
                    let vc = av + t * (bv - av);
                    if vc > v {
                        crossings += 1;
                    }
                }
            }
        }
    }
    crossings % 2 == 1
}

/// Unwrap a periodic coordinate along a closed loop so consecutive points
/// differ by less than half a period.
fn unwrap_loop(lp: &mut [[f64; 2]], period: f64) {
    for i in 1..lp.len() {
        let mut d = lp[i][0] - lp[i - 1][0];
        while d > period / 2.0 {
            lp[i][0] -= period;
            d -= period;
        }
        while d < -period / 2.0 {
            lp[i][0] += period;
            d += period;
        }
    }
}

fn push_surfel(out: &mut Vec<Surfel>, p: V3, n: V3, r: f64) {
    out.push(Surfel { pos: f32v(p), nrm: f32v(normalize(n)), radius: r as f32 });
}

/// Sample a parametric face: grid over the loops' parameter bounds, kept
/// where the even-odd test says inside. `map(u, v)` gives position and normal.
#[allow(clippy::too_many_arguments)]
fn sample_param(
    loops: &[Vec<[f64; 2]>],
    period_u: Option<f64>,
    du: f64,
    dv: f64,
    map: &dyn Fn(f64, f64) -> (V3, V3),
    radius: f64,
    max_count: usize,
    out: &mut Vec<Surfel>,
) -> bool {
    let (mut umin, mut umax, mut vmin, mut vmax) = (f64::INFINITY, f64::NEG_INFINITY, f64::INFINITY, f64::NEG_INFINITY);
    for lp in loops {
        for p in lp {
            umin = umin.min(p[0]);
            umax = umax.max(p[0]);
            vmin = vmin.min(p[1]);
            vmax = vmax.max(p[1]);
        }
    }
    if !umin.is_finite() || du <= 0.0 || dv <= 0.0 {
        return false;
    }
    let mut du = du;
    let mut dv = dv;
    let mut grow = 1.0;
    let est = ((umax - umin) / du + 1.0) * ((vmax - vmin) / dv + 1.0);
    if est > max_count as f64 {
        grow = (est / max_count as f64).sqrt();
        du *= grow;
        dv *= grow;
    }
    // The disc radius grows with the spacing so coarser grids still close up.
    let r = radius * grow;
    let nu = ((umax - umin) / du).ceil() as usize + 1;
    let nv = ((vmax - vmin) / dv).ceil() as usize + 1;
    let mut any = false;
    // offset the grid by half a step so points do not sit exactly on loop edges
    for i in 0..nu {
        let u = umin + (i as f64 + 0.5) * du;
        if u > umax {
            break;
        }
        for j in 0..nv {
            let v = vmin + (j as f64 + 0.5) * dv;
            if v > vmax {
                break;
            }
            if inside(loops, u, v, period_u) {
                let (p, n) = map(u, v);
                push_surfel(out, p, n, r * 0.9);
                any = true;
            }
        }
    }
    any
}

fn push_mesh_vertex(g: &mut DefGeometry, p: V3, n: V3) -> u32 {
    g.mesh_vertices.push(MeshVertex { pos: f32v(p), nrm: f32v(normalize(n)) });
    (g.mesh_vertices.len() - 1) as u32
}

/// Mesh a parametric face: vertices on a grid over the loops' parameter
/// bounds, one quad (two triangles) per cell whose four corners are inside.
#[allow(clippy::too_many_arguments)]
fn mesh_param(
    loops: &[Vec<[f64; 2]>],
    period_u: Option<f64>,
    du: f64,
    dv: f64,
    map: &dyn Fn(f64, f64) -> (V3, V3),
    max_cells: usize,
    g: &mut DefGeometry,
) -> bool {
    let (mut umin, mut umax, mut vmin, mut vmax) = (f64::INFINITY, f64::NEG_INFINITY, f64::INFINITY, f64::NEG_INFINITY);
    for lp in loops {
        for p in lp {
            umin = umin.min(p[0]);
            umax = umax.max(p[0]);
            vmin = vmin.min(p[1]);
            vmax = vmax.max(p[1]);
        }
    }
    if !umin.is_finite() || du <= 0.0 || dv <= 0.0 || umax - umin <= 0.0 || vmax - vmin <= 0.0 {
        return false;
    }
    let mut du = du;
    let mut dv = dv;
    let est = ((umax - umin) / du + 1.0) * ((vmax - vmin) / dv + 1.0);
    if est > max_cells as f64 {
        let f = (est / max_cells as f64).sqrt();
        du *= f;
        dv *= f;
    }
    let nu = ((umax - umin) / du).ceil().max(1.0) as usize;
    let nv = ((vmax - vmin) / dv).ceil().max(1.0) as usize;
    let du = (umax - umin) / nu as f64;
    let dv = (vmax - vmin) / nv as f64;
    let stride = nv + 1;
    let mut index: Vec<u32> = vec![u32::MAX; (nu + 1) * stride];
    let mut inside_flag: Vec<bool> = vec![false; (nu + 1) * stride];
    for i in 0..=nu {
        for j in 0..=nv {
            // pull the test point a hair inward so points on the loop count as inside
            let u = umin + i as f64 * du;
            let v = vmin + j as f64 * dv;
            let uu = if i == 0 { u + du * 1e-3 } else if i == nu { u - du * 1e-3 } else { u };
            let vv = if j == 0 { v + dv * 1e-3 } else if j == nv { v - dv * 1e-3 } else { v };
            inside_flag[i * stride + j] = inside(loops, uu, vv, period_u);
        }
    }
    let mut any = false;
    for i in 0..nu {
        for j in 0..nv {
            let c = [i * stride + j, (i + 1) * stride + j, (i + 1) * stride + j + 1, i * stride + j + 1];
            if !c.iter().all(|&k| inside_flag[k]) {
                continue;
            }
            let mut ids = [0u32; 4];
            for (q, &k) in c.iter().enumerate() {
                if index[k] == u32::MAX {
                    let (ii, jj) = (k / stride, k % stride);
                    let (p, n) = map(umin + ii as f64 * du, vmin + jj as f64 * dv);
                    index[k] = push_mesh_vertex(g, p, n);
                }
                ids[q] = index[k];
            }
            g.mesh_indices.extend_from_slice(&[ids[0], ids[1], ids[2], ids[0], ids[2], ids[3]]);
            any = true;
        }
    }
    any
}

/// Exact triangulation of a planar face: the largest loop is the outer
/// ring, the others are holes.
fn mesh_planar(loops2: &[Vec<[f64; 2]>], frame: &Frame, g: &mut DefGeometry) -> bool {
    let area = |lp: &Vec<[f64; 2]>| -> f64 {
        let n = lp.len();
        (0..n).map(|i| {
            let a = lp[i];
            let b = lp[(i + 1) % n];
            a[0] * b[1] - b[0] * a[1]
        }).sum::<f64>() * 0.5
    };
    let valid: Vec<&Vec<[f64; 2]>> = loops2.iter().filter(|l| l.len() >= 3).collect();
    if valid.is_empty() {
        return false;
    }
    let outer_idx = (0..valid.len()).max_by(|&a, &b| area(valid[a]).abs().partial_cmp(&area(valid[b]).abs()).unwrap()).unwrap();
    let mut data: Vec<[f64; 2]> = Vec::new();
    let mut holes: Vec<u32> = Vec::new();
    data.extend_from_slice(valid[outer_idx]);
    for (i, lp) in valid.iter().enumerate() {
        if i == outer_idx {
            continue;
        }
        holes.push(data.len() as u32);
        data.extend_from_slice(lp);
    }
    let mut tri: Vec<u32> = Vec::new();
    let mut ec = earcut::Earcut::<f64>::new();
    ec.earcut(data.iter().copied(), &holes, &mut tri);
    if tri.is_empty() {
        return false;
    }
    let base = g.mesh_vertices.len() as u32;
    for p in &data {
        push_mesh_vertex(g, frame.world(p[0], p[1], 0.0), frame.z);
    }
    g.mesh_indices.extend(tri.iter().map(|&t| base + t));
    true
}

/// Normal of a 3D loop by Newell's method.
fn loop_normal(pts: &[V3]) -> V3 {
    let mut n = [0.0; 3];
    for i in 0..pts.len() {
        let a = pts[i];
        let b = pts[(i + 1) % pts.len()];
        n[0] += (a[1] - b[1]) * (a[2] + b[2]);
        n[1] += (a[2] - b[2]) * (a[0] + b[0]);
        n[2] += (a[0] - b[0]) * (a[1] + b[1]);
    }
    normalize(n)
}

impl<'a> Ctx<'a> {
    fn sample_face(&self, face: u32, spacing: f64, edge_cache: &mut HashMap<u32, Vec<V3>>, g: &mut DefGeometry) {
        let (loops, surface, _same_sense) = self.face_loops(face, edge_cache, &mut g.lines, &mut g.edges);
        g.faces += 1;
        if loops.is_empty() {
            return;
        }
        let kinds = self.kinds;
        let before = g.surfels.len();
        let budget = self.cfg.max_surfels_per_face;
        let Some(surface) = surface else { return };
        let Some(se) = self.entity(surface) else { return };
        let k = se.kind as usize;
        let mut sampled = false;
        let mut meshed = false;
        let mesh_budget = budget * 2;
        if kinds.plane[k] {
            if let Some(a) = self.attrs(surface, &[b"PLANE"]) {
                if let Some(f) = a.get(1).and_then(|v| v.as_id()).and_then(|p| self.frame(p)) {
                    let loops2: Vec<Vec<[f64; 2]>> = loops.iter().map(|l| l.pts.iter().map(|p| { let q = f.local(*p); [q[0], q[1]] }).collect()).collect();
                    let map = |u: f64, v: f64| (f.world(u, v, 0.0), f.z);
                    sampled = sample_param(&loops2, None, spacing, spacing, &map, spacing, budget, &mut g.surfels);
                    meshed = mesh_planar(&loops2, &f, g);
                }
            }
        } else if kinds.cylinder[k] || kinds.cone[k] {
            if let Some(a) = self.attrs(surface, &[b"CYLINDRICAL_SURFACE", b"CONICAL_SURFACE"]) {
                if let (Some(f), Some(r)) = (a.get(1).and_then(|v| v.as_id()).and_then(|p| self.frame(p)), num(a.get(2))) {
                    let r = r.abs().max(1e-9);
                    let tan_a = if kinds.cone[k] { num(a.get(3)).unwrap_or(0.0).tan() } else { 0.0 };
                    let mut loops2: Vec<Vec<[f64; 2]>> = loops
                        .iter()
                        .map(|l| {
                            let mut v: Vec<[f64; 2]> = l.pts.iter().map(|p| { let q = f.local(*p); [q[1].atan2(q[0]), q[2]] }).collect();
                            unwrap_loop(&mut v, 2.0 * PI);
                            v
                        })
                        .collect();
                    // Loops that wind around the axis are open in u; close them through the seam shift.
                    for lp in loops2.iter_mut() {
                        if lp.len() >= 2 {
                            let d = lp[lp.len() - 1][0] - lp[0][0];
                            if d.abs() > PI {
                                // leave as is: the periodic test handles winding loops
                            }
                        }
                    }
                    let hmean = loops2.iter().flatten().map(|p| p[1]).sum::<f64>() / loops2.iter().map(|l| l.len()).sum::<usize>().max(1) as f64;
                    let r_mean = (r + hmean * tan_a).abs().max(spacing);
                    let map = |u: f64, v: f64| {
                        let rr = r + v * tan_a;
                        let radial = add(scale(f.x, u.cos()), scale(f.y, u.sin()));
                        let p = add(add(f.c, scale(radial, rr)), scale(f.z, v));
                        let n = normalize(sub(radial, scale(f.z, tan_a)));
                        (p, n)
                    };
                    sampled = sample_param(&loops2, Some(2.0 * PI), spacing / r_mean, spacing, &map, spacing, budget, &mut g.surfels);
                    meshed = mesh_param(&loops2, Some(2.0 * PI), spacing / r_mean, spacing, &map, mesh_budget, g);
                }
            }
        } else if kinds.sphere[k] {
            if let Some(a) = self.attrs(surface, &[b"SPHERICAL_SURFACE"]) {
                if let (Some(f), Some(r)) = (a.get(1).and_then(|v| v.as_id()).and_then(|p| self.frame(p)), num(a.get(2))) {
                    let r = r.abs().max(1e-9);
                    let loops2: Vec<Vec<[f64; 2]>> = loops
                        .iter()
                        .map(|l| {
                            let mut v: Vec<[f64; 2]> = l
                                .pts
                                .iter()
                                .map(|p| {
                                    let q = f.local(*p);
                                    let phi = (q[2] / r).clamp(-1.0, 1.0).asin();
                                    [q[1].atan2(q[0]), phi]
                                })
                                .collect();
                            unwrap_loop(&mut v, 2.0 * PI);
                            v
                        })
                        .collect();
                    let map = |u: f64, v: f64| {
                        let d = add(add(scale(f.x, v.cos() * u.cos()), scale(f.y, v.cos() * u.sin())), scale(f.z, v.sin()));
                        (add(f.c, scale(d, r)), d)
                    };
                    sampled = sample_param(&loops2, Some(2.0 * PI), spacing / r, spacing / r, &map, spacing, budget, &mut g.surfels);
                    meshed = mesh_param(&loops2, Some(2.0 * PI), spacing / r, spacing / r, &map, mesh_budget, g);
                }
            }
        } else if kinds.torus[k] {
            if let Some(a) = self.attrs(surface, &[b"TOROIDAL_SURFACE", b"DEGENERATE_TOROIDAL_SURFACE"]) {
                if let (Some(f), Some(big), Some(small)) = (a.get(1).and_then(|v| v.as_id()).and_then(|p| self.frame(p)), num(a.get(2)), num(a.get(3))) {
                    let big = big.abs();
                    let small = small.abs().max(1e-9);
                    let loops2: Vec<Vec<[f64; 2]>> = loops
                        .iter()
                        .map(|l| {
                            let mut v: Vec<[f64; 2]> = l
                                .pts
                                .iter()
                                .map(|p| {
                                    let q = f.local(*p);
                                    let rho = (q[0] * q[0] + q[1] * q[1]).sqrt();
                                    [q[1].atan2(q[0]), q[2].atan2(rho - big)]
                                })
                                .collect();
                            unwrap_loop(&mut v, 2.0 * PI);
                            // unwrap the minor angle too
                            for i in 1..v.len() {
                                let mut d = v[i][1] - v[i - 1][1];
                                while d > PI {
                                    v[i][1] -= 2.0 * PI;
                                    d -= 2.0 * PI;
                                }
                                while d < -PI {
                                    v[i][1] += 2.0 * PI;
                                    d += 2.0 * PI;
                                }
                            }
                            v
                        })
                        .collect();
                    let map = |u: f64, v: f64| {
                        let radial = add(scale(f.x, u.cos()), scale(f.y, u.sin()));
                        let p = add(add(f.c, scale(radial, big + small * v.cos())), scale(f.z, small * v.sin()));
                        let n = add(scale(radial, v.cos()), scale(f.z, v.sin()));
                        (p, n)
                    };
                    sampled = sample_param(&loops2, Some(2.0 * PI), spacing / (big + small).max(spacing), spacing / small, &map, spacing, budget, &mut g.surfels);
                    meshed = mesh_param(&loops2, Some(2.0 * PI), spacing / (big + small).max(spacing), spacing / small, &map, mesh_budget, g);
                }
            }
        } else if kinds.bspline_surface[k] {
            if let Some(s) = BsplineSurface::parse(self, surface) {
                let ((u0, u1), (v0, v1)) = s.range();
                let nu = self.cfg.spline_surface_grid.max(4).max((2 * s.ctrl.len()).min(40));
                let nv = self.cfg.spline_surface_grid.max(4).max((2 * s.ctrl[0].len()).min(40));
                let mut grid: Vec<V3> = Vec::with_capacity((nu + 1) * (nv + 1));
                for i in 0..=nu {
                    for j in 0..=nv {
                        grid.push(s.eval(u0 + (u1 - u0) * i as f64 / nu as f64, v0 + (v1 - v0) * j as f64 / nv as f64));
                    }
                }
                let at = |i: usize, j: usize| grid[i * (nv + 1) + j];
                let vbase = g.mesh_vertices.len() as u32;
                for i in 0..=nu {
                    for j in 0..=nv {
                        let p = at(i, j);
                        let i0 = i.saturating_sub(1);
                        let i1 = (i + 1).min(nu);
                        let j0 = j.saturating_sub(1);
                        let j1 = (j + 1).min(nv);
                        let du = sub(at(i1, j), at(i0, j));
                        let dv = sub(at(i, j1), at(i, j0));
                        let n = cross(du, dv);
                        let local = (norm(du) / (i1 - i0).max(1) as f64).max(norm(dv) / (j1 - j0).max(1) as f64);
                        if g.surfels.len() - before < budget {
                            push_surfel(&mut g.surfels, p, n, (local * 0.75).max(spacing * 0.5));
                        }
                        push_mesh_vertex(g, p, n);
                    }
                }
                for i in 0..nu {
                    for j in 0..nv {
                        let a = vbase + (i * (nv + 1) + j) as u32;
                        let b = vbase + ((i + 1) * (nv + 1) + j) as u32;
                        let c = vbase + ((i + 1) * (nv + 1) + j + 1) as u32;
                        let d = vbase + (i * (nv + 1) + j + 1) as u32;
                        g.mesh_indices.extend_from_slice(&[a, b, c, a, c, d]);
                    }
                }
                sampled = g.surfels.len() > before;
                meshed = nu > 0 && nv > 0;
            }
        }
        // Surfels along the loops give every face an outline, including
        // faces on surfaces that are not sampled.
        let n_est = loops.iter().map(|l| loop_normal(&l.pts)).next().unwrap_or([0.0, 0.0, 1.0]);
        for l in &loops {
            for p in &l.pts {
                if g.surfels.len() - before < budget + 2000 {
                    push_surfel(&mut g.surfels, *p, n_est, spacing * 0.55);
                }
            }
        }
        if sampled {
            g.faces_sampled += 1;
        }
        if meshed {
            g.faces_meshed += 1;
        }
    }
}

/// Faces reachable from a definition's shape representations.
fn collect_faces(ctx: &Ctx, roots: &[u32], max_id: u32) -> Vec<u32> {
    let words = (max_id as usize / 64) + 1;
    let mut visited = vec![0u64; words];
    let mut stack: Vec<u32> = roots.to_vec();
    let mut faces = Vec::new();
    let mut refs = Vec::with_capacity(32);
    while let Some(id) = stack.pop() {
        let w = (id / 64) as usize;
        let bit = 1u64 << (id % 64);
        if w >= visited.len() || visited[w] & bit != 0 {
            continue;
        }
        visited[w] |= bit;
        let Some(e) = ctx.table.get(id) else { continue };
        let k = e.kind as usize;
        if ctx.kinds.face[k] {
            faces.push(id);
            continue; // the face sampler reads below this
        }
        if ctx.kinds.stop[k] {
            continue;
        }
        refs.clear();
        crate::geom::entity_refs(e.text(ctx.data), &mut refs);
        stack.extend_from_slice(&refs);
    }
    faces
}

/// Sample every definition with bodies, largest first, in parallel.
/// `on_done` receives each definition's geometry as it completes.
pub fn definition_geometry<F>(
    data: &[u8],
    table: &EntityTable,
    tree: &ProductTree,
    bounds: &[Option<DefBounds>],
    cfg: &SampleConfig,
    on_done: F,
) -> Vec<Option<DefGeometry>>
where
    F: Fn(u32, &DefGeometry) + Sync,
{
    let kinds = classify(table);
    let n = tree.definitions.len();
    let mut order: Vec<u32> = (0..n as u32).filter(|&d| tree.definitions[d as usize].bodies > 0).collect();
    order.sort_by_key(|&d| {
        std::cmp::Reverse(tree.definitions[d as usize].shape_reps.iter().map(|&sr| table.get(sr).map(|e| e.len as u64).unwrap_or(0)).sum::<u64>())
    });
    let results: Vec<(u32, DefGeometry)> = order
        .par_iter()
        .map(|&d| {
            let ctx = Ctx { data, table, cfg, kinds: &kinds };
            let def = &tree.definitions[d as usize];
            let diag = bounds
                .get(d as usize)
                .and_then(|b| b.as_ref())
                .map(|b| {
                    let s = b.aabb.size();
                    (s[0] * s[0] + s[1] * s[1] + s[2] * s[2]).sqrt()
                })
                .filter(|v| v.is_finite() && *v > 0.0)
                .unwrap_or(100.0);
            let spacing = (diag / cfg.samples_across).max(1e-6);
            let mut g = DefGeometry { spacing: spacing as f32, ..Default::default() };
            let faces = collect_faces(&ctx, &def.shape_reps, table.max_id);
            let mut edge_cache: HashMap<u32, Vec<V3>> = HashMap::new();
            for face in faces {
                if g.surfels.len() >= cfg.max_surfels_per_def {
                    break;
                }
                ctx.sample_face(face, spacing, &mut edge_cache, &mut g);
            }
            on_done(d, &g);
            (d, g)
        })
        .collect();
    let mut out: Vec<Option<DefGeometry>> = (0..n).map(|_| None).collect();
    for (d, g) in results {
        out[d as usize] = Some(g);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn de_boor_matches_bezier() {
        // cubic Bezier as a clamped B-spline
        let knots = vec![0.0, 0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 1.0];
        let ctrl = vec![[0.0, 0.0, 0.0, 1.0], [1.0, 2.0, 0.0, 1.0], [3.0, 2.0, 0.0, 1.0], [4.0, 0.0, 0.0, 1.0]];
        let p = dehomog(de_boor(3, &knots, &ctrl, 0.5));
        // Bezier at t=0.5: (0.125*0 + 0.375*1 + 0.375*3 + 0.125*4, 0.375*2+0.375*2) = (2.0, 1.5)
        assert!((p[0] - 2.0).abs() < 1e-12 && (p[1] - 1.5).abs() < 1e-12);
        let s = dehomog(de_boor(3, &knots, &ctrl, 0.0));
        let e = dehomog(de_boor(3, &knots, &ctrl, 1.0));
        assert!((s[0]).abs() < 1e-12 && (e[0] - 4.0).abs() < 1e-12);
    }

    #[test]
    fn even_odd_with_hole_and_period() {
        let outer = vec![[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 10.0]];
        let hole = vec![[4.0, 4.0], [6.0, 4.0], [6.0, 6.0], [4.0, 6.0]];
        let loops = vec![outer, hole];
        assert!(inside(&loops, 1.0, 1.0, None));
        assert!(!inside(&loops, 5.0, 5.0, None));
        assert!(!inside(&loops, 11.0, 5.0, None));
        // periodic band around a cylinder: two winding loops at v=0 and v=5
        let bottom: Vec<[f64; 2]> = (0..=8).map(|i| [i as f64 * PI / 4.0, 0.0]).collect();
        let top: Vec<[f64; 2]> = (0..=8).map(|i| [i as f64 * PI / 4.0, 5.0]).collect();
        let band = vec![bottom, top];
        assert!(inside(&band, 1.0, 2.5, Some(2.0 * PI)));
        assert!(inside(&band, 1.0 + 2.0 * PI, 2.5, Some(2.0 * PI)));
        assert!(!inside(&band, 1.0, 7.0, Some(2.0 * PI)));
    }

    /// The plate-with-hole file used by the sampling tests.
    fn plate_file() -> String {
        let mut s = String::from("ISO-10303-21;\nHEADER;\nFILE_SCHEMA(('X'));\nENDSEC;\nDATA;\n");
        s.push_str("#1=PRODUCT('p','plate','',(#2));\n#2=PRODUCT_CONTEXT('',#3,'');\n#3=APPLICATION_CONTEXT('x');\n");
        s.push_str("#4=PRODUCT_DEFINITION_FORMATION('','',#1);\n#5=PRODUCT_DEFINITION('d','',#4,#6);\n#6=PRODUCT_DEFINITION_CONTEXT('',#3,'');\n");
        s.push_str("#7=PRODUCT_DEFINITION_SHAPE('','',#5);\n#8=SHAPE_DEFINITION_REPRESENTATION(#7,#9);\n");
        s.push_str("#9=ADVANCED_BREP_SHAPE_REPRESENTATION('',(#10),#90);\n#10=MANIFOLD_SOLID_BREP('',#11);\n#11=CLOSED_SHELL('',(#20,#40));\n");
        s.push_str("#20=ADVANCED_FACE('',(#21,#30),#60,.T.);\n#21=FACE_OUTER_BOUND('',#22,.T.);\n#22=EDGE_LOOP('',(#23,#24,#25,#26));\n");
        s.push_str("#23=ORIENTED_EDGE('',*,*,#27,.T.);\n#24=ORIENTED_EDGE('',*,*,#28,.T.);\n#25=ORIENTED_EDGE('',*,*,#29,.T.);\n#26=ORIENTED_EDGE('',*,*,#31,.T.);\n");
        s.push_str("#27=EDGE_CURVE('',#70,#71,#80,.T.);\n#28=EDGE_CURVE('',#71,#72,#81,.T.);\n#29=EDGE_CURVE('',#72,#73,#82,.T.);\n#31=EDGE_CURVE('',#73,#70,#83,.T.);\n");
        s.push_str("#30=FACE_BOUND('',#32,.T.);\n#32=EDGE_LOOP('',(#33));\n#33=ORIENTED_EDGE('',*,*,#34,.T.);\n#34=EDGE_CURVE('',#74,#74,#61,.T.);\n");
        s.push_str("#40=ADVANCED_FACE('',(#41,#44),#62,.T.);\n#41=FACE_BOUND('',#42,.T.);\n#42=EDGE_LOOP('',(#43));\n#43=ORIENTED_EDGE('',*,*,#34,.F.);\n");
        s.push_str("#44=FACE_BOUND('',#45,.T.);\n#45=EDGE_LOOP('',(#46));\n#46=ORIENTED_EDGE('',*,*,#47,.T.);\n#47=EDGE_CURVE('',#75,#75,#63,.T.);\n");
        s.push_str("#60=PLANE('',#64);\n#61=CIRCLE('',#65,2.);\n#62=CYLINDRICAL_SURFACE('',#65,2.);\n#63=CIRCLE('',#66,2.);\n");
        s.push_str("#64=AXIS2_PLACEMENT_3D('',#100,#101,#102);\n#65=AXIS2_PLACEMENT_3D('',#103,#101,#102);\n#66=AXIS2_PLACEMENT_3D('',#104,#101,#102);\n");
        s.push_str("#70=VERTEX_POINT('',#100);\n#71=VERTEX_POINT('',#105);\n#72=VERTEX_POINT('',#106);\n#73=VERTEX_POINT('',#107);\n#74=VERTEX_POINT('',#108);\n#75=VERTEX_POINT('',#109);\n");
        s.push_str("#80=LINE('',#100,#110);\n#81=LINE('',#105,#111);\n#82=LINE('',#106,#110);\n#83=LINE('',#107,#111);\n");
        s.push_str("#90=GEOMETRIC_REPRESENTATION_CONTEXT(3);\n");
        s.push_str("#100=CARTESIAN_POINT('',(0.,0.,0.));\n#101=DIRECTION('',(0.,0.,1.));\n#102=DIRECTION('',(1.,0.,0.));\n#103=CARTESIAN_POINT('',(5.,5.,0.));\n#104=CARTESIAN_POINT('',(5.,5.,3.));\n");
        s.push_str("#105=CARTESIAN_POINT('',(10.,0.,0.));\n#106=CARTESIAN_POINT('',(10.,10.,0.));\n#107=CARTESIAN_POINT('',(0.,10.,0.));\n#108=CARTESIAN_POINT('',(7.,5.,0.));\n#109=CARTESIAN_POINT('',(7.,5.,3.));\n");
        s.push_str("#110=VECTOR('',#102,1.);\n#111=VECTOR('',#112,1.);\n#112=DIRECTION('',(0.,1.,0.));\n");
        s.push_str("ENDSEC;\nEND-ISO-10303-21;\n");
        s
    }

    #[test]
    fn density_sweep_is_stable() {
        // Sampling at many densities must never panic, must respect the caps,
        // must produce finite numbers, and must grow roughly with density².
        let s = plate_file();
        let data = s.as_bytes();
        let table = crate::scan::scan(data, &crate::scan::ScanConfig::default()).unwrap();
        let tree = crate::structure::build(data, &table);
        let bounds = crate::geom::definition_bounds(data, &table, &tree, |_, _| {});
        let mut last = 0usize;
        for (across, cap) in [(4.0, 100usize), (8.0, 1000), (20.0, 1000), (56.0, 150_000), (160.0, 150_000), (400.0, 150_000), (400.0, 2_000), (1.0, 50)] {
            let cfg = SampleConfig { samples_across: across, max_surfels_per_def: cap, max_surfels_per_face: cap.max(500), ..Default::default() };
            let geoms = definition_geometry(data, &table, &tree, &bounds, &cfg, |_, _| {});
            let g = geoms[0].as_ref().unwrap();
            assert!(g.surfels.len() <= cap + 2 * 4000 + 100, "across {across}: {} surfels over cap {cap}", g.surfels.len());
            for sf in &g.surfels {
                assert!(sf.pos.iter().all(|v| v.is_finite()) && sf.nrm.iter().all(|v| v.is_finite()) && sf.radius.is_finite() && sf.radius > 0.0);
                assert!(sf.radius < 50.0, "radius {} at across {across}", sf.radius);
            }
            for p in &g.lines {
                assert!(p.iter().all(|v| v.is_finite()));
            }
            if across >= 8.0 && across <= 160.0 && cap >= 150_000 {
                assert!(g.surfels.len() >= last, "surfel count fell from {last} to {} at across {across}", g.surfels.len());
                last = g.surfels.len();
            }
        }
    }

    #[test]
    fn samples_a_plate_with_hole_and_a_bolt() {
        // A square plate face with a round hole (plane), and a cylinder face
        // (the hole wall), built as a tiny STEP file.
        let mut s = String::from("ISO-10303-21;\nHEADER;\nFILE_SCHEMA(('X'));\nENDSEC;\nDATA;\n");
        s.push_str("#1=PRODUCT('p','plate','',(#2));\n#2=PRODUCT_CONTEXT('',#3,'');\n#3=APPLICATION_CONTEXT('x');\n");
        s.push_str("#4=PRODUCT_DEFINITION_FORMATION('','',#1);\n#5=PRODUCT_DEFINITION('d','',#4,#6);\n#6=PRODUCT_DEFINITION_CONTEXT('',#3,'');\n");
        s.push_str("#7=PRODUCT_DEFINITION_SHAPE('','',#5);\n#8=SHAPE_DEFINITION_REPRESENTATION(#7,#9);\n");
        s.push_str("#9=ADVANCED_BREP_SHAPE_REPRESENTATION('',(#10),#90);\n#10=MANIFOLD_SOLID_BREP('',#11);\n#11=CLOSED_SHELL('',(#20,#40));\n");
        // plane face: outer square 0..10, inner circle r=2 at (5,5)
        s.push_str("#20=ADVANCED_FACE('',(#21,#30),#60,.T.);\n#21=FACE_OUTER_BOUND('',#22,.T.);\n#22=EDGE_LOOP('',(#23,#24,#25,#26));\n");
        s.push_str("#23=ORIENTED_EDGE('',*,*,#27,.T.);\n#24=ORIENTED_EDGE('',*,*,#28,.T.);\n#25=ORIENTED_EDGE('',*,*,#29,.T.);\n#26=ORIENTED_EDGE('',*,*,#31,.T.);\n");
        s.push_str("#27=EDGE_CURVE('',#70,#71,#80,.T.);\n#28=EDGE_CURVE('',#71,#72,#81,.T.);\n#29=EDGE_CURVE('',#72,#73,#82,.T.);\n#31=EDGE_CURVE('',#73,#70,#83,.T.);\n");
        s.push_str("#30=FACE_BOUND('',#32,.T.);\n#32=EDGE_LOOP('',(#33));\n#33=ORIENTED_EDGE('',*,*,#34,.T.);\n#34=EDGE_CURVE('',#74,#74,#61,.T.);\n");
        // cylinder face (hole wall): loops are the circle at z=0 and at z=3
        s.push_str("#40=ADVANCED_FACE('',(#41,#44),#62,.T.);\n#41=FACE_BOUND('',#42,.T.);\n#42=EDGE_LOOP('',(#43));\n#43=ORIENTED_EDGE('',*,*,#34,.F.);\n");
        s.push_str("#44=FACE_BOUND('',#45,.T.);\n#45=EDGE_LOOP('',(#46));\n#46=ORIENTED_EDGE('',*,*,#47,.T.);\n#47=EDGE_CURVE('',#75,#75,#63,.T.);\n");
        s.push_str("#60=PLANE('',#64);\n#61=CIRCLE('',#65,2.);\n#62=CYLINDRICAL_SURFACE('',#65,2.);\n#63=CIRCLE('',#66,2.);\n");
        s.push_str("#64=AXIS2_PLACEMENT_3D('',#100,#101,#102);\n#65=AXIS2_PLACEMENT_3D('',#103,#101,#102);\n#66=AXIS2_PLACEMENT_3D('',#104,#101,#102);\n");
        s.push_str("#70=VERTEX_POINT('',#100);\n#71=VERTEX_POINT('',#105);\n#72=VERTEX_POINT('',#106);\n#73=VERTEX_POINT('',#107);\n#74=VERTEX_POINT('',#108);\n#75=VERTEX_POINT('',#109);\n");
        s.push_str("#80=LINE('',#100,#110);\n#81=LINE('',#105,#111);\n#82=LINE('',#106,#110);\n#83=LINE('',#107,#111);\n");
        s.push_str("#90=GEOMETRIC_REPRESENTATION_CONTEXT(3);\n");
        s.push_str("#100=CARTESIAN_POINT('',(0.,0.,0.));\n#101=DIRECTION('',(0.,0.,1.));\n#102=DIRECTION('',(1.,0.,0.));\n#103=CARTESIAN_POINT('',(5.,5.,0.));\n#104=CARTESIAN_POINT('',(5.,5.,3.));\n");
        s.push_str("#105=CARTESIAN_POINT('',(10.,0.,0.));\n#106=CARTESIAN_POINT('',(10.,10.,0.));\n#107=CARTESIAN_POINT('',(0.,10.,0.));\n#108=CARTESIAN_POINT('',(7.,5.,0.));\n#109=CARTESIAN_POINT('',(7.,5.,3.));\n");
        s.push_str("#110=VECTOR('',#102,1.);\n#111=VECTOR('',#112,1.);\n#112=DIRECTION('',(0.,1.,0.));\n");
        s.push_str("ENDSEC;\nEND-ISO-10303-21;\n");
        let data = s.as_bytes();
        let table = crate::scan::scan(data, &crate::scan::ScanConfig::default()).unwrap();
        let tree = crate::structure::build(data, &table);
        assert_eq!(tree.definitions.len(), 1);
        let bounds = crate::geom::definition_bounds(data, &table, &tree, |_, _| {});
        let cfg = SampleConfig { samples_across: 20.0, ..Default::default() };
        let geoms = definition_geometry(data, &table, &tree, &bounds, &cfg, |_, _| {});
        let g = geoms[0].as_ref().unwrap();
        assert_eq!(g.faces, 2);
        assert_eq!(g.faces_sampled, 2);
        // 4 lines + 2 circles (24 segments each) of edges
        assert_eq!(g.edges, 6);
        assert!(g.lines.len() >= (4 + 48) * 2);
        // plate surfels must avoid the hole: no sample within r=2 of (5,5) at z=0
        let plane_hits = g.surfels.iter().filter(|s| s.pos[2].abs() < 1e-6 && s.nrm[2].abs() > 0.99).count();
        let in_hole = g.surfels.iter().filter(|s| s.pos[2].abs() < 1e-6 && s.nrm[2].abs() > 0.99 && ((s.pos[0] - 5.0).powi(2) + (s.pos[1] - 5.0).powi(2)).sqrt() < 1.7).count();
        assert!(plane_hits > 50, "plane surfels {plane_hits}");
        assert_eq!(in_hole, 0, "surfels inside the hole");
        // cylinder surfels sit on radius 2 around (5,5) between z=0 and z=3
        let cyl = g
            .surfels
            .iter()
            .filter(|s| s.pos[2] > 0.1 && s.pos[2] < 2.9 && (((s.pos[0] - 5.0).powi(2) + (s.pos[1] - 5.0).powi(2)).sqrt() - 2.0).abs() < 1e-3)
            .count();
        assert!(cyl > 20, "cylinder surfels {cyl}");
        // mesh: both faces meshed; the plate's triangles cover 100 - pi*4 of area and none cross the hole
        assert_eq!(g.faces_meshed, 2);
        assert!(g.mesh_indices.len() % 3 == 0 && !g.mesh_indices.is_empty());
        let mut area = 0.0f64;
        for t in g.mesh_indices.chunks(3) {
            let v: Vec<[f32; 3]> = t.iter().map(|&i| g.mesh_vertices[i as usize].pos).collect();
            if v.iter().all(|p| p[2].abs() < 1e-6) {
                let ax = (v[1][0] - v[0][0]) as f64;
                let ay = (v[1][1] - v[0][1]) as f64;
                let bx = (v[2][0] - v[0][0]) as f64;
                let by = (v[2][1] - v[0][1]) as f64;
                area += (ax * by - ay * bx).abs() * 0.5;
                let cx = (v[0][0] + v[1][0] + v[2][0]) as f64 / 3.0;
                let cy = (v[0][1] + v[1][1] + v[2][1]) as f64 / 3.0;
                assert!(((cx - 5.0).powi(2) + (cy - 5.0).powi(2)).sqrt() > 1.9, "plate triangle centroid inside the hole");
            }
        }
        let expected = 100.0 - std::f64::consts::PI * 4.0;
        assert!((area - expected).abs() < 1.0, "plate mesh area {area} vs {expected}");
        for i in &g.mesh_indices {
            assert!((*i as usize) < g.mesh_vertices.len());
        }
    }
}
