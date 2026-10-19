//! Parallel first-pass scan of the DATA section.
//!
//! Each worker takes a byte range, starts at the first entity that begins a
//! line (`\n#123=`), and walks entities sequentially using string-aware
//! searches for the terminating `;`. Part 21 forbids raw newlines inside
//! string literals, so a `#` at the start of a line followed by digits and
//! `=` can only be an entity start; that makes the chunk boundaries safe
//! without any shared state between workers. Multi-line comments that
//! contain such text are the one known exception and are not supported.

use memchr::{memchr, memchr3, memmem};
use rayon::prelude::*;
use std::collections::HashMap;
use std::fmt;

/// Interned entity type name. Complex (multi-supertype) instances get a
/// synthesized name made of the partial type names joined with `+`.
pub type TypeId = u16;

#[derive(Clone, Copy, Debug)]
pub struct Entity {
    /// Byte offset of the leading `#` in the file.
    pub start: u64,
    /// Length in bytes up to and including the terminating `;`.
    pub len: u32,
    pub id: u32,
    pub kind: TypeId,
    /// Offset from `start` of the `(` that opens the attribute list (or the
    /// complex record).
    pub attr_off: u16,
    pub complex: bool,
}

impl Entity {
    #[inline]
    pub fn range(&self) -> std::ops::Range<usize> {
        let s = self.start as usize;
        s..s + self.len as usize
    }
    /// The whole instance text, `#id=TYPE(...);`.
    #[inline]
    pub fn text<'a>(&self, data: &'a [u8]) -> &'a [u8] {
        &data[self.range()]
    }
    /// Bytes strictly inside the outermost parentheses.
    pub fn attrs<'a>(&self, data: &'a [u8]) -> &'a [u8] {
        let t = self.text(data);
        let open = self.attr_off as usize;
        let mut end = t.len() - 1; // the ';'
        while end > open && t[end] != b')' {
            end -= 1;
        }
        if end <= open {
            return &t[0..0];
        }
        &t[open + 1..end]
    }
}

#[derive(Clone, Debug)]
pub struct ScanConfig {
    /// Nominal bytes per work chunk. The default suits files from a few MB up
    /// to several GB; smaller chunks only add merge overhead.
    pub chunk_bytes: usize,
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self { chunk_bytes: 4 << 20 }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ScanStats {
    pub chunks: usize,
    /// Lines in the DATA section that did not parse as an entity and were skipped.
    pub skipped_lines: usize,
    /// Entity ids that appeared more than once (the last occurrence wins).
    pub duplicate_ids: usize,
}

#[derive(Debug)]
pub enum ScanError {
    NoDataSection,
    IdTooLarge,
}

impl fmt::Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScanError::NoDataSection => write!(f, "no DATA section found"),
            ScanError::IdTooLarge => write!(f, "an entity id does not fit in 32 bits"),
        }
    }
}
impl std::error::Error for ScanError {}

enum IdIndex {
    Dense(Vec<u32>),
    Sparse(HashMap<u32, u32>),
}

/// Every entity in the file, in file order, with lookups by id and by type.
pub struct EntityTable {
    pub entities: Vec<Entity>,
    pub type_names: Vec<String>,
    type_index: HashMap<String, TypeId>,
    by_id: IdIndex,
    /// Entity indices grouped by kind: `kind_order[kind_offsets[k]..kind_offsets[k+1]]`.
    kind_offsets: Vec<u32>,
    kind_order: Vec<u32>,
    pub max_id: u32,
    pub data_start: usize,
    pub data_end: usize,
    pub stats: ScanStats,
}

