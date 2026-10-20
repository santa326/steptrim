//! Pruning: remove assembly occurrences together with everything only they
//! needed, then write the survivors verbatim.
//!
//! The rules, applied to a fixpoint over a worklist:
//!
//! 1. Deletion propagates backwards. An entity that references a deleted
//!    entity through a single-valued attribute is deleted too (a shape of a
//!    removed usage, a property on it, a formation of a removed product).
//!    One that references it only inside an aggregate is kept and rewritten
//!    without that element; if an aggregate that had elements becomes empty
//!    the entity is deleted.
//! 2. Garbage is collected forwards. An entity that had referrers and has
//!    lost all of them is deleted (the transform of a removed occurrence,
//!    then its placements, a product's geometry once nothing points at it).
//!    Entities that were never referenced (graph roots such as
//!    SHAPE_DEFINITION_REPRESENTATION or layer assignments) stay unless
//!    rule 1 removes them.
//! 3. A definition that was only ever a child and has lost its last
//!    occurrence is orphaned: its PRODUCT_DEFINITION is deleted so the file
//!    does not grow an extra top-level product. Its own occurrences cascade
//!    by rule 1, so whole sub-assemblies disappear.
//! 4. Validation properties of a parent that lost a child are deleted, since
//!    their child counts and centroids no longer hold.

use crate::attr::{parse_attrs, parse_complex, write_complex, write_simple, Value};
use crate::geom::entity_refs;
use crate::scan::{Entity, EntityTable};
use crate::structure::ProductTree;
use rayon::prelude::*;
use std::collections::HashSet;
use std::io::Write;
use std::time::Instant;

#[derive(Clone, Debug, Default)]
pub struct PruneStats {
    pub removed_occurrences: usize,
    pub orphaned_definitions: usize,
    pub affected_parents: usize,
    pub deleted_properties: usize,
    pub deleted_entities: usize,
    pub rewritten_entities: usize,
    pub kept_entities: usize,
    /// Single-valued references to deleted entities found at write time;
    /// should be zero, counted as a self-check.
    pub dangling_single_refs: usize,
    pub graph_ms: f64,
    pub propagate_ms: f64,
}

pub struct PrunePlan {
    /// Per entity index.
    pub deleted: Vec<bool>,
    /// Per entity index: keep, but drop aggregate elements that reference
    /// deleted entities.
    pub rewrite: Vec<bool>,
    pub stats: PruneStats,
}

/// Forward and inverse reference graph over entity indices (CSR).
pub struct RefGraph {
    pub fwd_off: Vec<u32>,
    pub fwd: Vec<u32>,
    pub inv_off: Vec<u32>,
    pub inv: Vec<u32>,
    /// Number of original referrers per entity (with multiplicity).
    pub refcount: Vec<u32>,
    /// Number of original referrers that are owners, i.e. not annotation
    /// entities (styles, layers, categories, properties, documents,
    /// approvals...). Annotation references point at things they decorate
    /// and must not keep them alive.
    pub owners: Vec<u32>,
    /// Per entity: is it an annotation-kind entity?
    pub annotation: Vec<bool>,
    /// References whose target id does not exist in the file.
    pub unresolved: usize,
}

/// Kinds whose references decorate rather than own their targets, by
/// whole `_`-separated token. Relationship entities stay owners: a
/// SHAPE_REPRESENTATION_RELATIONSHIP owns the B-rep representation it links
/// to a product's placement representation (SolidWorks files have no other
/// owner for it). The deadlock this would create for an orphaned product,
/// whose representation is pinned by the relationship and vice versa, is
/// broken by deleting an orphaned definition's representations explicitly.
fn annotation_kinds(table: &EntityTable) -> Vec<bool> {
    const ANNOT: &[&str] = &[
        "STYLED", "STYLE", "PRESENTATION", "DRAUGHTING", "CATEGORY", "PROPERTY", "ASPECT", "APPLIED", "LAYER", "COLOUR",
        "ANNOTATION", "DIMENSIONAL", "TOLERANCE", "DATUM", "DOCUMENT", "APPROVAL", "SECURITY", "DATE", "PERSON",
        "ORGANIZATION", "CERTIFICATION", "CONTRACT", "EFFECTIVITY", "CAMERA", "VIEW", "APPLICATION", "CONFIGURATION",
        "ALTERNATE", "MAKE", "SUPPLIED", "ACTION", "EVENT",
    ];
    table.type_names.iter().map(|n| n.split(['_', '+']).any(|tok| ANNOT.contains(&tok))).collect()
}

