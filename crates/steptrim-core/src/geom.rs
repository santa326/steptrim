//! Placements, occurrence transforms, expanded instances and per-definition
//! bounding boxes, all computed from the entity table without a kernel.

use crate::attr::{parse_attrs, parse_complex};
use crate::scan::EntityTable;
use crate::structure::ProductTree;
use rayon::prelude::*;
use std::cell::RefCell;
use std::collections::HashMap;

/// Rigid (or affine) transform: `x' = m * x + t`, `m` stored as rows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Transform {
    pub m: [[f64; 3]; 3],
    pub t: [f64; 3],
}

impl Transform {
    pub const IDENTITY: Transform = Transform { m: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]], t: [0.0; 3] };

    /// `self ∘ other`: apply `other` first, then `self`.
    pub fn mul(&self, other: &Transform) -> Transform {
        let mut m = [[0.0; 3]; 3];
        for i in 0..3 {
            for j in 0..3 {
                m[i][j] = self.m[i][0] * other.m[0][j] + self.m[i][1] * other.m[1][j] + self.m[i][2] * other.m[2][j];
            }
        }
        let t = self.apply(other.t);
        Transform { m, t }
    }

    pub fn apply(&self, p: [f64; 3]) -> [f64; 3] {
        [
            self.m[0][0] * p[0] + self.m[0][1] * p[1] + self.m[0][2] * p[2] + self.t[0],
            self.m[1][0] * p[0] + self.m[1][1] * p[1] + self.m[1][2] * p[2] + self.t[1],
            self.m[2][0] * p[0] + self.m[2][1] * p[1] + self.m[2][2] * p[2] + self.t[2],
        ]
    }

    /// General 3x3 inverse (handles mirrored or scaled placements too).
    pub fn inverse(&self) -> Option<Transform> {
        let m = &self.m;
        let det = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1]) - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
            + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
        if det.abs() < 1e-18 {
            return None;
        }
        let d = 1.0 / det;
        let inv = [
            [
                (m[1][1] * m[2][2] - m[1][2] * m[2][1]) * d,
                (m[0][2] * m[2][1] - m[0][1] * m[2][2]) * d,
                (m[0][1] * m[1][2] - m[0][2] * m[1][1]) * d,
            ],
            [
                (m[1][2] * m[2][0] - m[1][0] * m[2][2]) * d,
                (m[0][0] * m[2][2] - m[0][2] * m[2][0]) * d,
                (m[0][2] * m[1][0] - m[0][0] * m[1][2]) * d,
            ],
            [
                (m[1][0] * m[2][1] - m[1][1] * m[2][0]) * d,
                (m[0][1] * m[2][0] - m[0][0] * m[2][1]) * d,
                (m[0][0] * m[1][1] - m[0][1] * m[1][0]) * d,
            ],
        ];
        let r = Transform { m: inv, t: [0.0; 3] };
        let t = r.apply([-self.t[0], -self.t[1], -self.t[2]]);
        Some(Transform { m: inv, t })
    }

    /// Column-major 4x4 as f32, for GPU upload.
    pub fn to_cols_f32(&self) -> [f32; 16] {
        let m = &self.m;
        [
            m[0][0] as f32, m[1][0] as f32, m[2][0] as f32, 0.0,
            m[0][1] as f32, m[1][1] as f32, m[2][1] as f32, 0.0,
            m[0][2] as f32, m[1][2] as f32, m[2][2] as f32, 0.0,
            self.t[0] as f32, self.t[1] as f32, self.t[2] as f32, 1.0,
        ]
    }
}

fn normalize(v: [f64; 3]) -> Option<[f64; 3]> {
    let n = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    if n < 1e-300 {
        None
    } else {
        Some([v[0] / n, v[1] / n, v[2] / n])
    }
}

fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}

fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

