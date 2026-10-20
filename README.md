# StepTrim

StepTrim trims a STEP assembly down to the parts you need. Load a large STEP (ISO 10303-21) assembly fast, preview it at low fidelity without a CAD
kernel, remove occurrences from the product tree, and save a STEP file that still opens
in CAD. Design: [docs/StepTrim_Design.md](docs/StepTrim_Design.md)
(live copy: <https://claude.ai/code/artifact/16b44a95-789d-42d4-ae2f-6b6396d58d9d>).
Repository: <https://github.com/santa326/steptrim> (private).

## Layout

| Path | What |
| --- | --- |
| `crates/steptrim-core` | Library: parallel Part 21 scanner, entity table, attribute parser, header parser, product tree |
| `crates/steptrim-cli` | `stepinfo` (scan, stats, tree, JSON, `--bounds`, `--check-refs`), `stepprune` (remove occurrences from the command line, with output check) and `stepreplicate` (build large test files) |
| `crates/steptrim-app` | `steptrim`, the desktop app: egui + wgpu, progressive load, tree, box and body preview (edges + surfels), pick, filter, select by size, remove/undo, save trimmed |
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

Desktop app:

```powershell
.	arget
.\target\release\steptrim.exe fixtures\steptools\Scania-8x4.stp
```

Open a file from the toolbar, the command line, or by dropping it on the window. The product
tree appears as soon as the scan finishes; part boxes stream in behind it.

Selection is shared between the tree and the viewport. Click a row or a box to select it;
Ctrl-click toggles, Shift-click selects the range from the last click (in the tree's row
order), Ctrl+A selects everything visible. Selecting an assembly highlights its whole subtree
in a lighter orange. Selecting a box in the viewport expands and scrolls the tree to it. The
box under the pointer, or the part under the pointer in the tree, is tinted yellow in both
views, and the viewport names the hovered box next to the cursor.

Bodies are drawn as splats: each face is sampled on a grid in its surface's own parameter
space, trimmed by an even-odd test against the face loops projected into that space, and each
sample becomes an oriented disc with a Gaussian footprint, drawn opaque with a depth test so
nothing has to be sorted. Edges are sampled exactly from the file (lines, arcs, splines) and
drawn as lines. Boxes stand in for parts whose bodies are not sampled yet and can be switched
back on; the three layers are toggles in the toolbar. See the design doc's Gaussian splatting
section for why this primitive and not triangles or true 3DGS.

The toolbar's "bodies:" choice switches between **splats** and **mesh**. Mesh mode draws
triangles from the same sampling pass: planar faces are triangulated exactly with their holes
(earcut), curved analytic faces become quads on their parameter grid where all four corners
lie inside the loops, and B-spline faces are untrimmed grids. Edges can be drawn over either.

"Splats…" opens a panel with the disc size multiplier (live, in the shader), the sampling
density (samples across a part, default 56, and the surfel cap per part, applied by Resample,
which re-runs the sampler in the background while boxes stand in), and the GPU budget. The
budget caps surfels plus mesh vertices; parts that would overflow it stay as boxes and a
notice says so. The app requests the adapter's real buffer limit (eframe's default caps a
buffer at 256 MiB, which a dense resample exceeds) and logs GPU errors instead of aborting.

![Mesh mode: the truck as 3.3 million shaded triangles](docs/screenshots/steptrim-mesh-truck.png)

"Select by size…" opens a panel that selects visible parts by box diagonal, with a range, quick
presets (parts smaller than 1, 2, 5 or 10 percent of the largest), a live match count, and
Select, Add, and Remove buttons: the fast way to strip fasteners and small hardware.

Visibility is separate from removal: H hides the selection, Shift+H isolates it (hides
everything else), U shows all, and the context menu has Hide, Isolate, Show and Show all.
Hidden parts are dimmed and marked in the tree and are still saved. Ctrl+I inverts the
selection among visible parts; Ctrl+A selects all visible parts.

Export from the selection: "Export selection…" writes one STEP file containing only the
selected parts with the assembly structure down to them kept; "Export parts…" writes one
standalone STEP file per selected part into a folder, each part becoming the root of its
file. Both go through the same pruner as Save, so the geometry text is copied byte for byte
and the output is checked for dangling references.