impl RefGraph {
    pub fn targets(&self, i: usize) -> &[u32] {
        &self.fwd[self.fwd_off[i] as usize..self.fwd_off[i + 1] as usize]
    }
    pub fn referrers(&self, i: usize) -> &[u32] {
        &self.inv[self.inv_off[i] as usize..self.inv_off[i + 1] as usize]
    }
}

/// Build the reference graph in parallel.
pub fn build_graph(data: &[u8], table: &EntityTable) -> RefGraph {
    let n = table.len();
    const CH: usize = 32768;
    let chunks: Vec<(Vec<u32>, Vec<u32>, usize)> = table
        .entities
        .par_chunks(CH)
        .map(|c| {
            let mut counts = Vec::with_capacity(c.len());
            let mut targets = Vec::with_capacity(c.len() * 3);
            let mut refs = Vec::with_capacity(32);
            let mut unresolved = 0usize;
            for e in c {
                refs.clear();
                entity_refs(e.text(data), &mut refs);
                let mut k = 0u32;
                for &id in &refs {
                    match table.index_of(id) {
                        Some(t) => {
                            targets.push(t as u32);
                            k += 1;
                        }
                        None => unresolved += 1,
                    }
                }
                counts.push(k);
            }
            (counts, targets, unresolved)
        })
        .collect();
    let total: usize = chunks.iter().map(|c| c.1.len()).sum();
    let unresolved: usize = chunks.iter().map(|c| c.2).sum();
    let mut fwd_off = Vec::with_capacity(n + 1);
    let mut fwd = Vec::with_capacity(total);
    fwd_off.push(0u32);
    let mut acc = 0u32;
    for (counts, targets, _) in chunks {
        for c in counts {
            acc += c;
            fwd_off.push(acc);
        }
        fwd.extend_from_slice(&targets);
    }
    let annotation_kind = annotation_kinds(table);
    let annotation: Vec<bool> = table.entities.iter().map(|e| annotation_kind[e.kind as usize]).collect();
    let mut refcount = vec![0u32; n];
    let mut owners = vec![0u32; n];
    for src in 0..n {
        let own = !annotation[src];
        for &t in &fwd[fwd_off[src] as usize..fwd_off[src + 1] as usize] {
            refcount[t as usize] += 1;
            if own {
                owners[t as usize] += 1;
            }
        }
    }
    let mut inv_off = Vec::with_capacity(n + 1);
    inv_off.push(0u32);
    let mut acc = 0u32;
    for &c in &refcount {
        acc += c;
        inv_off.push(acc);
    }
    let mut fill: Vec<u32> = inv_off[..n].to_vec();
    let mut inv = vec![0u32; total];
    for src in 0..n {
        for &t in &fwd[fwd_off[src] as usize..fwd_off[src + 1] as usize] {
            let p = &mut fill[t as usize];
            inv[*p as usize] = src as u32;
            *p += 1;
        }
    }
    RefGraph { fwd_off, fwd, inv_off, inv, refcount, owners, annotation, unresolved }
}

