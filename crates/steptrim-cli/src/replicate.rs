//! stepreplicate (StepTrim dev tool): build a large test assembly by placing K renumbered copies
//! of a STEP file's root assembly in a grid under a new root product.
//!
//! Every original entity is copied verbatim except for the `#id` references,
//! which are offset per copy. This is the same byte-preserving write path the
//! editor will use, exercised on a file we can check exactly: the output tree
//! must be one new root with K children, each identical to the source root.

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use memmap2::Mmap;
use rayon::prelude::*;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::time::Instant;
use steptrim_core::attr::{parse_attrs, Value};
use steptrim_core::scan::{scan, Entity, EntityTable, ScanConfig};

#[derive(Parser, Debug)]
#[command(name = "stepreplicate", about = "Replicate a STEP assembly K times into one large STEP file")]
struct Args {
    /// Source STEP file
    file: PathBuf,
    /// Number of copies to place
    #[arg(long, default_value_t = 3)]
    copies: usize,
    /// Output file (default: <file>-x<copies>.stp)
    #[arg(long)]
    out: Option<PathBuf>,
    /// Grid spacing between copies in file units (default: 1.25 x the extent of all points)
    #[arg(long)]
    spacing: Option<f64>,
    /// Copies per grid row
    #[arg(long, default_value_t = 4)]
    cols: usize,
}

/// Append `text` with every `#n` reference outside string literals offset by `offset`.
fn rewrite_refs(text: &[u8], offset: u64, out: &mut Vec<u8>) {
    let mut i = 0usize;
    let mut in_str = false;
    let mut num = itoa::Buffer::new();
    while i < text.len() {
        let b = text[i];
        if in_str {
            if b == b'\'' {
                in_str = false;
            }
            out.push(b);
            i += 1;
            continue;
        }
        match b {
            b'\'' => {
                in_str = true;
                out.push(b);
                i += 1;
            }
            b'#' => {
                let s = i + 1;
                let mut e = s;
                let mut v: u64 = 0;
                while e < text.len() && text[e].is_ascii_digit() {
                    v = v * 10 + (text[e] - b'0') as u64;
                    e += 1;
                }
                if e > s {
                    out.push(b'#');
                    out.extend_from_slice(num.format(v + offset).as_bytes());
                    i = e;
                } else {
                    out.push(b);
                    i += 1;
                }
            }
            _ => {
                out.push(b);
                i += 1;
            }
        }
    }
}

fn write_value(out: &mut Vec<u8>, v: &Value, offset: u64) {
    match v {
        Value::Ref(r) => {
            out.push(b'#');
            out.extend_from_slice((*r as u64 + offset).to_string().as_bytes());
        }
        Value::Str(s) => {
            out.push(b'\'');
            out.extend_from_slice(s);
            out.push(b'\'');
        }
        Value::Num(n) => out.extend_from_slice(n),
        Value::Enum(e) => {
            out.push(b'.');
            out.extend_from_slice(e);
            out.push(b'.');
        }
        Value::Null => out.push(b'$'),
        Value::Derived => out.push(b'*'),
        Value::List(l) => {
            out.push(b'(');
            for (i, x) in l.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_value(out, x, offset);
            }
            out.push(b')');
        }
        Value::Typed(name, inner) => {
            out.extend_from_slice(name);
            out.push(b'(');
            write_value(out, inner, offset);
            out.push(b')');
        }
    }
}

/// Serialize a simple entity as `#id=TYPE(attrs);` with an id offset applied.
fn write_entity(out: &mut Vec<u8>, id: u64, type_name: &str, attrs: &[Value], offset: u64) {
    out.push(b'#');
    out.extend_from_slice(id.to_string().as_bytes());
    out.push(b'=');
    out.extend_from_slice(type_name.as_bytes());
    out.push(b'(');
    for (i, v) in attrs.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        write_value(out, v, offset);
    }
    out.extend_from_slice(b");\n");
}

fn fmt_num(x: f64) -> String {
    let s = format!("{:.6}", x);
    s
}

