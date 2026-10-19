//! Product structure: products, definitions and assembly occurrences.
//!
//! Only the handful of structure entity types are parsed, so this runs in
//! milliseconds even on multi-gigabyte files once the entity table exists.

use crate::attr::{parse_attrs, parse_complex, Value};
use crate::scan::{EntityTable, TypeId};
use serde::Serialize;
use std::collections::HashMap;

#[derive(Clone, Debug, Serialize)]
pub struct Product {
    pub entity: u32,
    pub pid: String,
    pub name: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Definition {
    pub entity: u32,
    /// Index into `products`, if the formation chain resolved.
    pub product: Option<u32>,
    pub did: String,
    pub description: String,
    /// True when some PRODUCT_DEFINITION_SHAPE points at this definition.
    pub has_shape: bool,
    /// Entity ids of the shape representations that carry this definition's
    /// geometry: the one named by SHAPE_DEFINITION_REPRESENTATION plus any
    /// linked to it by a plain SHAPE_REPRESENTATION_RELATIONSHIP.
    pub shape_reps: Vec<u32>,
    /// Number of body items (solids, shells, geometric sets) across those
    /// representations. CAD systems show a part with several bodies as a
    /// container with one feature per body.
    pub bodies: u32,
}

#[derive(Clone, Debug, Serialize)]
pub struct Occurrence {
    /// The NEXT_ASSEMBLY_USAGE_OCCURRENCE entity id.
    pub entity: u32,
    /// Index into `definitions` of the parent (relating) definition.
    pub parent: u32,
    /// Index into `definitions` of the child (related) definition.
    pub child: u32,
    pub oid: String,
    pub name: String,
    pub refdes: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct StructureWarnings {
    pub nauo_unresolved: usize,
    pub nauo_malformed: usize,
    pub definition_without_product: usize,
    pub cycles_broken: usize,
}

#[derive(Debug, Serialize)]
pub struct ProductTree {
    pub products: Vec<Product>,
    pub definitions: Vec<Definition>,
    pub occurrences: Vec<Occurrence>,
    /// Per definition: indices into `occurrences` whose parent is that
    /// definition, sorted by child name.
    pub children: Vec<Vec<u32>>,
    /// Definitions that are never a child, sorted by expanded instance count
    /// (largest first).
    pub roots: Vec<u32>,
    /// Per definition: number of leaf-and-branch instances in its expanded
    /// subtree, including itself.
    pub instance_count: Vec<u64>,
    /// Per definition: depth of its expanded subtree (a leaf is 1).
    pub depth: Vec<u32>,
    pub warnings: StructureWarnings,
}

impl ProductTree {
    pub fn definition_name(&self, d: u32) -> &str {
        let def = &self.definitions[d as usize];
        match def.product {
            Some(p) => {
                let pr = &self.products[p as usize];
                if !pr.name.is_empty() {
                    &pr.name
                } else if !pr.pid.is_empty() {
                    &pr.pid
                } else {
                    &def.did
                }
            }
            None => &def.did,
        }
    }
    pub fn definition_pid(&self, d: u32) -> &str {
        match self.definitions[d as usize].product {
            Some(p) => &self.products[p as usize].pid,
            None => "",
        }
    }
    /// Total expanded instances under all roots.
    pub fn total_instances(&self) -> u64 {
        self.roots.iter().map(|&r| self.instance_count[r as usize]).sum()
    }
    pub fn max_depth(&self) -> u32 {
        self.roots.iter().map(|&r| self.depth[r as usize]).max().unwrap_or(0)
    }
    /// Definitions that appear as a child in at least one occurrence or are a
    /// root with children: the ones a CAD tree would show.
    pub fn used_definitions(&self) -> usize {
        let mut used = vec![false; self.definitions.len()];
        for o in &self.occurrences {
            used[o.parent as usize] = true;
            used[o.child as usize] = true;
        }
        used.iter().filter(|&&u| u).count()
    }
}

fn s(v: Option<&Value>) -> String {
    v.map(|x| x.display_string()).unwrap_or_default()
}

fn r(v: Option<&Value>) -> Option<u32> {
    v.and_then(|x| x.as_id())
}

/// Collect all kinds matching a predicate on the type name, including the
/// synthesized names of complex instances.
fn kinds_where(table: &EntityTable, pred: impl Fn(&str) -> bool) -> Vec<TypeId> {
    table
        .type_names
        .iter()
        .enumerate()
        .filter(|(_, n)| pred(n))
        .map(|(i, _)| i as TypeId)
        .collect()
}

fn has_part(name: &str, part: &str) -> bool {
    name.split('+').any(|p| p == part)
}

/// Attribute list for a (possibly complex) instance: for complex instances,
/// the attributes of the partial record named `partial`.
fn attrs_of<'a>(
    data: &'a [u8],
    table: &EntityTable,
    idx: usize,
    partial: &[u8],
) -> Option<Vec<Value<'a>>> {
    let e = &table.entities[idx];
    let body = e.attrs(data);
    if !e.complex {
        return parse_attrs(body).ok();
    }
    let parts = parse_complex(body).ok()?;
    parts.into_iter().find(|(n, _)| *n == partial).map(|(_, a)| a)
}