/// Does `text` reference `target` only inside aggregates (true) or at least
/// once as a single-valued attribute (false)? `complex` instances have one
/// extra level of parentheses around their partial records.
fn ref_only_nested(text: &[u8], complex: bool, target: u32) -> bool {
    let base = if complex { 2 } else { 1 };
    let mut depth = 0i32;
    let mut in_str = false;
    let mut i = text.iter().position(|&b| b == b'=').map(|p| p + 1).unwrap_or(0);
    let mut found_single = false;
    let mut found_any = false;
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
            b'\'' => in_str = true,
            b'(' => depth += 1,
            b')' => depth -= 1,
            b'#' => {
                let mut v: u32 = 0;
                let mut e = i + 1;
                while e < text.len() && text[e].is_ascii_digit() {
                    v = v.wrapping_mul(10).wrapping_add((text[e] - b'0') as u32);
                    e += 1;
                }
                if e > i + 1 && v == target {
                    found_any = true;
                    if depth <= base {
                        found_single = true;
                    }
                }
                i = e;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    found_any && !found_single
}

/// Filter aggregates: drop references to deleted entities. Returns true if
/// an aggregate that had elements is now empty.
fn filter_values(values: &mut Vec<Value<'_>>, deleted: &dyn Fn(u32) -> bool, dangling: &mut usize) -> bool {
    let mut emptied = false;
    for v in values.iter_mut() {
        match v {
            Value::List(l) => {
                let before = l.len();
                l.retain(|x| !matches!(x, Value::Ref(r) if deleted(*r)));
                if filter_nested(l, deleted, dangling) {
                    emptied = true;
                }
                if before > 0 && l.is_empty() {
                    emptied = true;
                }
            }
            Value::Ref(r) if deleted(*r) => {
                *dangling += 1;
                *v = Value::Null;
            }
            Value::Typed(_, inner) => {
                if let Value::Ref(r) = **inner {
                    if deleted(r) {
                        *dangling += 1;
                        **inner = Value::Null;
                    }
                }
            }
            _ => {}
        }
    }
    emptied
}

fn filter_nested(values: &mut Vec<Value<'_>>, deleted: &dyn Fn(u32) -> bool, dangling: &mut usize) -> bool {
    let mut emptied = false;
    for v in values.iter_mut() {
        if let Value::List(l) = v {
            let before = l.len();
            l.retain(|x| !matches!(x, Value::Ref(r) if deleted(*r)));
            if filter_nested(l, deleted, dangling) {
                emptied = true;
            }
            if before > 0 && l.is_empty() {
                emptied = true;
            }
        }
    }
    emptied
}

/// Rewritten text of an entity with references to deleted entities dropped
/// from its aggregates. Returns (text, an aggregate became empty).
fn rewrite_entity(data: &[u8], e: &Entity, type_name: &str, deleted: &dyn Fn(u32) -> bool, dangling: &mut usize) -> Option<(Vec<u8>, bool)> {
    let mut out = Vec::with_capacity(e.len as usize);
    if e.complex {
        let mut parts = parse_complex(e.attrs(data)).ok()?;
        let mut emptied = false;
        for (_, attrs) in parts.iter_mut() {
            if filter_values(attrs, deleted, dangling) {
                emptied = true;
            }
        }
        write_complex(&mut out, e.id as u64, &parts, 0);
        Some((out, emptied))
    } else {
        let mut attrs = parse_attrs(e.attrs(data)).ok()?;
        let emptied = filter_values(&mut attrs, deleted, dangling);
        write_simple(&mut out, e.id as u64, type_name, &attrs, 0);
        Some((out, emptied))
    }
}

/// What to prune. Besides removing occurrences, a request can promote a
/// definition to a root (`keep_definitions`: it survives even when it has
/// lost every occurrence, which is how a part is exported on its own) and
/// delete definitions outright (`delete_definitions`: the old root, so the
/// exported file has a single root).
#[derive(Clone, Debug, Default)]
pub struct PruneRequest {
    /// Indices into `tree.occurrences`.
    pub remove_occurrences: Vec<u32>,
    /// Indices into `tree.definitions`.
    pub keep_definitions: Vec<u32>,
    /// Indices into `tree.definitions`.
    pub delete_definitions: Vec<u32>,
}

/// A definition and every definition below it in the tree.
pub fn descendant_definitions(tree: &ProductTree, def: u32) -> HashSet<u32> {
    let mut out = HashSet::new();
    let mut stack = vec![def];
    while let Some(d) = stack.pop() {
        if !out.insert(d) {
            continue;
        }
        for &o in &tree.children[d as usize] {
            stack.push(tree.occurrences[o as usize].child);
        }
    }
    out
}

/// Request for exporting one definition as a standalone file: every
/// occurrence outside its subtree goes, it becomes a root, and the current
/// roots are deleted.
pub fn standalone_request(tree: &ProductTree, def: u32) -> PruneRequest {
    let inside = descendant_definitions(tree, def);
    let remove: Vec<u32> = tree
        .occurrences
        .iter()
        .enumerate()
        .filter(|(_, o)| !inside.contains(&o.parent))
        .map(|(i, _)| i as u32)
        .collect();
    let delete: Vec<u32> = tree.roots.iter().copied().filter(|&r| r != def && !inside.contains(&r)).collect();
    PruneRequest { remove_occurrences: remove, keep_definitions: vec![def], delete_definitions: delete }
}

/// Plan the removal of the given occurrences (indices into `tree.occurrences`).
pub fn plan(data: &[u8], table: &EntityTable, tree: &ProductTree, graph: &RefGraph, remove_occurrences: &[u32]) -> PrunePlan {
    plan_request(data, table, tree, graph, &PruneRequest { remove_occurrences: remove_occurrences.to_vec(), ..Default::default() })
}

/// Plan a full request.
pub fn plan_request(data: &[u8], table: &EntityTable, tree: &ProductTree, graph: &RefGraph, req: &PruneRequest) -> PrunePlan {
    let remove_occurrences = &req.remove_occurrences;
    let keep: HashSet<u32> = req.keep_definitions.iter().copied().collect();
    let n = table.len();
    let mut stats = PruneStats::default();
    let t0 = Instant::now();
    let mut deleted = vec![false; n];
    let mut rewrite = vec![false; n];
    let mut refcount = graph.refcount.clone();
    let mut owners = graph.owners.clone();
    let mut work: Vec<u32> = Vec::new();

    let mut occ_removed = vec![false; tree.occurrences.len()];
    for &o in remove_occurrences {
        if let Some(i) = table.index_of(tree.occurrences[o as usize].entity) {
            if !deleted[i] {
                deleted[i] = true;
                work.push(i as u32);
                occ_removed[o as usize] = true;
                stats.removed_occurrences += 1;
            }
        }
    }
    for &d in &req.delete_definitions {
        let def = &tree.definitions[d as usize];
        let mut ids = vec![def.entity];
        ids.extend_from_slice(&def.shape_reps);
        for id in ids {
            if let Some(i) = table.index_of(id) {
                if !deleted[i] {
                    deleted[i] = true;
                    work.push(i as u32);
                }
            }
        }
    }
    let affected_parents: HashSet<u32> = remove_occurrences.iter().map(|&o| tree.occurrences[o as usize].parent).collect();

    // Validation and other properties on parents that lost children.
    let mut prop_targets: HashSet<u32> = HashSet::new();
    for &p in &affected_parents {
        let pd = tree.definitions[p as usize].entity;
        prop_targets.insert(pd);
    }
    if let Some(k) = table.kind_id("PRODUCT_DEFINITION_SHAPE") {
        for &i in table.of_kind(k) {
            let e = &table.entities[i as usize];
            if let Ok(a) = parse_attrs(e.attrs(data)) {
                if let Some(d) = a.get(2).and_then(|v| v.as_id()) {
                    if affected_parents.iter().any(|&p| tree.definitions[p as usize].entity == d) {
                        prop_targets.insert(e.id);
                    }
                }
            }
        }
    }
    for k in table.kinds_containing("PROPERTY_DEFINITION") {
        let name = table.kind_name(k);
        if name.split('+').any(|p| p == "PROPERTY_DEFINITION") {
            for &i in table.of_kind(k) {
                let e = &table.entities[i as usize];
                let def = if e.complex {
                    parse_complex(e.attrs(data)).ok().and_then(|parts| {
                        parts.iter().find(|(n, _)| *n == b"PROPERTY_DEFINITION").and_then(|(_, a)| a.get(2).and_then(|v| v.as_id()))
                    })
                } else {
                    parse_attrs(e.attrs(data)).ok().and_then(|a| a.get(2).and_then(|v| v.as_id()))
                };
                if let Some(d) = def {
                    if prop_targets.contains(&d) && !deleted[i as usize] {
                        deleted[i as usize] = true;
                        work.push(i);
                        stats.deleted_properties += 1;
                    }
                }
            }
        }
    }
    stats.affected_parents = affected_parents.len();

    // Original child status per definition and the NAUO entity index per occurrence.
    let mut was_child = vec![false; tree.definitions.len()];
    for o in &tree.occurrences {
        was_child[o.child as usize] = true;
    }
    let occ_index: Vec<Option<usize>> = tree.occurrences.iter().map(|o| table.index_of(o.entity)).collect();
    let def_index: Vec<Option<usize>> = tree.definitions.iter().map(|d| table.index_of(d.entity)).collect();

    loop {
        // Propagate.
        while let Some(x) = work.pop() {
            let xi = x as usize;
            let x_id = table.entities[xi].id;
            for &r in graph.referrers(xi) {
                let ri = r as usize;
                if deleted[ri] {
                    continue;
                }
                let re = &table.entities[ri];
                if ref_only_nested(re.text(data), re.complex, x_id) {
                    rewrite[ri] = true;
                } else {
                    deleted[ri] = true;
                    work.push(r);
                }
            }
            let x_owns = !graph.annotation[xi];
            for &t in graph.targets(xi) {
                let ti = t as usize;
                refcount[ti] = refcount[ti].saturating_sub(1);
                if x_owns {
                    owners[ti] = owners[ti].saturating_sub(1);
                }
                let lost_all = refcount[ti] == 0 && graph.refcount[ti] > 0;
                let lost_owners = owners[ti] == 0 && graph.owners[ti] > 0;
                if (lost_all || lost_owners) && !deleted[ti] {
                    deleted[ti] = true;
                    work.push(t);
                }
            }
        }
        // Rewrites that emptied an aggregate become deletions.
        let mut changed = false;
        let mut emptied_idx: Vec<usize> = Vec::new();
        {
            let is_deleted = |id: u32| table.index_of(id).map(|i| deleted[i]).unwrap_or(false);
            for i in 0..n {
                if rewrite[i] && !deleted[i] {
                    let e = &table.entities[i];
                    let mut dangling = 0usize;
                    if let Some((_, emptied)) = rewrite_entity(data, e, table.kind_name(e.kind), &is_deleted, &mut dangling) {
                        if emptied {
                            emptied_idx.push(i);
                        }
                    }
                }
            }
        }
        for i in emptied_idx {
            deleted[i] = true;
            rewrite[i] = false;
            work.push(i as u32);
            changed = true;
        }
        // Orphaned definitions.
        for (d, def) in tree.definitions.iter().enumerate() {
            if !was_child[d] || keep.contains(&(d as u32)) {
                continue;
            }
            let Some(di) = def_index[d] else { continue };
            if deleted[di] {
                continue;
            }
            let alive = tree
                .occurrences
                .iter()
                .enumerate()
                .any(|(o, occ)| occ.child as usize == d && occ_index[o].map(|i| !deleted[i]).unwrap_or(false));
            if !alive {
                deleted[di] = true;
                work.push(di as u32);
                // Its representations go with it (see annotation_kinds).
                for &sr in &def.shape_reps {
                    if let Some(si) = table.index_of(sr) {
                        if !deleted[si] {
                            deleted[si] = true;
                            work.push(si as u32);
                        }
                    }
                }
                stats.orphaned_definitions += 1;
                changed = true;
            }
        }
        if !changed && work.is_empty() {
            break;
        }
    }
    stats.propagate_ms = t0.elapsed().as_secs_f64() * 1000.0;
    stats.deleted_entities = deleted.iter().filter(|&&d| d).count();
    stats.rewritten_entities = rewrite.iter().zip(&deleted).filter(|(&r, &d)| r && !d).count();
    stats.kept_entities = n - stats.deleted_entities;
    let _ = occ_removed;
    PrunePlan { deleted, rewrite, stats }
}

#[derive(Clone, Debug, Default)]
pub struct WriteStats {
    pub bytes: u64,
    pub kept: usize,
    pub deleted: usize,
    pub rewritten: usize,
    pub dangling_single_refs: usize,
    pub write_ms: f64,
}

/// Write the surviving entities verbatim (rewritten ones re-serialized),
/// with the original header and trailer.
pub fn write_pruned<W: Write>(data: &[u8], table: &EntityTable, plan: &PrunePlan, out: &mut W) -> std::io::Result<WriteStats> {
    let t0 = Instant::now();
    let head = &data[..table.data_start];
    let eol: &[u8] = if head.windows(2).any(|w| w == b"\r\n") { b"\r\n" } else { b"\n" };
    let mut stats = WriteStats::default();
    out.write_all(head)?;
    out.write_all(eol)?;
    stats.bytes += (head.len() + eol.len()) as u64;

    let is_deleted = |id: u32| table.index_of(id).map(|i| plan.deleted[i]).unwrap_or(false);
    const CH: usize = 32768;
    let bufs: Vec<(Vec<u8>, usize, usize, usize)> = table
        .entities
        .par_chunks(CH)
        .enumerate()
        .map(|(ci, chunk)| {
            let mut buf = Vec::with_capacity(chunk.iter().map(|e| e.len as usize + 2).sum());
            let (mut kept, mut rewritten, mut dangling) = (0usize, 0usize, 0usize);
            for (j, e) in chunk.iter().enumerate() {
                let i = ci * CH + j;
                if plan.deleted[i] {
                    continue;
                }
                if plan.rewrite[i] {
                    match rewrite_entity(data, e, table.kind_name(e.kind), &is_deleted, &mut dangling) {
                        Some((text, _)) => {
                            buf.extend_from_slice(&text);
                            rewritten += 1;
                        }
                        None => buf.extend_from_slice(e.text(data)),
                    }
                } else {
                    buf.extend_from_slice(e.text(data));
                }
                buf.extend_from_slice(eol);
                kept += 1;
            }
            (buf, kept, rewritten, dangling)
        })
        .collect();
    for (buf, kept, rewritten, dangling) in bufs {
        out.write_all(&buf)?;
        stats.bytes += buf.len() as u64;
        stats.kept += kept;
        stats.rewritten += rewritten;
        stats.dangling_single_refs += dangling;
    }
    let tail = &data[table.data_end..];
    out.write_all(tail)?;
    stats.bytes += tail.len() as u64;
    stats.deleted = table.len() - stats.kept;
    stats.write_ms = t0.elapsed().as_secs_f64() * 1000.0;
    Ok(stats)
}

/// Reference integrity check over a whole file: every `#n` resolves.
pub fn check_refs(data: &[u8], table: &EntityTable) -> (usize, usize, Vec<(u32, u32)>) {
    let results: Vec<(usize, usize, Vec<(u32, u32)>)> = table
        .entities
        .par_chunks(32768)
        .map(|chunk| {
            let mut refs = Vec::with_capacity(32);
            let (mut total, mut bad) = (0usize, 0usize);
            let mut examples = Vec::new();
            for e in chunk {
                refs.clear();
                entity_refs(e.text(data), &mut refs);
                total += refs.len();
                for &r in &refs {
                    if table.index_of(r).is_none() {
                        bad += 1;
                        if examples.len() < 5 {
                            examples.push((e.id, r));
                        }
                    }
                }
            }
            (total, bad, examples)
        })
        .collect();
    let mut total = 0;
    let mut bad = 0;
    let mut examples = Vec::new();
    for (t, b, ex) in results {
        total += t;
        bad += b;
        if examples.len() < 5 {
            examples.extend(ex);
        }
    }
    examples.truncate(5);
    (total, bad, examples)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{scan, ScanConfig};

    // as1-like: root with bolt x2 and plate; plate contains bolt. Bolt and
    // plate have geometry; a layer lists both solids; a validation property
    // sits on the root.
    const FILE: &str = "ISO-10303-21;\nHEADER;\nFILE_SCHEMA(('AUTOMOTIVE_DESIGN'));\nENDSEC;\nDATA;\n\
#1=PRODUCT('as1','as1','',(#2));\n#2=PRODUCT_CONTEXT('',#3,'');\n#3=APPLICATION_CONTEXT('x');\n\
#4=PRODUCT_DEFINITION_FORMATION('','',#1);\n#5=PRODUCT_DEFINITION('design','',#4,#6);\n#6=PRODUCT_DEFINITION_CONTEXT('',#3,'');\n\
#7=PRODUCT_DEFINITION_SHAPE('','',#5);\n#8=SHAPE_DEFINITION_REPRESENTATION(#7,#9);\n#9=SHAPE_REPRESENTATION('',(#60,#61,#62),#90);\n\
#10=PRODUCT('bolt','Bolt','',(#2));\n#11=PRODUCT_DEFINITION_FORMATION('','',#10);\n#12=PRODUCT_DEFINITION('design','',#11,#6);\n\
#13=PRODUCT_DEFINITION_SHAPE('','',#12);\n#14=SHAPE_DEFINITION_REPRESENTATION(#13,#15);\n#15=ADVANCED_BREP_SHAPE_REPRESENTATION('',(#60,#16),#90);\n\
#16=MANIFOLD_SOLID_BREP('',#17);\n#17=CLOSED_SHELL('',(#18));\n#18=ADVANCED_FACE('',(),#19,.T.);\n#19=PLANE('',#60);\n\
#20=PRODUCT('plate','Plate','',(#2));\n#21=PRODUCT_DEFINITION_FORMATION('','',#20);\n#22=PRODUCT_DEFINITION('design','',#21,#6);\n\
#23=PRODUCT_DEFINITION_SHAPE('','',#22);\n#24=SHAPE_DEFINITION_REPRESENTATION(#23,#25);\n#25=ADVANCED_BREP_SHAPE_REPRESENTATION('',(#60,#26,#63),#90);\n\
#26=MANIFOLD_SOLID_BREP('',#27);\n#27=CLOSED_SHELL('',(#28));\n#28=ADVANCED_FACE('',(),#29,.T.);\n#29=PLANE('',#60);\n\
#30=NEXT_ASSEMBLY_USAGE_OCCURRENCE('o1','bolt 1','',#5,#12,$);\n#31=PRODUCT_DEFINITION_SHAPE('','',#30);\n#32=CONTEXT_DEPENDENT_SHAPE_REPRESENTATION(#33,#31);\n\
#33=(REPRESENTATION_RELATIONSHIP('','',#15,#9)REPRESENTATION_RELATIONSHIP_WITH_TRANSFORMATION(#34)SHAPE_REPRESENTATION_RELATIONSHIP());\n#34=ITEM_DEFINED_TRANSFORMATION('','',#60,#61);\n\
#35=NEXT_ASSEMBLY_USAGE_OCCURRENCE('o2','bolt 2','',#5,#12,$);\n#36=PRODUCT_DEFINITION_SHAPE('','',#35);\n#37=CONTEXT_DEPENDENT_SHAPE_REPRESENTATION(#38,#36);\n\
#38=(REPRESENTATION_RELATIONSHIP('','',#15,#9)REPRESENTATION_RELATIONSHIP_WITH_TRANSFORMATION(#39)SHAPE_REPRESENTATION_RELATIONSHIP());\n#39=ITEM_DEFINED_TRANSFORMATION('','',#60,#62);\n\
#40=NEXT_ASSEMBLY_USAGE_OCCURRENCE('o3','plate','',#5,#22,$);\n#41=PRODUCT_DEFINITION_SHAPE('','',#40);\n#42=CONTEXT_DEPENDENT_SHAPE_REPRESENTATION(#43,#41);\n\
#43=(REPRESENTATION_RELATIONSHIP('','',#25,#9)REPRESENTATION_RELATIONSHIP_WITH_TRANSFORMATION(#44)SHAPE_REPRESENTATION_RELATIONSHIP());\n#44=ITEM_DEFINED_TRANSFORMATION('','',#60,#60);\n\
#45=NEXT_ASSEMBLY_USAGE_OCCURRENCE('o4','bolt in plate','',#22,#12,$);\n#46=PRODUCT_DEFINITION_SHAPE('','',#45);\n#47=CONTEXT_DEPENDENT_SHAPE_REPRESENTATION(#48,#46);\n\
#48=(REPRESENTATION_RELATIONSHIP('','',#15,#25)REPRESENTATION_RELATIONSHIP_WITH_TRANSFORMATION(#49)SHAPE_REPRESENTATION_RELATIONSHIP());\n#49=ITEM_DEFINED_TRANSFORMATION('','',#60,#63);\n\
#60=AXIS2_PLACEMENT_3D('',#70,$,$);\n#61=AXIS2_PLACEMENT_3D('',#71,$,$);\n#62=AXIS2_PLACEMENT_3D('',#72,$,$);\n#63=AXIS2_PLACEMENT_3D('',#73,$,$);\n\
#70=CARTESIAN_POINT('',(0.,0.,0.));\n#71=CARTESIAN_POINT('',(10.,0.,0.));\n#72=CARTESIAN_POINT('',(20.,0.,0.));\n#73=CARTESIAN_POINT('',(5.,5.,0.));\n\
#80=PRESENTATION_LAYER_ASSIGNMENT('1','layer',(#16,#26));\n#81=PRODUCT_RELATED_PRODUCT_CATEGORY('part',$,(#10,#20));\n\
#82=PROPERTY_DEFINITION('assembly validation property','',#7);\n#83=PROPERTY_DEFINITION_REPRESENTATION(#82,#84);\n#84=REPRESENTATION('number of children',(#85),#90);\n#85=INTEGER_REPRESENTATION_ITEM('number of children',3);\n\
#90=GEOMETRIC_REPRESENTATION_CONTEXT(3);\n\
ENDSEC;\nEND-ISO-10303-21;\n";

    fn ids_kept(plan: &PrunePlan, table: &EntityTable) -> Vec<u32> {
        table.entities.iter().enumerate().filter(|(i, _)| !plan.deleted[*i]).map(|(_, e)| e.id).collect()
    }

    #[test]
    fn removing_one_bolt_keeps_bolt_product() {
        let data = FILE.as_bytes();
        let table = scan(data, &ScanConfig::default()).unwrap();
        let tree = crate::structure::build(data, &table);
        let graph = build_graph(data, &table);
        // occurrence o1 (#30)
        let o1 = tree.occurrences.iter().position(|o| o.entity == 30).unwrap() as u32;
        let plan = plan(data, &table, &tree, &graph, &[o1]);
        let kept = ids_kept(&plan, &table);
        for gone in [30, 31, 32, 33, 34, 82, 83, 84, 85] {
            assert!(!kept.contains(&gone), "#{gone} should be deleted");
        }
        // bolt still used by o2 and o4: product and geometry survive; placements survive via SR items.
        for stay in [10, 11, 12, 13, 14, 15, 16, 61, 71, 9, 5, 80, 81, 90] {
            assert!(kept.contains(&stay), "#{stay} should survive");
        }
        assert_eq!(plan.stats.orphaned_definitions, 0);
        assert_eq!(plan.stats.deleted_properties, 1);
        let mut out = Vec::new();
        let ws = write_pruned(data, &table, &plan, &mut out).unwrap();
        assert_eq!(ws.dangling_single_refs, 0);
        let t2 = scan(&out, &ScanConfig::default()).unwrap();
        let (_, bad, _) = check_refs(&out, &t2);
        assert_eq!(bad, 0);
        let tree2 = crate::structure::build(&out, &t2);
        assert_eq!(tree2.occurrences.len(), 3);
        assert_eq!(tree2.roots.len(), 1);
    }

    #[test]
    fn removing_plate_orphans_plate_and_keeps_bolt() {
        let data = FILE.as_bytes();
        let table = scan(data, &ScanConfig::default()).unwrap();
        let tree = crate::structure::build(data, &table);
        let graph = build_graph(data, &table);
        let o3 = tree.occurrences.iter().position(|o| o.entity == 40).unwrap() as u32;
        let plan = plan(data, &table, &tree, &graph, &[o3]);
        let kept = ids_kept(&plan, &table);
        // plate product, its geometry, its own occurrence o4 and the layer entry are gone
        for gone in [40, 41, 42, 43, 44, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 45, 46, 47, 48, 49, 63, 73] {
            assert!(!kept.contains(&gone), "#{gone} should be deleted");
        }
        for stay in [10, 12, 15, 16, 30, 35, 80, 81, 60, 70] {
            assert!(kept.contains(&stay), "#{stay} should survive");
        }
        assert_eq!(plan.stats.orphaned_definitions, 1);
        let mut out = Vec::new();
        write_pruned(data, &table, &plan, &mut out).unwrap();
        let text = String::from_utf8(out.clone()).unwrap();
        // the layer and category lists were rewritten without the plate entries
        assert!(text.contains("#80=PRESENTATION_LAYER_ASSIGNMENT('1','layer',(#16));"));
        assert!(text.contains("#81=PRODUCT_RELATED_PRODUCT_CATEGORY('part',$,(#10));"));
        let t2 = scan(&out, &ScanConfig::default()).unwrap();
        assert_eq!(check_refs(&out, &t2).1, 0);
        let tree2 = crate::structure::build(&out, &t2);
        assert_eq!(tree2.occurrences.len(), 2);
        assert_eq!(tree2.roots.len(), 1);
        assert_eq!(tree2.total_instances(), 3);
    }

    #[test]
    fn exports_a_part_standalone() {
        // Export the plate (which contains a bolt) on its own: as1 and the
        // top-level bolts go, the plate becomes the only root with its bolt.
        let data = FILE.as_bytes();
        let table = scan(data, &ScanConfig::default()).unwrap();
        let tree = crate::structure::build(data, &table);
        let graph = build_graph(data, &table);
        let plate = tree.definitions.iter().position(|d| d.entity == 22).unwrap() as u32;
        let req = standalone_request(&tree, plate);
        assert_eq!(req.keep_definitions, vec![plate]);
        assert_eq!(req.delete_definitions.len(), 1);
        let plan = plan_request(data, &table, &tree, &graph, &req);
        let kept = ids_kept(&plan, &table);
        for gone in [1, 4, 5, 7, 8, 9, 30, 31, 35, 36, 40, 41, 82, 83] {
            assert!(!kept.contains(&gone), "#{gone} should be deleted");
        }
        for stay in [20, 21, 22, 23, 24, 25, 26, 45, 46, 47, 48, 49, 10, 12, 15, 16, 63, 73, 90] {
            assert!(kept.contains(&stay), "#{stay} should survive");
        }
        let mut out = Vec::new();
        let ws = write_pruned(data, &table, &plan, &mut out).unwrap();
        assert_eq!(ws.dangling_single_refs, 0);
        let t2 = scan(&out, &ScanConfig::default()).unwrap();
        assert_eq!(check_refs(&out, &t2).1, 0);
        let tree2 = crate::structure::build(&out, &t2);
        assert_eq!(tree2.roots.len(), 1);
        assert_eq!(tree2.definition_name(tree2.roots[0]), "Plate");
        assert_eq!(tree2.occurrences.len(), 1);
        assert_eq!(tree2.total_instances(), 2);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("#81=PRODUCT_RELATED_PRODUCT_CATEGORY('part',$,(#10,#20));"));
    }

    #[test]
    fn removing_all_bolts_drops_bolt_product() {
        let data = FILE.as_bytes();
        let table = scan(data, &ScanConfig::default()).unwrap();
        let tree = crate::structure::build(data, &table);
        let graph = build_graph(data, &table);
        let occs: Vec<u32> = tree.occurrences.iter().enumerate().filter(|(_, o)| [30u32, 35, 45].contains(&o.entity)).map(|(i, _)| i as u32).collect();
        let plan = plan(data, &table, &tree, &graph, &occs);
        let kept = ids_kept(&plan, &table);
        for gone in [10, 11, 12, 13, 14, 15, 16, 17, 18, 19] {
            assert!(!kept.contains(&gone), "#{gone} should be deleted");
        }
        assert!(kept.contains(&20) && kept.contains(&26) && kept.contains(&40));
        let mut out = Vec::new();
        write_pruned(data, &table, &plan, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("#80=PRESENTATION_LAYER_ASSIGNMENT('1','layer',(#26));"));
    }
}