struct RootShape {
    /// Entity index of the root definition's SHAPE_REPRESENTATION.
    sr_index: usize,
    /// Its representation context (entity id, un-offset).
    context: u32,
    /// An AXIS2_PLACEMENT_3D among its items, if any (entity id, un-offset).
    placement: Option<u32>,
}

fn find_root_shape(data: &[u8], table: &EntityTable, root_pd: u32) -> Result<RootShape> {
    // PRODUCT_DEFINITION_SHAPE entities whose definition is the root PD.
    let mut pds_ids = Vec::new();
    for k in table.kinds_containing("PRODUCT_DEFINITION_SHAPE") {
        for &i in table.of_kind(k) {
            let e = &table.entities[i as usize];
            if e.complex {
                continue;
            }
            if let Ok(a) = parse_attrs(e.attrs(data)) {
                if a.get(2).and_then(|v| v.as_id()) == Some(root_pd) {
                    pds_ids.push(e.id);
                }
            }
        }
    }
    if pds_ids.is_empty() {
        bail!("root definition #{root_pd} has no PRODUCT_DEFINITION_SHAPE");
    }
    // SHAPE_DEFINITION_REPRESENTATION(definition=PDS, used_representation=SR)
    let mut sr_id = None;
    for k in table.kinds_containing("SHAPE_DEFINITION_REPRESENTATION") {
        for &i in table.of_kind(k) {
            let e = &table.entities[i as usize];
            if e.complex {
                continue;
            }
            if let Ok(a) = parse_attrs(e.attrs(data)) {
                let d = a.first().and_then(|v| v.as_id());
                if d.map(|d| pds_ids.contains(&d)).unwrap_or(false) {
                    sr_id = a.get(1).and_then(|v| v.as_id());
                    if sr_id.is_some() {
                        break;
                    }
                }
            }
        }
        if sr_id.is_some() {
            break;
        }
    }
    let sr_id = sr_id.ok_or_else(|| anyhow!("no SHAPE_DEFINITION_REPRESENTATION for the root definition"))?;
    let sr_index = table.index_of(sr_id).ok_or_else(|| anyhow!("dangling SR reference #{sr_id}"))?;
    let sr = &table.entities[sr_index];
    let a = parse_attrs(sr.attrs(data)).context("parsing the root SHAPE_REPRESENTATION")?;
    let context = a.get(2).and_then(|v| v.as_id()).ok_or_else(|| anyhow!("root SR has no context"))?;
    let placement_kind = table.kind_id("AXIS2_PLACEMENT_3D");
    let placement = a
        .get(1)
        .and_then(|v| v.as_list())
        .and_then(|items| {
            items.iter().filter_map(|v| v.as_id()).find(|&id| {
                table.get(id).map(|e| Some(e.kind) == placement_kind).unwrap_or(false)
            })
        });
    Ok(RootShape { sr_index, context, placement })
}