/// Parse the coordinate list of a CARTESIAN_POINT or DIRECTION from its
/// attribute text. Returns the values and how many there were (2 or 3).
pub fn parse_coords(attrs: &[u8]) -> Option<([f64; 3], usize)> {
    let open = attrs.iter().rposition(|&b| b == b'(')?;
    let close = attrs[open..].iter().position(|&b| b == b')').map(|p| open + p)?;
    let mut v = [0.0f64; 3];
    let mut n = 0usize;
    for part in attrs[open + 1..close].split(|&b| b == b',') {
        if n >= 3 {
            n += 1;
            break;
        }
        let s = std::str::from_utf8(part).ok()?.trim();
        v[n] = parse_step_real(s)?;
        n += 1;
    }
    if n == 0 {
        None
    } else {
        Some((v, n.min(3)))
    }
}

/// STEP reals may end with a bare `.` (e.g. `1.`) or use `E` exponents; Rust's
/// parser accepts both, but `.5` and `-.5` need a leading zero.
fn parse_step_real(s: &str) -> Option<f64> {
    if let Ok(v) = s.parse::<f64>() {
        return Some(v);
    }
    let fixed = if let Some(r) = s.strip_prefix("-.") {
        format!("-0.{r}")
    } else if let Some(r) = s.strip_prefix('.') {
        format!("0.{r}")
    } else {
        return None;
    };
    fixed.parse::<f64>().ok()
}

/// Build the transform of an AXIS2_PLACEMENT_3D entity: location, axis (z)
/// and ref_direction (x), with STEP's defaults when they are absent.
pub fn axis2_placement(data: &[u8], table: &EntityTable, id: u32) -> Option<Transform> {
    let e = table.get(id)?;
    let a = parse_attrs(e.attrs(data)).ok()?;
    let loc = a.get(1).and_then(|v| v.as_id()).and_then(|p| table.get(p)).and_then(|p| parse_coords(p.attrs(data))).map(|(v, _)| v)?;
    let axis = a
        .get(2)
        .and_then(|v| v.as_id())
        .and_then(|d| table.get(d))
        .and_then(|d| parse_coords(d.attrs(data)))
        .map(|(v, _)| v)
        .and_then(normalize)
        .unwrap_or([0.0, 0.0, 1.0]);
    let refd = a
        .get(3)
        .and_then(|v| v.as_id())
        .and_then(|d| table.get(d))
        .and_then(|d| parse_coords(d.attrs(data)))
        .map(|(v, _)| v)
        .unwrap_or(if axis[0].abs() < 0.9 { [1.0, 0.0, 0.0] } else { [0.0, 1.0, 0.0] });
    // x = ref_direction made orthogonal to z
    let k = dot(refd, axis);
    let x = normalize([refd[0] - k * axis[0], refd[1] - k * axis[1], refd[2] - k * axis[2]])
        .or_else(|| normalize(cross([0.0, 1.0, 0.0], axis)))
        .unwrap_or([1.0, 0.0, 0.0]);
    let y = cross(axis, x);
    Some(Transform {
        m: [[x[0], y[0], axis[0]], [x[1], y[1], axis[1]], [x[2], y[2], axis[2]]],
        t: loc,
    })
}

#[derive(Clone, Debug, Default)]
pub struct TransformStats {
    pub resolved: usize,
    pub missing_shape: usize,
    pub missing_relationship: usize,
    pub bad_transform: usize,
}

