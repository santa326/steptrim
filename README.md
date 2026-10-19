# StepTrim

StepTrim trims a STEP assembly down to the parts you need. Load a large STEP (ISO 10303-21) assembly fast, preview it at low fidelity without a CAD
kernel, remove occurrences from the product tree, and save a STEP file that still opens
in CAD. Design: [docs/StepTrim_Design.md](docs/StepTrim_Design.md)
(live copy: <https://claude.ai/code/artifact/16b44a95-789d-42d4-ae2f-6b6396d58d9d>).

## Layout

| Path | What |
| --- | --- |
| `crates/steptrim-core` | Library: parallel Part 21 scanner, entity table, attribute parser, header parser, product tree |
| `crates/steptrim-cli` | `stepinfo` (scan, stats, timings, tree, JSON) and `stepreplicate` (build large test files) |
| `fixtures/steptools` | Public sample assemblies from steptools.com (downloaded by `tools/download_fixtures.sh`, not in git) |
| `fixtures/generated` | Files over 1 GB made with `stepreplicate` (not in git) |
| `fixtures/freecad` | FreeCAD tree dumps for cross-checking (`tools/freecad_tree.py`) |
| `docs/` | Design doc export and diagrams |

## Build and run

Requires Rust (stable, MSVC toolchain on Windows).

```powershell
cargo build --release
.\target\release\stepinfo.exe fixtures\steptools\as1-ec-214.stp --tree-depth 4
.\target\release\stepinfo.exe fixtures\steptools\Scania-Engine-V8-XT-Turbo.step --tree-depth 2 --json tree.json
.\target\release\stepreplicate.exe fixtures\steptools\Scania-Engine-V8-XT-Turbo.step --copies 3 --cols 2 --out fixtures\generated\scania-engine-x3.stp
cargo test
```

`stepinfo` options: `--tree-depth N`, `--expand` (one line per occurrence), `--top-types N`,
`--chunk-mb N`, `--threads N`, `--json FILE`, `--json-depth N`.

## How the scanner works

One parallel pass finds every entity's byte span, id and type name; attributes are parsed
later only for the entities a caller asks about. Workers take 4 MB byte ranges and start at
the first `\n#<digits>=` at or after their range, which Part 21 guarantees is an entity
start because strings cannot contain raw newlines. String literals, `''` escapes, `/* */`
comments, multi-line entities, several entities per line, and complex (multi-supertype)
instances are handled; a complex instance gets a synthesized type name such as
`REPRESENTATION_RELATIONSHIP+REPRESENTATION_RELATIONSHIP_WITH_TRANSFORMATION+SHAPE_REPRESENTATION_RELATIONSHIP`.

Known limitation: a multi-line `/* */` comment containing text that looks like an entity
start would confuse the chunk boundaries.

## Measured on this machine (i9-14900HX, 32 threads, file in page cache)

| File | Size | Entities | Scan | Rate |
| --- | --- | --- | --- | --- |
| Scania engine (Inventor) | 357 MB | 3.86 M | 77 ms | 4.6 GB/s |
| Scania engine, 1 thread | 357 MB | 3.86 M | 318 ms | 1.1 GB/s |
| Engine x3 (generated) | 1,068 MB | 11.6 M | 248 ms | 4.3 GB/s |

Product-tree extraction after the scan takes under 2 ms on all of these.

## Fixture cross-check

`tools/freecad_tree.py` dumps the tree FreeCAD 1.0 builds from a file. Counting FreeCAD's
`App::Part` and `Part::Feature` objects against `stepinfo`'s expanded instances:

| File | FreeCAD | stepinfo |
| --- | --- | --- |
| as1-ec-214.stp | 28 | 28 |
| rear.stp | 50 | 50 |
| UMC-500 (SolidWorks) | 456 | 351 instances, 336 bodies |

FreeCAD splits a multi-body part into an `App::Part` container with one `Part::Feature`
per body. Predicting its counts from our bodies-per-part (110 containers, 336 features)
lands within 5 of FreeCAD's 115 and 341; the residual is not yet explained. Labels cannot
be compared directly because FreeCAD suffixes duplicates with three digits.

| File | Size | Entities | Scan | Rate |
| --- | --- | --- | --- | --- |
| Engine x6 (generated) | 2,158 MB | 23.2 M | 507 ms | 4.2 GB/s |