/// Crude extent of all CARTESIAN_POINT coordinates (part-local, unplaced).
fn point_extent(data: &[u8], table: &EntityTable) -> Option<[f64; 3]> {
    let k = table.kind_id("CARTESIAN_POINT")?;
    let idx = table.of_kind(k);
    let (lo, hi) = idx
        .par_chunks(65536)
        .map(|chunk| {
            let mut lo = [f64::INFINITY; 3];
            let mut hi = [f64::NEG_INFINITY; 3];
            for &i in chunk {
                let e = &table.entities[i as usize];
                let a = e.attrs(data);
                // fast path: find the coordinate list after the name string
                if let Some(open) = a.iter().rposition(|&b| b == b'(') {
                    let close = a[open..].iter().position(|&b| b == b')').map(|p| open + p).unwrap_or(a.len());
                    let mut n = 0;
                    for part in a[open + 1..close].split(|&b| b == b',') {
                        if n >= 3 {
                            break;
                        }
                        if let Ok(v) = std::str::from_utf8(part).unwrap_or("x").trim().parse::<f64>() {
                            lo[n] = lo[n].min(v);
                            hi[n] = hi[n].max(v);
                        }
                        n += 1;
                    }
                }
            }
            (lo, hi)
        })
        .reduce(
            || ([f64::INFINITY; 3], [f64::NEG_INFINITY; 3]),
            |a, b| {
                let mut lo = a.0;
                let mut hi = a.1;
                for i in 0..3 {
                    lo[i] = lo[i].min(b.0[i]);
                    hi[i] = hi[i].max(b.1[i]);
                }
                (lo, hi)
            },
        );
    if lo[0].is_finite() {
        Some([hi[0] - lo[0], hi[1] - lo[1], hi[2] - lo[2]])
    } else {
        None
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let t0 = Instant::now();
    let file = File::open(&args.file).with_context(|| format!("opening {}", args.file.display()))?;
    let mmap = unsafe { Mmap::map(&file)? };
    let data: &[u8] = &mmap;
    let table = scan(data, &ScanConfig::default())?;
    let tree = steptrim_core::structure::build(data, &table);
    let root = *tree
        .roots
        .iter()
        .find(|&&r| !tree.children[r as usize].is_empty() || tree.definitions[r as usize].has_shape)
        .ok_or_else(|| anyhow!("no root definition with children or shape"))?;
    let root_def = &tree.definitions[root as usize];
    let root_pd = root_def.entity;
    let root_name = tree.definition_name(root).to_string();
    let shape = find_root_shape(data, &table, root_pd)?;
    let root_sr = &table.entities[shape.sr_index];
    if root_sr.complex {
        bail!("root SHAPE_REPRESENTATION is a complex instance; not supported");
    }
    let sr_attrs = parse_attrs(root_sr.attrs(data)).context("root SR attributes")?;
    let sr_type = table.kind_name(root_sr.kind).to_string();

    // Contexts to reuse from copy 0.
    let root_pd_entity = table.get(root_pd).ok_or_else(|| anyhow!("root PD missing"))?;
    let pd_attrs = parse_attrs(root_pd_entity.attrs(data))?;
    let pd_context = pd_attrs.get(3).and_then(|v| v.as_id()).ok_or_else(|| anyhow!("root PD has no context"))?;
    let product_idx = root_def.product.ok_or_else(|| anyhow!("root definition has no product"))?;
    let product_entity = table.get(tree.products[product_idx as usize].entity).unwrap();
    let product_attrs = parse_attrs(product_entity.attrs(data))?;
    let product_context = product_attrs
        .get(3)
        .and_then(|v| v.as_list())
        .and_then(|l| l.first())
        .and_then(|v| v.as_id())
        .ok_or_else(|| anyhow!("root PRODUCT has no context"))?;

    let extent = point_extent(data, &table).unwrap_or([1000.0, 1000.0, 1000.0]);
    let spacing = args.spacing.unwrap_or_else(|| extent.iter().cloned().fold(0.0, f64::max) * 1.25);
    let copies = args.copies.max(1);
    let stride: u64 = table.max_id as u64 + 1;
    let out_path = args.out.clone().unwrap_or_else(|| {
        let stem = args.file.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or("out".into());
        args.file.with_file_name(format!("{stem}-x{copies}.stp"))
    });
    eprintln!(
        "Source: {} entities, root '{}' (#{}), root SR #{} ({}), extent {:.1} x {:.1} x {:.1}, spacing {:.1}",
        table.len(),
        root_name,
        root_pd,
        root_sr.id,
        sr_type,
        extent[0],
        extent[1],
        extent[2],
        spacing
    );
    eprintln!("Writing {} copies (id stride {}) to {}", copies, stride, out_path.display());

    let mut w = BufWriter::with_capacity(8 << 20, File::create(&out_path)?);
    // Header and DATA; line, verbatim.
    w.write_all(&data[..table.data_start])?;
    w.write_all(b"\n")?;

    // New entity ids start after the last copy.
    let base: u64 = copies as u64 * stride + 1;
    // Layout of new ids: see below.
    let per_copy: u64 = 12;
    let copy_base = |k: u64| base + 10 + k * per_copy;
    let needs_child_placement = shape.placement.is_none();

    let entities: &[Entity] = &table.entities;
    for k in 0..copies as u64 {
        let offset = k * stride;
        let child_placement_id = match shape.placement {
            Some(p) => p as u64 + offset,
            None => copy_base(k) + 7,
        };
        let bufs: Vec<Vec<u8>> = entities
            .par_chunks(32768)
            .enumerate()
            .map(|(ci, chunk)| {
                let mut buf = Vec::with_capacity(chunk.iter().map(|e| e.len as usize + 1).sum::<usize>() + 64);
                for (j, e) in chunk.iter().enumerate() {
                    let idx = ci * 32768 + j;
                    if idx == shape.sr_index && needs_child_placement {
                        // Rebuild the root SR with the new origin placement appended to its items.
                        let mut attrs = sr_attrs.clone();
                        if let Some(Value::List(items)) = attrs.get_mut(1) {
                            items.push(Value::Ref((child_placement_id - offset) as u32));
                        }
                        write_entity(&mut buf, e.id as u64 + offset, &sr_type, &attrs, offset);
                    } else {
                        rewrite_refs(e.text(data), offset, &mut buf);
                        buf.push(b'\n');
                    }
                }
                buf
            })
            .collect();
        for b in &bufs {
            w.write_all(b)?;
        }
        eprintln!("  copy {} written ({:.1} s)", k, t0.elapsed().as_secs_f64());
    }

    // New root assembly.
    let mut s = Vec::with_capacity(4096);
    let mut placements_in_parent: Vec<Value> = vec![Value::Ref((base + 5) as u32)];
    for k in 0..copies as u64 {
        placements_in_parent.push(Value::Ref(copy_base(k) as u32));
    }
    let title = format!("{} x{}", root_name.replace('\'', "''"), copies);
    write_entity(&mut s, base, "PRODUCT", &[
        Value::Str(b"replicated"),
        Value::Str(title.as_bytes()),
        Value::Str(b"generated by stepreplicate"),
        Value::List(vec![Value::Ref(product_context)]),
    ], 0);
    write_entity(&mut s, base + 1, "PRODUCT_DEFINITION_FORMATION", &[Value::Str(b""), Value::Str(b""), Value::Ref(base as u32)], 0);
    write_entity(&mut s, base + 2, "PRODUCT_DEFINITION", &[Value::Str(b"design"), Value::Str(b""), Value::Ref((base + 1) as u32), Value::Ref(pd_context)], 0);
    write_entity(&mut s, base + 3, "PRODUCT_DEFINITION_SHAPE", &[Value::Str(b""), Value::Str(b""), Value::Ref((base + 2) as u32)], 0);
    write_entity(&mut s, base + 4, "SHAPE_REPRESENTATION", &[Value::Str(b""), Value::List(placements_in_parent), Value::Ref(shape.context)], 0);
    write_entity(&mut s, base + 5, "AXIS2_PLACEMENT_3D", &[Value::Str(b""), Value::Ref((base + 6) as u32), Value::Ref((base + 7) as u32), Value::Ref((base + 8) as u32)], 0);
    write_entity(&mut s, base + 6, "CARTESIAN_POINT", &[Value::Str(b""), Value::List(vec![Value::Num(b"0."), Value::Num(b"0."), Value::Num(b"0.")])], 0);
    write_entity(&mut s, base + 7, "DIRECTION", &[Value::Str(b""), Value::List(vec![Value::Num(b"0."), Value::Num(b"0."), Value::Num(b"1.")])], 0);
    write_entity(&mut s, base + 8, "DIRECTION", &[Value::Str(b""), Value::List(vec![Value::Num(b"1."), Value::Num(b"0."), Value::Num(b"0.")])], 0);
    write_entity(&mut s, base + 9, "SHAPE_DEFINITION_REPRESENTATION", &[Value::Ref((base + 3) as u32), Value::Ref((base + 4) as u32)], 0);

    for k in 0..copies as u64 {
        let offset = k * stride;
        let cb = copy_base(k);
        let col = (k as usize % args.cols.max(1)) as f64;
        let row = (k as usize / args.cols.max(1)) as f64;
        let x = fmt_num(col * spacing);
        let y = fmt_num(row * spacing);
        let child_sr = root_sr.id as u64 + offset;
        let child_pd = root_pd as u64 + offset;
        let child_placement = match shape.placement {
            Some(p) => p as u64 + offset,
            None => cb + 7,
        };
        let oid = format!("copy_{k}");
        // placement in the parent
        write_entity(&mut s, cb, "AXIS2_PLACEMENT_3D", &[Value::Str(b""), Value::Ref((cb + 1) as u32), Value::Ref((base + 7) as u32), Value::Ref((base + 8) as u32)], 0);
        write_entity(&mut s, cb + 1, "CARTESIAN_POINT", &[Value::Str(b""), Value::List(vec![Value::Num(x.as_bytes()), Value::Num(y.as_bytes()), Value::Num(b"0.")])], 0);
        write_entity(&mut s, cb + 2, "NEXT_ASSEMBLY_USAGE_OCCURRENCE", &[
            Value::Str(oid.as_bytes()),
            Value::Str(oid.as_bytes()),
            Value::Str(b""),
            Value::Ref((base + 2) as u32),
            Value::Ref(child_pd as u32),
            Value::Null,
        ], 0);
        write_entity(&mut s, cb + 3, "PRODUCT_DEFINITION_SHAPE", &[Value::Str(b"Placement"), Value::Str(b"Placement of an item"), Value::Ref((cb + 2) as u32)], 0);
        // complex transform relationship, written by hand
        s.extend_from_slice(
            format!(
                "#{}=(REPRESENTATION_RELATIONSHIP('','',#{},#{})REPRESENTATION_RELATIONSHIP_WITH_TRANSFORMATION(#{})SHAPE_REPRESENTATION_RELATIONSHIP());\n",
                cb + 4,
                child_sr,
                base + 4,
                cb + 5
            )
            .as_bytes(),
        );
        write_entity(&mut s, cb + 5, "ITEM_DEFINED_TRANSFORMATION", &[Value::Str(b""), Value::Str(b""), Value::Ref(child_placement as u32), Value::Ref(cb as u32)], 0);
        write_entity(&mut s, cb + 6, "CONTEXT_DEPENDENT_SHAPE_REPRESENTATION", &[Value::Ref((cb + 4) as u32), Value::Ref((cb + 3) as u32)], 0);
        if needs_child_placement {
            // origin placement added to the child's SR items
            write_entity(&mut s, cb + 7, "AXIS2_PLACEMENT_3D", &[Value::Str(b""), Value::Ref((cb + 8) as u32), Value::Ref((cb + 9) as u32), Value::Ref((cb + 10) as u32)], 0);
            write_entity(&mut s, cb + 8, "CARTESIAN_POINT", &[Value::Str(b""), Value::List(vec![Value::Num(b"0."), Value::Num(b"0."), Value::Num(b"0.")])], 0);
            write_entity(&mut s, cb + 9, "DIRECTION", &[Value::Str(b""), Value::List(vec![Value::Num(b"0."), Value::Num(b"0."), Value::Num(b"1.")])], 0);
            write_entity(&mut s, cb + 10, "DIRECTION", &[Value::Str(b""), Value::List(vec![Value::Num(b"1."), Value::Num(b"0."), Value::Num(b"0.")])], 0);
        }
    }
    let cat_id = copy_base(copies as u64);
    write_entity(&mut s, cat_id, "PRODUCT_RELATED_PRODUCT_CATEGORY", &[Value::Str(b"assembly"), Value::Null, Value::List(vec![Value::Ref(base as u32)])], 0);
    w.write_all(&s)?;
    w.write_all(b"ENDSEC;\nEND-ISO-10303-21;\n")?;
    w.flush()?;
    let size = std::fs::metadata(&out_path)?.len();
    eprintln!(
        "Done: {} ({:.1} MB) in {:.1} s; new root PRODUCT #{} with {} occurrences",
        out_path.display(),
        size as f64 / 1_048_576.0,
        t0.elapsed().as_secs_f64(),
        base,
        copies
    );
    Ok(())
}