/// Per occurrence: the transform that places the child's representation in
/// the parent's, from CONTEXT_DEPENDENT_SHAPE_REPRESENTATION, the transform
/// relationship and ITEM_DEFINED_TRANSFORMATION. `None` when unresolved.
pub fn occurrence_transforms(data: &[u8], table: &EntityTable, tree: &ProductTree) -> (Vec<Option<Transform>>, TransformStats) {
    let mut stats = TransformStats::default();
    // NAUO id -> PRODUCT_DEFINITION_SHAPE id
    let mut pds_of: HashMap<u32, u32> = HashMap::new();
    if let Some(k) = table.kind_id("PRODUCT_DEFINITION_SHAPE") {
        for &i in table.of_kind(k) {
            let e = &table.entities[i as usize];
            if let Ok(a) = parse_attrs(e.attrs(data)) {
                if let Some(d) = a.get(2).and_then(|v| v.as_id()) {
                    pds_of.insert(d, e.id);
                }
            }
        }
    }
    // PDS id -> representation relationship id
    let mut rel_of: HashMap<u32, u32> = HashMap::new();
    for k in table.kinds_containing("CONTEXT_DEPENDENT_SHAPE_REPRESENTATION") {
        for &i in table.of_kind(k) {
            let e = &table.entities[i as usize];
            if e.complex {
                continue;
            }
            if let Ok(a) = parse_attrs(e.attrs(data)) {
                if let (Some(rr), Some(pds)) = (a.first().and_then(|v| v.as_id()), a.get(1).and_then(|v| v.as_id())) {
                    rel_of.insert(pds, rr);
                }
            }
        }
    }

    let mut cache: HashMap<u32, Option<Transform>> = HashMap::new();
    let mut placement = |id: u32| -> Option<Transform> {
        *cache.entry(id).or_insert_with(|| axis2_placement(data, table, id))
    };

    let mut out = Vec::with_capacity(tree.occurrences.len());
    for occ in &tree.occurrences {
        let Some(&pds) = pds_of.get(&occ.entity) else {
            stats.missing_shape += 1;
            out.push(None);
            continue;
        };
        let Some(&rr) = rel_of.get(&pds) else {
            stats.missing_relationship += 1;
            out.push(None);
            continue;
        };
        let Some(e) = table.get(rr) else {
            stats.missing_relationship += 1;
            out.push(None);
            continue;
        };
        // (rep_1, rep_2, transformation)
        let parsed: Option<(u32, u32, u32)> = if e.complex {
            parse_complex(e.attrs(data)).ok().and_then(|parts| {
                let rel = parts.iter().find(|(n, _)| *n == b"REPRESENTATION_RELATIONSHIP");
                let wt = parts.iter().find(|(n, _)| *n == b"REPRESENTATION_RELATIONSHIP_WITH_TRANSFORMATION");
                match (rel, wt) {
                    (Some((_, r)), Some((_, w))) => Some((r.get(2)?.as_id()?, r.get(3)?.as_id()?, w.first()?.as_id()?)),
                    _ => None,
                }
            })
        } else {
            parse_attrs(e.attrs(data)).ok().and_then(|a| Some((a.get(2)?.as_id()?, a.get(3)?.as_id()?, a.get(4)?.as_id()?)))
        };
        let Some((rep1, rep2, idt)) = parsed else {
            stats.bad_transform += 1;
            out.push(None);
            continue;
        };
        let idt_attrs = table.get(idt).and_then(|t| parse_attrs(t.attrs(data)).ok());
        let Some((item1, item2)) = idt_attrs.and_then(|a| Some((a.get(2)?.as_id()?, a.get(3)?.as_id()?))) else {
            stats.bad_transform += 1;
            out.push(None);
            continue;
        };
        // By convention rep_1 / item_1 belong to the child. Verify against the
        // child's shape representations and swap if the file is reversed.
        let child_reps = &tree.definitions[occ.child as usize].shape_reps;
        let (child_pl, parent_pl) = if child_reps.contains(&rep2) && !child_reps.contains(&rep1) {
            (item2, item1)
        } else {
            (item1, item2)
        };
        match (placement(parent_pl), placement(child_pl).and_then(|c| c.inverse())) {
            (Some(p), Some(ci)) => {
                stats.resolved += 1;
                out.push(Some(p.mul(&ci)));
            }
            _ => {
                stats.bad_transform += 1;
                out.push(None);
            }
        }
    }
    (out, stats)
}