impl EntityTable {
    #[inline]
    pub fn len(&self) -> usize {
        self.entities.len()
    }
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }
    pub fn index_of(&self, id: u32) -> Option<usize> {
        match &self.by_id {
            IdIndex::Dense(v) => v
                .get(id as usize)
                .copied()
                .filter(|&i| i != u32::MAX)
                .map(|i| i as usize),
            IdIndex::Sparse(m) => m.get(&id).map(|&i| i as usize),
        }
    }
    #[inline]
    pub fn get(&self, id: u32) -> Option<&Entity> {
        self.index_of(id).map(|i| &self.entities[i])
    }
    pub fn kind_id(&self, name: &str) -> Option<TypeId> {
        self.type_index.get(name).copied()
    }
    #[inline]
    pub fn kind_name(&self, k: TypeId) -> &str {
        &self.type_names[k as usize]
    }
    /// All kinds whose (possibly synthesized) name contains `part`, which
    /// finds complex instances that include a given supertype.
    pub fn kinds_containing(&self, part: &str) -> Vec<TypeId> {
        self.type_names
            .iter()
            .enumerate()
            .filter(|(_, n)| n.contains(part))
            .map(|(i, _)| i as TypeId)
            .collect()
    }
    /// Indices (into `entities`) of every instance of a kind, in file order.
    pub fn of_kind(&self, k: TypeId) -> &[u32] {
        let a = self.kind_offsets[k as usize] as usize;
        let b = self.kind_offsets[k as usize + 1] as usize;
        &self.kind_order[a..b]
    }
    pub fn count_of_kind(&self, k: TypeId) -> usize {
        self.of_kind(k).len()
    }
    /// (kind, count) for every kind, most frequent first.
    pub fn histogram(&self) -> Vec<(TypeId, usize)> {
        let mut h: Vec<(TypeId, usize)> = (0..self.type_names.len())
            .map(|k| (k as TypeId, self.count_of_kind(k as TypeId)))
            .collect();
        h.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        h
    }
}

/// Locate the DATA section body: returns (start, end) byte offsets of the
/// text between `DATA;` and the final `ENDSEC;`.
pub fn find_data_section(data: &[u8]) -> Option<(usize, usize)> {
    let d = memmem::find(data, b"DATA;")?;
    let start = d + 5;
    let e = memmem::rfind(data, b"ENDSEC;")?;
    if e < start {
        return None;
    }
    Some((start, e))
}

#[inline]
pub(crate) fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\n' | b'\r' | b'\t')
}

#[inline]
pub(crate) fn skip_ws(data: &[u8], mut i: usize) -> usize {
    while i < data.len() && is_ws(data[i]) {
        i += 1;
    }
    i
}

#[inline]
pub(crate) fn is_ident(b: u8) -> bool {
    b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_'
}

/// Parse decimal digits at `i`; returns (value, index after digits).
#[inline]
fn parse_uint(data: &[u8], mut i: usize) -> Option<(u64, usize)> {
    let s = i;
    let mut v: u64 = 0;
    while i < data.len() && data[i].is_ascii_digit() {
        v = v.checked_mul(10)?.checked_add((data[i] - b'0') as u64)?;
        i += 1;
    }
    if i == s {
        None
    } else {
        Some((v, i))
    }
}

/// Is there an entity start `#<digits>=` at `p`?
#[inline]
fn entity_start_at(data: &[u8], p: usize) -> bool {
    if p >= data.len() || data[p] != b'#' {
        return false;
    }
    match parse_uint(data, p + 1) {
        Some((_, q)) => {
            let q = skip_ws(data, q);
            q < data.len() && data[q] == b'='
        }
        None => false,
    }
}

/// First position `p >= lo` (and `< hi`) such that `p` starts a line and
/// begins an entity.
fn find_line_entity_start(data: &[u8], lo: usize, hi: usize) -> Option<usize> {
    if (lo == 0 || data[lo - 1] == b'\n') && entity_start_at(data, lo) {
        return Some(lo);
    }
    let finder = memmem::Finder::new(b"\n#");
    let mut from = lo.saturating_sub(1);
    while from < hi {
        let k = finder.find(&data[from..hi])?;
        let p = from + k + 1;
        if entity_start_at(data, p) {
            return Some(p);
        }
        from = p;
    }
    None
}

/// Index just past the `;` that ends the entity whose attribute list starts
/// at or after `from`. Skips string literals and `/* */` comments.
fn find_entity_end(data: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    loop {
        let k = memchr3(b';', b'\'', b'/', &data[i..])?;
        let j = i + k;
        match data[j] {
            b';' => return Some(j + 1),
            b'\'' => {
                let mut s = j + 1;
                loop {
                    let m = memchr(b'\'', &data[s..])?;
                    let c = s + m;
                    if data.get(c + 1) == Some(&b'\'') {
                        s = c + 2; // doubled apostrophe = escaped
                    } else {
                        i = c + 1;
                        break;
                    }
                }
            }
            _ => {
                if data.get(j + 1) == Some(&b'*') {
                    let m = memmem::find(&data[j + 2..], b"*/")?;
                    i = j + 2 + m + 2;
                } else {
                    i = j + 1;
                }
            }
        }
    }
}