/// Build the product tree from the entity table.
pub fn build(data: &[u8], table: &EntityTable) -> ProductTree {
    let mut warnings = StructureWarnings::default();

    // Products.
    let mut products = Vec::new();
    let mut product_by_entity: HashMap<u32, u32> = HashMap::new();
    for k in kinds_where(table, |n| has_part(n, "PRODUCT")) {
        for &i in table.of_kind(k) {
            let e = &table.entities[i as usize];
            if let Some(a) = attrs_of(data, table, i as usize, b"PRODUCT") {
                product_by_entity.insert(e.id, products.len() as u32);
                products.push(Product { entity: e.id, pid: s(a.first()), name: s(a.get(1)) });
            }
        }
    }

    // Formations: entity id -> product index.
    let mut formation_product: HashMap<u32, u32> = HashMap::new();
    for k in kinds_where(table, |n| {
        has_part(n, "PRODUCT_DEFINITION_FORMATION")
            || has_part(n, "PRODUCT_DEFINITION_FORMATION_WITH_SPECIFIED_SOURCE")
    }) {
        let partial: &[u8] = if table.kind_name(k).contains("WITH_SPECIFIED_SOURCE") {
            b"PRODUCT_DEFINITION_FORMATION_WITH_SPECIFIED_SOURCE"
        } else {
            b"PRODUCT_DEFINITION_FORMATION"
        };
        for &i in table.of_kind(k) {
            let e = &table.entities[i as usize];
            if let Some(a) = attrs_of(data, table, i as usize, partial) {
                if let Some(p) = r(a.get(2)).and_then(|id| product_by_entity.get(&id)) {
                    formation_product.insert(e.id, *p);
                }
            }
        }
    }

    // Definitions.
    let mut definitions = Vec::new();
    let mut def_by_entity: HashMap<u32, u32> = HashMap::new();
    for k in kinds_where(table, |n| {
        has_part(n, "PRODUCT_DEFINITION") || has_part(n, "PRODUCT_DEFINITION_WITH_ASSOCIATED_DOCUMENTS")
    }) {
        let partial: &[u8] = if table.kind_name(k).contains("WITH_ASSOCIATED_DOCUMENTS") {
            b"PRODUCT_DEFINITION_WITH_ASSOCIATED_DOCUMENTS"
        } else {
            b"PRODUCT_DEFINITION"
        };
        for &i in table.of_kind(k) {
            let e = &table.entities[i as usize];
            if let Some(a) = attrs_of(data, table, i as usize, partial) {
                let product = r(a.get(2)).and_then(|id| formation_product.get(&id)).copied();
                if product.is_none() {
                    warnings.definition_without_product += 1;
                }
                def_by_entity.insert(e.id, definitions.len() as u32);
                definitions.push(Definition {
                    entity: e.id,
                    product,
                    did: s(a.first()),
                    description: s(a.get(1)),
                    has_shape: false,
                    shape_reps: Vec::new(),
                    bodies: 0,
                });
            }
        }
    }

    // Shapes: mark definitions that own a PRODUCT_DEFINITION_SHAPE, and
    // remember which PDS belongs to which definition.
    let mut pds_def: HashMap<u32, u32> = HashMap::new();
    for k in kinds_where(table, |n| has_part(n, "PRODUCT_DEFINITION_SHAPE")) {
        for &i in table.of_kind(k) {
            let e = &table.entities[i as usize];
            if let Some(a) = attrs_of(data, table, i as usize, b"PRODUCT_DEFINITION_SHAPE") {
                if let Some(&d) = r(a.get(2)).and_then(|id| def_by_entity.get(&id)) {
                    definitions[d as usize].has_shape = true;
                    pds_def.insert(e.id, d);
                }
            }
        }
    }

    // Shape representations: SHAPE_DEFINITION_REPRESENTATION(PDS, SR).
    let mut sr_def: HashMap<u32, u32> = HashMap::new();
    for k in kinds_where(table, |n| has_part(n, "SHAPE_DEFINITION_REPRESENTATION")) {
        for &i in table.of_kind(k) {
            if let Some(a) = attrs_of(data, table, i as usize, b"SHAPE_DEFINITION_REPRESENTATION") {
                if let (Some(&d), Some(sr)) = (r(a.first()).and_then(|id| pds_def.get(&id)), r(a.get(1))) {
                    if sr_def.insert(sr, d).is_none() {
                        definitions[d as usize].shape_reps.push(sr);
                    }
                }
            }
        }
    }
    // Plain SHAPE_REPRESENTATION_RELATIONSHIP links a definition's SR to the
    // representation that actually holds its bodies (SolidWorks and others).
    // Complex instances with a transformation are occurrence placements and
    // are skipped here.
    if let Some(k) = table.kind_id("SHAPE_REPRESENTATION_RELATIONSHIP") {
        let mut extra: Vec<(u32, u32)> = Vec::new();
        for &i in table.of_kind(k) {
            let e = &table.entities[i as usize];
            if let Ok(a) = parse_attrs(e.attrs(data)) {
                if let (Some(r1), Some(r2)) = (r(a.get(2)), r(a.get(3))) {
                    match (sr_def.get(&r1), sr_def.get(&r2)) {
                        (Some(&d), None) => extra.push((d, r2)),
                        (None, Some(&d)) => extra.push((d, r1)),
                        _ => {}
                    }
                }
            }
        }
        for (d, sr) in extra {
            if sr_def.insert(sr, d).is_none() {
                definitions[d as usize].shape_reps.push(sr);
            }
        }
    }
    // Count body items in those representations.
    let body_kinds: Vec<bool> = table
        .type_names
        .iter()
        .map(|n| {
            n.split('+').any(|p| {
                matches!(
                    p,
                    "MANIFOLD_SOLID_BREP"
                        | "BREP_WITH_VOIDS"
                        | "FACETED_BREP"
                        | "SHELL_BASED_SURFACE_MODEL"
                        | "SHELL_BASED_WIREFRAME_MODEL"
                        | "GEOMETRIC_SET"
                        | "GEOMETRIC_CURVE_SET"
                        | "TESSELLATED_SOLID"
                        | "TESSELLATED_SHELL"
                )
            })
        })
        .collect();
    for d in definitions.iter_mut() {
        let mut n = 0u32;
        for &sr in &d.shape_reps {
            if let Some(e) = table.get(sr) {
                if e.complex {
                    continue;
                }
                if let Ok(a) = parse_attrs(e.attrs(data)) {
                    if let Some(items) = a.get(1).and_then(|v| v.as_list()) {
                        n += items
                            .iter()
                            .filter_map(|v| v.as_id())
                            .filter(|&id| table.get(id).map(|x| body_kinds[x.kind as usize]).unwrap_or(false))
                            .count() as u32;
                    }
                }
            }
        }
        d.bodies = n;
    }

    // Occurrences.
    let mut occurrences = Vec::new();
    for k in kinds_where(table, |n| has_part(n, "NEXT_ASSEMBLY_USAGE_OCCURRENCE")) {
        for &i in table.of_kind(k) {
            let e = &table.entities[i as usize];
            let (oid, name, relating, related, refdes) = if !e.complex {
                match parse_attrs(e.attrs(data)) {
                    Ok(a) if a.len() >= 5 => (s(a.first()), s(a.get(1)), r(a.get(3)), r(a.get(4)), s(a.get(5))),
                    _ => {
                        warnings.nauo_malformed += 1;
                        continue;
                    }
                }
            } else {
                // External mapping: inherited attributes live on the
                // PRODUCT_DEFINITION_RELATIONSHIP partial record.
                let parts = match parse_complex(e.attrs(data)) {
                    Ok(p) => p,
                    Err(_) => {
                        warnings.nauo_malformed += 1;
                        continue;
                    }
                };
                let rel = parts.iter().find(|(n, _)| *n == b"PRODUCT_DEFINITION_RELATIONSHIP");
                let own = parts.iter().find(|(n, _)| *n == b"NEXT_ASSEMBLY_USAGE_OCCURRENCE");
                match rel {
                    Some((_, a)) if a.len() >= 5 => (
                        s(a.first()),
                        s(a.get(1)),
                        r(a.get(3)),
                        r(a.get(4)),
                        own.map(|(_, o)| s(o.first())).unwrap_or_default(),
                    ),
                    _ => {
                        warnings.nauo_malformed += 1;
                        continue;
                    }
                }
            };
            let (Some(pe), Some(ce)) = (relating, related) else {
                warnings.nauo_malformed += 1;
                continue;
            };
            let (Some(&parent), Some(&child)) = (def_by_entity.get(&pe), def_by_entity.get(&ce)) else {
                warnings.nauo_unresolved += 1;
                continue;
            };
            occurrences.push(Occurrence { entity: e.id, parent, child, oid, name, refdes });
        }
    }

    // Children lists and roots.
    let nd = definitions.len();
    let mut children: Vec<Vec<u32>> = vec![Vec::new(); nd];
    let mut is_child = vec![false; nd];
    for (i, o) in occurrences.iter().enumerate() {
        children[o.parent as usize].push(i as u32);
        is_child[o.child as usize] = true;
    }

    // Build a temporary tree view to sort children by name.
    let name_of = |d: u32| -> String {
        let def = &definitions[d as usize];
        match def.product {
            Some(p) => {
                let pr = &products[p as usize];
                if !pr.name.is_empty() {
                    pr.name.clone()
                } else {
                    pr.pid.clone()
                }
            }
            None => def.did.clone(),
        }
    };
    for list in children.iter_mut() {
        list.sort_by_cached_key(|&o| (name_of(occurrences[o as usize].child), o));
    }

    // Expanded instance counts and depths, with cycle protection.
    let mut instance_count = vec![0u64; nd];
    let mut depth = vec![0u32; nd];
    let mut state = vec![0u8; nd]; // 0 = new, 1 = visiting, 2 = done
    let mut cycles = 0usize;
    fn visit(
        d: usize,
        children: &[Vec<u32>],
        occ: &[Occurrence],
        count: &mut [u64],
        depth: &mut [u32],
        state: &mut [u8],
        cycles: &mut usize,
    ) {
        if state[d] == 2 {
            return;
        }
        if state[d] == 1 {
            *cycles += 1;
            return;
        }
        state[d] = 1;
        let mut c = 1u64;
        let mut dp = 1u32;
        for &o in &children[d] {
            let ch = occ[o as usize].child as usize;
            visit(ch, children, occ, count, depth, state, cycles);
            if state[ch] == 2 {
                c += count[ch];
                dp = dp.max(depth[ch] + 1);
            }
        }
        count[d] = c;
        depth[d] = dp;
        state[d] = 2;
    }
    for d in 0..nd {
        visit(d, &children, &occurrences, &mut instance_count, &mut depth, &mut state, &mut cycles);
    }
    warnings.cycles_broken = cycles;

    let mut roots: Vec<u32> = (0..nd as u32).filter(|&d| !is_child[d as usize]).collect();
    roots.sort_by(|&a, &b| {
        instance_count[b as usize]
            .cmp(&instance_count[a as usize])
            .then_with(|| definitions[a as usize].entity.cmp(&definitions[b as usize].entity))
    });

    ProductTree { products, definitions, occurrences, children, roots, instance_count, depth, warnings }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{scan, ScanConfig};

    const AS1: &str = "ISO-10303-21;\nHEADER;\nFILE_SCHEMA(('AUTOMOTIVE_DESIGN'));\nENDSEC;\nDATA;\n\
#1=PRODUCT('as1','as1','assembly',(#2));\n#2=PRODUCT_CONTEXT('',#3,'');\n#3=APPLICATION_CONTEXT('x');\n\
#4=PRODUCT_DEFINITION_FORMATION('','',#1);\n#5=PRODUCT_DEFINITION('design','',#4,#6);\n#6=PRODUCT_DEFINITION_CONTEXT('',#3,'');\n\
#10=PRODUCT('bolt','Bolt M8','',(#2));\n#11=PRODUCT_DEFINITION_FORMATION_WITH_SPECIFIED_SOURCE('','',#10,.BOUGHT.);\n#12=PRODUCT_DEFINITION('design','',#11,#6);\n\
#20=PRODUCT('plate','Plate','',(#2));\n#21=PRODUCT_DEFINITION_FORMATION('','',#20);\n#22=PRODUCT_DEFINITION('design','',#21,#6);\n\
#30=NEXT_ASSEMBLY_USAGE_OCCURRENCE('o1','bolt 1','',#5,#12,'B1');\n\
#31=NEXT_ASSEMBLY_USAGE_OCCURRENCE('o2','bolt 2','',#5,#12,'B2');\n\
#32=(NEXT_ASSEMBLY_USAGE_OCCURRENCE('P1')PRODUCT_DEFINITION_RELATIONSHIP('o3','plate','',#5,#22)PRODUCT_DEFINITION_USAGE());\n\
#33=NEXT_ASSEMBLY_USAGE_OCCURRENCE('o4','bolt in plate','',#22,#12,$);\n\
#40=PRODUCT_DEFINITION_SHAPE('','',#5);\n#41=PRODUCT_DEFINITION_SHAPE('','',#12);\n\
#50=SHAPE_DEFINITION_REPRESENTATION(#41,#51);\n#51=SHAPE_REPRESENTATION('',(#52),#53);\n\
#52=AXIS2_PLACEMENT_3D('',#54,$,$);\n#53=GEOMETRIC_REPRESENTATION_CONTEXT(3);\n#54=CARTESIAN_POINT('',(0.,0.,0.));\n\
#55=SHAPE_REPRESENTATION_RELATIONSHIP('','',#51,#56);\n#56=ADVANCED_BREP_SHAPE_REPRESENTATION('',(#52,#57,#58),#53);\n\
#57=MANIFOLD_SOLID_BREP('',#59);\n#58=MANIFOLD_SOLID_BREP('',#59);\n#59=CLOSED_SHELL('',());\n\
ENDSEC;\nEND-ISO-10303-21;\n";

    #[test]
    fn builds_tree() {
        let data = AS1.as_bytes();
        let t = scan(data, &ScanConfig::default()).unwrap();
        let tree = build(data, &t);
        assert_eq!(tree.products.len(), 3);
        assert_eq!(tree.definitions.len(), 3);
        assert_eq!(tree.occurrences.len(), 4);
        assert_eq!(tree.roots.len(), 1);
        let root = tree.roots[0];
        assert_eq!(tree.definition_name(root), "as1");
        // as1 -> bolt, bolt, plate(-> bolt) = 1 + 1 + 1 + (1 + 1) = 5 instances
        assert_eq!(tree.instance_count[root as usize], 5);
        assert_eq!(tree.depth[root as usize], 3);
        assert_eq!(tree.children[root as usize].len(), 3);
        // sorted by child name: "Bolt M8", "Bolt M8", "Plate"
        let names: Vec<&str> = tree.children[root as usize]
            .iter()
            .map(|&o| tree.definition_name(tree.occurrences[o as usize].child))
            .collect();
        assert_eq!(names, vec!["Bolt M8", "Bolt M8", "Plate"]);
        let plate_occ = &tree.occurrences[tree.children[root as usize][2] as usize];
        assert_eq!(plate_occ.refdes, "P1");
        assert!(tree.definitions[root as usize].has_shape);
        let bolt = tree.occurrences[tree.children[root as usize][0] as usize].child as usize;
        assert_eq!(tree.definitions[bolt].shape_reps, vec![51, 56]);
        assert_eq!(tree.definitions[bolt].bodies, 2);
        assert_eq!(tree.definitions[root as usize].bodies, 0);
        assert_eq!(tree.warnings.nauo_unresolved, 0);
        assert_eq!(tree.warnings.nauo_malformed, 0);
    }
}