Left-drag orbits, right-drag pans, the wheel zooms, double-click fits the view to a box, F
fits to the selection. Right-click a row to remove the selection (one occurrence, or every
occurrence of that part), restore it, or fit the view; Delete removes the selection and
Ctrl+Z undoes. Removed subtrees disappear from the viewport. "Save trimmed…" writes a STEP
file without the removed occurrences; every surviving entity is copied byte for byte. The
filter box lists matching instances by name.

![Cross-highlighting: the selected cab is orange in both views, the hovered part yellow](docs/screenshots/steptrim-cross-highlight.png)

![The truck drawn as bodies: surfels and edges, 2.8 million samples](docs/screenshots/steptrim-bodies-truck.png)

For tests and screenshots the app can be driven without clicks:
`steptrim file.stp --remove "Cylinder Head" --save out.stp` removes every occurrence whose
part name contains the text, then saves; `--select NAME` selects matching instances and
`--fit` fits the view to them; `--samples N` resamples at that density after loading and
`--mode mesh` starts in mesh mode; `--hide NAME`, `--export-dir DIR` and `--export-one FILE`
script the visibility and export features.

![StepTrim after removing the cylinder heads, with the save notice](docs/screenshots/steptrim-engine-trimmed.png)

### How removal works

Removing an occurrence deletes its NEXT_ASSEMBLY_USAGE_OCCURRENCE and lets the consequences
propagate: entities that reference a deleted one through a single attribute are deleted
(the usage's shape, its placement relationship, properties on it); entities that reference
it only inside a list are rewritten without that element (layers, categories, style
containers). Entities that have lost all their owners are garbage-collected, where style,
layer, category, property, document and approval entities count as decoration, not owners.
A part that has lost its last occurrence is dropped together with its geometry, so the file
does not grow an orphan top-level product. Validation properties of parents that lost a child
are removed since their child counts no longer hold. The original header, entity numbers and
geometry text are untouched.

```powershell
.	arget
.\target\release\stepprune.exe fixtures\steptools\Scania-8x4.stp --remove-name "Roda" --out trimmed.stp
```

`stepprune` re-scans its output and checks that every reference resolves.

![StepTrim showing the Scania 8x4 truck as part boxes](docs/screenshots/steptrim-scania-8x4.png)

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

Body sampling (`stepinfo --geometry`), in parallel over parts at about 56 samples across each:

| File | Faces | Edge segments | Surfels | Time |
| --- | --- | --- | --- | --- |
| Scania truck, 281 MB | 26,736 | 469k | 2.77 M, plus 3.28 M triangles | 234 ms |
| Scania engine, 357 MB | 27,724 | 617k | 3.46 M | 740 ms |
| Nissan GT-R, 167 MB | 25,966 | 699k | 1.98 M | 232 ms |

Product-tree extraction after the scan takes under 3 ms on all of these. Pruning the truck
(air tank and twelve wheels, 85 occurrences) plans in 73 ms and writes 279 MB in 125 ms;
removing both cylinder heads from the engine deletes 132k entities and writes 348 MB in 0.3 s. Occurrence transforms
resolve for every occurrence in all twelve fixtures. The per-part bounding-box walk (vertex
points, spline control points, arc extents; analytic base points skipped) takes 250 ms for the
357 MB engine and 180 ms for the 281 MB truck, walking entity text at 1.4 to 1.8 GB/s.

## Fixture cross-check

`tools/freecad_tree.py` dumps the tree FreeCAD 1.0 builds from a file. Counting FreeCAD's
`App::Part` and `Part::Feature` objects against `stepinfo`'s expanded instances:

| File | FreeCAD | stepinfo |
| --- | --- | --- |
| as1-ec-214.stp | 28 | 28 |
| rear.stp | 50 | 50 |
| UMC-500 (SolidWorks) | 456 | 351 instances, 336 bodies |
| as1 trimmed (rod assembly removed) | 24 | 24 |
| Scania 8x4 trimmed (air tank, wheels removed) | 1462, from 1569 | 531, from 638 |

FreeCAD splits a multi-body part into an `App::Part` container with one `Part::Feature`
per body. Predicting its counts from our bodies-per-part (110 containers, 336 features)
lands within 5 of FreeCAD's 115 and 341; the residual is not yet explained. Labels cannot
be compared directly because FreeCAD suffixes duplicates with three digits.

| File | Size | Entities | Scan | Rate |
| --- | --- | --- | --- | --- |
| Engine x6 (generated) | 2,158 MB | 23.2 M | 507 ms | 4.2 GB/s |