/// One node of the expanded assembly: a definition placed in world space.
#[derive(Clone, Copy, Debug)]
pub struct Instance {
    pub definition: u32,
    /// Index into `tree.occurrences`, `None` for a root.
    pub occurrence: Option<u32>,
    /// Index of the parent instance, `None` for a root.
    pub parent: Option<u32>,
    pub depth: u16,
    pub world: Transform,
}

/// Expand the tree into placed instances, depth-first from each root, so a
/// parent always precedes its children.
pub fn expand_instances(tree: &ProductTree, occ_transforms: &[Option<Transform>]) -> Vec<Instance> {
    let mut out: Vec<Instance> = Vec::with_capacity(tree.total_instances() as usize);
    let mut stack: Vec<(u32, Option<u32>, Option<u32>, u16, Transform)> = Vec::new();
    for &r in tree.roots.iter().rev() {
        if tree.children[r as usize].is_empty() && !tree.definitions[r as usize].has_shape {
            continue;
        }
        stack.push((r, None, None, 0, Transform::IDENTITY));
    }
    while let Some((d, occ, parent, depth, world)) = stack.pop() {
        let idx = out.len() as u32;
        out.push(Instance { definition: d, occurrence: occ, parent, depth, world });
        if depth >= 64 {
            continue;
        }
        for &o in tree.children[d as usize].iter().rev() {
            let occurrence = &tree.occurrences[o as usize];
            let local = occ_transforms.get(o as usize).copied().flatten().unwrap_or(Transform::IDENTITY);
            stack.push((occurrence.child, Some(o), Some(idx), depth + 1, world.mul(&local)));
        }
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Aabb {
    pub min: [f64; 3],
    pub max: [f64; 3],
}

impl Aabb {
    pub const EMPTY: Aabb = Aabb { min: [f64::INFINITY; 3], max: [f64::NEG_INFINITY; 3] };
    pub fn is_empty(&self) -> bool {
        self.min[0] > self.max[0]
    }
    pub fn add(&mut self, p: [f64; 3]) {
        for i in 0..3 {
            self.min[i] = self.min[i].min(p[i]);
            self.max[i] = self.max[i].max(p[i]);
        }
    }
    pub fn union(&mut self, o: &Aabb) {
        if !o.is_empty() {
            self.add(o.min);
            self.add(o.max);
        }
    }
    /// Box of this box's eight corners after a transform.
    pub fn transformed(&self, t: &Transform) -> Aabb {
        let mut b = Aabb::EMPTY;
        if self.is_empty() {
            return b;
        }
        for i in 0..8 {
            let p = [
                if i & 1 == 0 { self.min[0] } else { self.max[0] },
                if i & 2 == 0 { self.min[1] } else { self.max[1] },
                if i & 4 == 0 { self.min[2] } else { self.max[2] },
            ];
            b.add(t.apply(p));
        }
        b
    }
    pub fn center(&self) -> [f64; 3] {
        [(self.min[0] + self.max[0]) * 0.5, (self.min[1] + self.max[1]) * 0.5, (self.min[2] + self.max[2]) * 0.5]
    }
    pub fn size(&self) -> [f64; 3] {
        [self.max[0] - self.min[0], self.max[1] - self.min[1], self.max[2] - self.min[2]]
    }
}

/// Bounding box of one definition's own geometry, plus the number of 3D
/// points that contributed.
#[derive(Clone, Copy, Debug)]
pub struct DefBounds {
    pub aabb: Aabb,
    pub points: u32,
    pub entities: u32,
}

/// Append every `#n` reference in an entity's text (after the `=`) to `out`.
pub fn entity_refs(text: &[u8], out: &mut Vec<u32>) {
    let start = text.iter().position(|&b| b == b'=').map(|p| p + 1).unwrap_or(0);
    let mut i = start;
    let mut in_str = false;
    while i < text.len() {
        let b = text[i];
        if in_str {
            if b == b'\'' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        match b {
            b'\'' => {
                in_str = true;
                i += 1;
            }
            b'#' => {
                let mut v: u32 = 0;
                let mut e = i + 1;
                while e < text.len() && text[e].is_ascii_digit() {
                    v = v.wrapping_mul(10).wrapping_add((text[e] - b'0') as u32);
                    e += 1;
                }
                if e > i + 1 {
                    out.push(v);
                }
                i = e;
            }
            _ => i += 1,
        }
    }
}

thread_local! {
    static VISITED: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
}

/// Kinds the geometry walk must not enter. Matched on whole `_`-separated
/// tokens so that REPRESENTATION does not match PRESENTATION.
///
/// Besides contexts, units, styling, 2D parameter-space curves and product
/// data, this stops at analytic curves and surfaces (LINE, PLANE, cylinders,
/// cones, spheres, tori, sweeps): their base points can lie anywhere along an
/// unbounded shape, so they say nothing about a part's extent. The box comes
/// from vertex points, B-spline control points, and circle and ellipse
/// extents instead.
fn stop_kinds(table: &EntityTable) -> Vec<bool> {
    const STOP: &[&str] = &[
        "CONTEXT", "UNIT", "STYLE", "STYLED", "PRESENTATION", "COLOUR", "LAYER", "PCURVE", "DEFINITIONAL", "PRODUCT",
        "PROPERTY", "DOCUMENT", "APPLICATION", "RELATIONSHIP", "MEASURE", "ORGANIZATION", "PERSON", "DATE", "APPROVAL",
        "SECURITY", "CATEGORY", "MAPPED", "MAP", "ANNOTATION", "DRAUGHTING", "CAMERA", "TEXT", "LINE", "PLANE", "VECTOR",
        "PLACEMENT", "CYLINDRICAL", "CONICAL", "SPHERICAL", "TOROIDAL", "REVOLUTION", "EXTRUSION", "HYPERBOLA",
        "PARABOLA",
    ];
    table
        .type_names
        .iter()
        .map(|n| n.split(['_', '+']).any(|tok| STOP.contains(&tok)))
        .collect()
}

fn kinds_with_token(table: &EntityTable, token: &str) -> Vec<bool> {
    table.type_names.iter().map(|n| n.split(['_', '+']).any(|tok| tok == token)).collect()
}

/// Add the extent of a CIRCLE or ELLIPSE to the box. With the arc's two end
/// points known and distinct, a short arc (chord shorter than the radius)
/// is bounded by its end points and midpoint; otherwise the whole conic
/// (centre plus or minus the radii along the placement axes) is added.
/// Exporters write nearly straight edges as arcs of enormous radius, so the
/// full-circle bound must not be used for those.
fn add_conic_extent(
    data: &[u8],
    table: &EntityTable,
    e: &crate::scan::Entity,
    ellipse: bool,
    ends: Option<([f64; 3], [f64; 3])>,
    aabb: &mut Aabb,
) -> bool {
    let Ok(a) = parse_attrs(e.attrs(data)) else { return false };
    let Some(pl) = a.get(1).and_then(|v| v.as_id()).and_then(|p| axis2_placement(data, table, p)) else { return false };
    let r1 = a.get(2).and_then(|v| v.as_f64()).unwrap_or(0.0).abs();
    let r2 = if ellipse { a.get(3).and_then(|v| v.as_f64()).unwrap_or(r1).abs() } else { r1 };
    let c = pl.t;
    if let Some((p, q)) = ends {
        let chord = ((p[0] - q[0]).powi(2) + (p[1] - q[1]).powi(2) + (p[2] - q[2]).powi(2)).sqrt();
        let r = r1.max(r2);
        if chord > 1e-9 && chord < r {
            // Short arc: midpoint on the conic in the direction of the chord's middle.
            let m = [(p[0] + q[0]) * 0.5 - c[0], (p[1] + q[1]) * 0.5 - c[1], (p[2] + q[2]) * 0.5 - c[2]];
            if let Some(d) = normalize(m) {
                aabb.add([c[0] + d[0] * r, c[1] + d[1] * r, c[2] + d[2] * r]);
            }
            aabb.add(p);
            aabb.add(q);
            return true;
        }
    }
    let x = [pl.m[0][0], pl.m[1][0], pl.m[2][0]];
    let y = [pl.m[0][1], pl.m[1][1], pl.m[2][1]];
    for (sx, sy) in [(1.0, 1.0), (1.0, -1.0), (-1.0, 1.0), (-1.0, -1.0)] {
        aabb.add([
            c[0] + sx * r1 * x[0] + sy * r2 * y[0],
            c[1] + sx * r1 * x[1] + sy * r2 * y[1],
            c[2] + sx * r1 * x[2] + sy * r2 * y[2],
        ]);
    }
    true
}

/// Coordinates of a VERTEX_POINT's point.
fn vertex_coords(data: &[u8], table: &EntityTable, vertex: u32) -> Option<[f64; 3]> {
    let v = table.get(vertex)?;
    let a = parse_attrs(v.attrs(data)).ok()?;
    let p = table.get(a.get(1)?.as_id()?)?;
    let (c, dims) = parse_coords(p.attrs(data))?;
    if dims == 3 {
        Some(c)
    } else {
        None
    }
}

/// Per definition: the bounding box of its own bodies, walking references
/// from its shape representations down to the 3D points. Runs in parallel
/// over definitions; `on_done` is called per finished definition (from worker
/// threads) so a UI can show boxes as they arrive.
pub fn definition_bounds<F>(data: &[u8], table: &EntityTable, tree: &ProductTree, on_done: F) -> Vec<Option<DefBounds>>
where
    F: Fn(u32, &DefBounds) + Sync,
{
    let stop = stop_kinds(table);
    let circle = kinds_with_token(table, "CIRCLE");
    let ellipse = kinds_with_token(table, "ELLIPSE");
    let point_kind = table.kind_id("CARTESIAN_POINT");
    let edge_curve_kind = table.kind_id("EDGE_CURVE");
    let words = (table.max_id as usize / 64) + 1;
    let n = tree.definitions.len();
    // Largest definitions first so the work balances and big parts show first.
    let mut order: Vec<u32> = (0..n as u32).filter(|&d| tree.definitions[d as usize].bodies > 0).collect();
    order.sort_by_key(|&d| std::cmp::Reverse(tree.definitions[d as usize].shape_reps.iter().map(|&sr| table.get(sr).map(|e| e.len as u64).unwrap_or(0)).sum::<u64>()));

    let results: Vec<(u32, DefBounds)> = order
        .par_iter()
        .map(|&d| {
            let def = &tree.definitions[d as usize];
            let mut aabb = Aabb::EMPTY;
            let mut points = 0u32;
            let mut entities = 0u32;
            VISITED.with(|cell| {
                let mut visited = cell.borrow_mut();
                if visited.len() < words {
                    visited.resize(words, 0);
                }
                let mut touched: Vec<u32> = Vec::with_capacity(1024);
                let mut stack: Vec<u32> = def.shape_reps.clone();
                let mut refs: Vec<u32> = Vec::with_capacity(32);
                while let Some(id) = stack.pop() {
                    let w = (id / 64) as usize;
                    let bit = 1u64 << (id % 64);
                    if w >= visited.len() || visited[w] & bit != 0 {
                        continue;
                    }
                    visited[w] |= bit;
                    touched.push(id);
                    let Some(e) = table.get(id) else { continue };
                    entities += 1;
                    if Some(e.kind) == point_kind {
                        if let Some((p, dims)) = parse_coords(e.attrs(data)) {
                            if dims == 3 {
                                aabb.add(p);
                                points += 1;
                            }
                        }
                        continue;
                    }
                    let k = e.kind as usize;
                    if Some(e.kind) == edge_curve_kind {
                        // EDGE_CURVE(name, start, end, geometry, sense): the
                        // vertices bound the edge; arcs get their extent here,
                        // other curve geometry is walked normally.
                        if let Ok(a) = parse_attrs(e.attrs(data)) {
                            let sv = a.get(1).and_then(|v| v.as_id());
                            let ev = a.get(2).and_then(|v| v.as_id());
                            if let Some(s) = sv {
                                stack.push(s);
                            }
                            if let Some(t) = ev {
                                stack.push(t);
                            }
                            if let Some(g) = a.get(3).and_then(|v| v.as_id()).and_then(|g| table.get(g)) {
                                let gk = g.kind as usize;
                                if circle[gk] || ellipse[gk] {
                                    let ends = match (sv, ev) {
                                        (Some(s), Some(t)) if s != t => vertex_coords(data, table, s).zip(vertex_coords(data, table, t)),
                                        _ => None,
                                    };
                                    if add_conic_extent(data, table, g, ellipse[gk], ends, &mut aabb) {
                                        points += 3;
                                    }
                                } else if let Some(gid) = a.get(3).and_then(|v| v.as_id()) {
                                    stack.push(gid);
                                }
                            }
                        }
                        continue;
                    }
                    if circle[k] || ellipse[k] {
                        if add_conic_extent(data, table, e, ellipse[k], None, &mut aabb) {
                            points += 4;
                        }
                        continue;
                    }
                    if stop[k] {
                        continue;
                    }
                    refs.clear();
                    entity_refs(e.text(data), &mut refs);
                    stack.extend_from_slice(&refs);
                }
                for id in touched {
                    visited[(id / 64) as usize] &= !(1u64 << (id % 64));
                }
            });
            let b = DefBounds { aabb, points, entities };
            on_done(d, &b);
            (d, b)
        })
        .collect();
    let mut out = vec![None; n];
    for (d, b) in results {
        out[d as usize] = Some(b);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transform_roundtrip() {
        let t = Transform { m: [[0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]], t: [10.0, 20.0, 30.0] };
        let inv = t.inverse().unwrap();
        let p = [1.0, 2.0, 3.0];
        let q = inv.apply(t.apply(p));
        for i in 0..3 {
            assert!((q[i] - p[i]).abs() < 1e-12);
        }
        let id = t.mul(&inv);
        for i in 0..3 {
            for j in 0..3 {
                assert!((id.m[i][j] - if i == j { 1.0 } else { 0.0 }).abs() < 1e-12);
            }
            assert!(id.t[i].abs() < 1e-12);
        }
    }

    #[test]
    fn parses_coordinates() {
        assert_eq!(parse_coords(b"'',(1.,-2.5E1,.5)"), Some(([1.0, -25.0, 0.5], 3)));
        assert_eq!(parse_coords(b"'p',(0.1,0.2)"), Some(([0.1, 0.2, 0.0], 2)));
        assert_eq!(parse_coords(b"'NONE',( 1.0, 2.0, 3.0 )"), Some(([1.0, 2.0, 3.0], 3)));
    }

    #[test]
    fn extracts_refs() {
        let mut v = Vec::new();
        entity_refs(b"#5=SHAPE_REPRESENTATION('#99 not a ref',(#8,\n#9),#10);", &mut v);
        assert_eq!(v, vec![8, 9, 10]);
    }

    #[test]
    fn aabb_transform() {
        let b = Aabb { min: [0.0, 0.0, 0.0], max: [1.0, 2.0, 3.0] };
        let t = Transform { m: [[0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]], t: [5.0, 0.0, 0.0] };
        let r = b.transformed(&t);
        assert!((r.min[0] - 3.0).abs() < 1e-12 && (r.max[0] - 5.0).abs() < 1e-12);
        assert!((r.min[1] - 0.0).abs() < 1e-12 && (r.max[1] - 1.0).abs() < 1e-12);
    }
}
