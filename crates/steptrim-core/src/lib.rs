//! steptrim-core: StepTrim's kernel-free reader for ISO 10303-21 (STEP Part 21) files.
//!
//! The design is a two-level scan: [`scan::scan`] finds every entity's byte
//! span, id and type name in one parallel pass; attributes are parsed later,
//! only for the entities a caller asks about ([`attr`]). [`structure`] builds
//! the product tree (products, definitions, assembly occurrences) from that
//! table without touching geometry.

pub mod attr;
pub mod geom;
pub mod header;
pub mod prune;
pub mod sample;
pub mod scan;
pub mod structure;

pub use scan::{Entity, EntityTable, ScanConfig, TypeId};
pub use structure::ProductTree;