/// Skip a balanced parenthesised group starting at `i` (which must be `(`),
/// string-aware. Returns the index after the closing `)`.
fn skip_group(data: &[u8], mut i: usize, limit: usize) -> Option<usize> {
    let mut depth = 0i32;
    while i < limit {
        match data[i] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            b'\'' => {
                let mut s = i + 1;
                loop {
                    let m = memchr(b'\'', &data[s..limit])?;
                    let c = s + m;
                    if c + 1 < limit && data[c + 1] == b'\'' {
                        s = c + 2;
                    } else {
                        i = c;
                        break;
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Synthesize the name of a complex instance whose record spans `open..end`
/// (`open` at `(`, `end` past the `;`). Returns e.g.
/// `REPRESENTATION_RELATIONSHIP+REPRESENTATION_RELATIONSHIP_WITH_TRANSFORMATION`.
fn complex_name(data: &[u8], open: usize, end: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = open + 1;
    let limit = end;
    loop {
        i = skip_ws(data, i);
        if i >= limit || data[i] == b')' {
            break;
        }
        let s = i;
        while i < limit && is_ident(data[i]) {
            i += 1;
        }
        if i == s {
            break;
        }
        if !out.is_empty() {
            out.push(b'+');
        }
        out.extend_from_slice(&data[s..i]);
        i = skip_ws(data, i);
        if i < limit && data[i] == b'(' {
            match skip_group(data, i, limit) {
                Some(n) => i = n,
                None => break,
            }
        }
    }
    if out.is_empty() {
        out.extend_from_slice(b"<COMPLEX>");
    }
    out
}

struct ChunkOut {
    entities: Vec<Entity>,
    names: Vec<Vec<u8>>,
    skipped: usize,
}

fn intern(map: &mut HashMap<Vec<u8>, TypeId>, names: &mut Vec<Vec<u8>>, name: &[u8]) -> TypeId {
    if let Some(&k) = map.get(name) {
        return k;
    }
    let k = names.len() as TypeId;
    names.push(name.to_vec());
    map.insert(name.to_vec(), k);
    k
}

/// Advance `p` to the start of the next line (used to skip unparsable text).
#[inline]
fn next_line(data: &[u8], p: usize, data_end: usize) -> Option<usize> {
    memchr(b'\n', &data[p..data_end]).map(|n| p + n + 1)
}

fn scan_range(data: &[u8], lo: usize, hi: usize, data_start: usize, data_end: usize) -> ChunkOut {
    let mut names: Vec<Vec<u8>> = Vec::new();
    let mut map: HashMap<Vec<u8>, TypeId> = HashMap::new();
    let mut out: Vec<Entity> = Vec::with_capacity((hi - lo) / 48 + 16);
    let mut skipped = 0usize;

    let mut p = if lo == data_start {
        skip_ws(data, lo)
    } else {
        match find_line_entity_start(data, lo, data_end) {
            Some(p) => p,
            None => return ChunkOut { entities: out, names, skipped },
        }
    };

    loop {
        p = skip_ws(data, p);
        if p >= data_end {
            break;
        }
        if p >= hi && data[p - 1] == b'\n' {
            break; // the next chunk owns this entity
        }
        if data[p] != b'#' {
            if data[p..].starts_with(b"ENDSEC") {
                break;
            }
            if data[p..].starts_with(b"/*") {
                // top-level comment
                match memmem::find(&data[p + 2..data_end], b"*/") {
                    Some(m) => {
                        p = p + 2 + m + 2;
                        continue;
                    }
                    None => break,
                }
            }
            skipped += 1;
            match next_line(data, p, data_end) {
                Some(n) => {
                    p = n;
                    continue;
                }
                None => break,
            }
        }
        let (id, q) = match parse_uint(data, p + 1) {
            Some(v) => v,
            None => {
                skipped += 1;
                match next_line(data, p, data_end) {
                    Some(n) => {
                        p = n;
                        continue;
                    }
                    None => break,
                }
            }
        };
        let q = skip_ws(data, q);
        if q >= data_end || data[q] != b'=' {
            skipped += 1;
            match next_line(data, p, data_end) {
                Some(n) => {
                    p = n;
                    continue;
                }
                None => break,
            }
        }
        let q = skip_ws(data, q + 1);
        if q >= data_end {
            break;
        }
        let (kind, open, complex) = if data[q] == b'(' {
            (None, q, true)
        } else {
            let s = q;
            let mut e = q;
            while e < data_end && is_ident(data[e]) {
                e += 1;
            }
            let a = skip_ws(data, e);
            if e == s || a >= data_end || data[a] != b'(' {
                skipped += 1;
                match next_line(data, p, data_end) {
                    Some(n) => {
                        p = n;
                        continue;
                    }
                    None => break,
                }
            }
            (Some(&data[s..e]), a, false)
        };
        let end = match find_entity_end(data, open) {
            Some(e) if e <= data_end + 1 => e,
            _ => break,
        };
        let k = match kind {
            Some(name) => intern(&mut map, &mut names, name),
            None => {
                let n = complex_name(data, open, end);
                intern(&mut map, &mut names, &n)
            }
        };
        let id32 = if id > u32::MAX as u64 { u32::MAX } else { id as u32 };
        out.push(Entity {
            start: p as u64,
            len: (end - p) as u32,
            id: id32,
            kind: k,
            attr_off: (open - p).min(u16::MAX as usize) as u16,
            complex,
        });
        p = end;
    }
    ChunkOut { entities: out, names, skipped }
}

/// Scan the whole file in parallel and build the entity table.
pub fn scan(data: &[u8], cfg: &ScanConfig) -> Result<EntityTable, ScanError> {
    let (data_start, data_end) = find_data_section(data).ok_or(ScanError::NoDataSection)?;
    let len = data_end - data_start;
    let chunk = cfg.chunk_bytes.max(64 << 10);
    let n = ((len + chunk - 1) / chunk).max(1);

    let mut chunks: Vec<ChunkOut> = (0..n)
        .into_par_iter()
        .map(|i| {
            let lo = data_start + i * chunk;
            let hi = (lo + chunk).min(data_end);
            scan_range(data, lo, hi, data_start, data_end)
        })
        .collect();

    // Global type interning and remap of per-chunk kinds.
    let mut type_index: HashMap<String, TypeId> = HashMap::new();
    let mut type_names: Vec<String> = Vec::new();
    let remaps: Vec<Vec<TypeId>> = chunks
        .iter()
        .map(|c| {
            c.names
                .iter()
                .map(|n| {
                    let s = String::from_utf8_lossy(n);
                    if let Some(&k) = type_index.get(s.as_ref()) {
                        k
                    } else {
                        let k = type_names.len() as TypeId;
                        type_names.push(s.to_string());
                        type_index.insert(s.to_string(), k);
                        k
                    }
                })
                .collect()
        })
        .collect();
    chunks.par_iter_mut().zip(remaps.par_iter()).for_each(|(c, r)| {
        for e in &mut c.entities {
            e.kind = r[e.kind as usize];
        }
    });

    let total: usize = chunks.iter().map(|c| c.entities.len()).sum();
    let skipped: usize = chunks.iter().map(|c| c.skipped).sum();
    let mut entities: Vec<Entity> = Vec::with_capacity(total);
    for c in chunks {
        entities.extend_from_slice(&c.entities);
    }

    let mut max_id = 0u32;
    for e in &entities {
        if e.id == u32::MAX {
            return Err(ScanError::IdTooLarge);
        }
        max_id = max_id.max(e.id);
    }

    // Id index: dense when ids are reasonably packed, sparse otherwise.
    let mut duplicates = 0usize;
    let by_id = if (max_id as usize) <= entities.len().saturating_mul(8) + (1 << 20) {
        let mut v = vec![u32::MAX; max_id as usize + 1];
        for (i, e) in entities.iter().enumerate() {
            let slot = &mut v[e.id as usize];
            if *slot != u32::MAX {
                duplicates += 1;
            }
            *slot = i as u32;
        }
        IdIndex::Dense(v)
    } else {
        let mut m: HashMap<u32, u32> = HashMap::with_capacity(entities.len());
        for (i, e) in entities.iter().enumerate() {
            if m.insert(e.id, i as u32).is_some() {
                duplicates += 1;
            }
        }
        IdIndex::Sparse(m)
    };

    // Per-kind index by counting sort (stable, keeps file order).
    let nk = type_names.len();
    let mut counts = vec![0u32; nk + 1];
    for e in &entities {
        counts[e.kind as usize + 1] += 1;
    }
    for k in 0..nk {
        counts[k + 1] += counts[k];
    }
    let kind_offsets = counts.clone();
    let mut fill = counts;
    let mut kind_order = vec![0u32; entities.len()];
    for (i, e) in entities.iter().enumerate() {
        let slot = &mut fill[e.kind as usize];
        kind_order[*slot as usize] = i as u32;
        *slot += 1;
    }

    Ok(EntityTable {
        entities,
        type_names,
        type_index,
        by_id,
        kind_offsets,
        kind_order,
        max_id,
        data_start,
        data_end,
        stats: ScanStats { chunks: n, skipped_lines: skipped, duplicate_ids: duplicates },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) const SAMPLE: &str = "ISO-10303-21;\nHEADER;\nFILE_DESCRIPTION(('x'),'2;1');\nFILE_NAME('a','b',(''),(''),'c','d','');\nFILE_SCHEMA(('AUTOMOTIVE_DESIGN'));\nENDSEC;\nDATA;\n#1=PRODUCT('p1','Part one','it''s; tricky',(#2));\n#2=PRODUCT_CONTEXT('',#3,'mechanical');\n#3 = APPLICATION_CONTEXT('core data');\n#4=(REPRESENTATION_RELATIONSHIP('','',#5,#6)REPRESENTATION_RELATIONSHIP_WITH_TRANSFORMATION(#7)SHAPE_REPRESENTATION_RELATIONSHIP());\n#5=SHAPE_REPRESENTATION('',(#8,\n#9),#10); #6=SHAPE_REPRESENTATION('',(#9),#10);\n/* a comment ; with # noise */\n#7=ITEM_DEFINED_TRANSFORMATION('','',#8,#9);\n#8=AXIS2_PLACEMENT_3D('',#11,$,$);\n#9=AXIS2_PLACEMENT_3D('',#11,$,$);\n#10=GEOMETRIC_REPRESENTATION_CONTEXT(3);\n#11=CARTESIAN_POINT('',(0.,0.,0.));\nENDSEC;\nEND-ISO-10303-21;\n";

    #[test]
    fn scans_sample_with_tiny_chunks() {
        let data = SAMPLE.as_bytes();
        for chunk in [64usize << 10, 1 << 20] {
            let t = scan(data, &ScanConfig { chunk_bytes: chunk }).unwrap();
            assert_eq!(t.len(), 11, "chunk {chunk}");
            let p = t.get(1).unwrap();
            assert_eq!(t.kind_name(p.kind), "PRODUCT");
            assert_eq!(p.attrs(data), b"'p1','Part one','it''s; tricky',(#2)");
            let c = t.get(4).unwrap();
            assert!(c.complex);
            assert_eq!(
                t.kind_name(c.kind),
                "REPRESENTATION_RELATIONSHIP+REPRESENTATION_RELATIONSHIP_WITH_TRANSFORMATION+SHAPE_REPRESENTATION_RELATIONSHIP"
            );
            assert_eq!(t.get(6).map(|e| t.kind_name(e.kind)), Some("SHAPE_REPRESENTATION"));
            assert_eq!(t.get(5).unwrap().attrs(data), b"'',(#8,\n#9),#10");
            assert_eq!(t.count_of_kind(t.kind_id("AXIS2_PLACEMENT_3D").unwrap()), 2);
            assert_eq!(t.stats.duplicate_ids, 0);
            assert_eq!(t.stats.skipped_lines, 0);
        }
    }

    #[test]
    fn chunk_boundaries_are_consistent() {
        // Build a larger file and check that any chunking yields the same entity set.
        let mut s = String::from("ISO-10303-21;\nHEADER;\nFILE_SCHEMA(('X'));\nENDSEC;\nDATA;\n");
        for i in 1..=5000u32 {
            if i % 7 == 0 {
                s.push_str(&format!(
                    "#{i}=CARTESIAN_POINT('long name with ; and # inside',(1.5,\n2.5,3.5));\n"
                ));
            } else if i % 11 == 0 {
                s.push_str(&format!("#{i}=A(#1); #{}=B(#2);\n", i + 100000));
            } else {
                s.push_str(&format!("#{i}=DIRECTION('',(0.,0.,1.));\n"));
            }
        }
        s.push_str("ENDSEC;\nEND-ISO-10303-21;\n");
        let data = s.as_bytes();
        let base = scan(data, &ScanConfig { chunk_bytes: 1 << 30 }).unwrap();
        let ids: Vec<u32> = base.entities.iter().map(|e| e.id).collect();
        // multiples of 77 take the first branch, so they add no second entity
        assert_eq!(ids.len(), 5000 + 5000 / 11 - 5000 / 77);
        for chunk in [64 << 10, 70_000, 100_003] {
            let t = scan(data, &ScanConfig { chunk_bytes: chunk }).unwrap();
            let got: Vec<u32> = t.entities.iter().map(|e| e.id).collect();
            assert_eq!(got, ids, "chunk {chunk}");
            assert_eq!(t.stats.skipped_lines, 0);
        }
    }
}
