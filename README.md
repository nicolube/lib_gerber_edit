# lib_gerber_edit

A Rust library for manipulating RS-274X (Extended Gerber) and Excellon drill files,
built on top of [`gerber-parser`](https://crates.io/crates/gerber_parser).

All lengths in the public API are in **millimetres**.

```toml
[dependencies]
lib_gerber_edit = "0.6"
```

---

## Features

- **Load** a full PCB stackup from a folder or individual file readers
- **Translate** layers or a whole board by any offset
- **Scale** layers by independent X/Y factors (includes unit conversion)
- **Merge** two boards or layers of the same type
- **Step-and-repeat** — tile a pattern across a grid
- **Bounding-box queries** — with correct tool-width and arc accounting
- **Flatten** a layer into plain geometry (strokes, flashes, regions, exact
  arcs, aperture macros as polygons, step-and-repeat / aperture blocks as
  instances) for rendering or hit-testing
- **Safe saving** — unedited layers are written byte-for-byte; edited layers
  with content the library did not understand are refused unless allowed
- **Vector text** — render ASCII strings into a Gerber silkscreen layer with configurable size, line thickness, and horizontal/vertical alignment
- **Error messages** include the file name and line number of the failing layer

---

## Quick start

### Panelise a board (2 × 1 grid)

```rust
use lib_gerber_edit::board::Board;
use lib_gerber_edit::error::WriteOptions;
use lib_gerber_edit::{LayerCorners, LayerMerge, LayerTransform, Pos};
use std::path::Path;

# fn main() -> Result<(), Box<dyn std::error::Error>> {
let loaded = Board::from_folder(Path::new("test/mobo"))?;
for (file, err) in &loaded.errors {
    eprintln!("skipped {file}: {err}");
}
let mut board = loaded.board;
let size = board.get_size();

let mut copy = board.clone();
copy.transform(&Pos { x: size.width + 2.0, y: 0.0 }); // 2 mm gap

board.merge(&copy)?;
# let out = std::env::temp_dir().join("lib_gerber_edit-readme-panel");
board.write_to_folder(&out, &WriteOptions::default())?;
# Ok(())
# }
```

### Render text onto a silkscreen layer

```rust
use lib_gerber_edit::board::Board;
use lib_gerber_edit::error::WriteOptions;
use lib_gerber_edit::gerber_ascii::{AsciiText, HAlign, VAlign};
use lib_gerber_edit::layer::{Layer, LayerType};
use lib_gerber_edit::{LayerTransform, Pos};
use std::path::Path;

# fn main() -> Result<(), Box<dyn std::error::Error>> {
// Build a reusable format: 3 mm tall, centred on the origin.
let fmt = AsciiText::new(3.0)
    .h_align(HAlign::Center)
    .v_align(VAlign::Middle);

// Produce two layers from the same format object.
let rev_layer = fmt.build("Rev 1.0", LayerType::SilkScreenTop);

let mut board = Board::from_folder(Path::new("test/mobo"))?.board;
board.add_layer(Layer::new(LayerType::SilkScreenTop, "board.gto", rev_layer))?;
# let out = std::env::temp_dir().join("lib_gerber_edit-readme-text");
board.write_to_folder(&out, &WriteOptions::default())?;
# Ok(())
# }
```

---

## API overview

### Core traits

| Trait | Key method | Description |
|-------|-----------|-------------|
| `LayerCorners` | `get_corners() -> (Pos, Pos)` | Axis-aligned bounding box (ink boundary, tool width included) |
| `LayerCorners` | `get_size() -> Size` | Width/height derived from `get_corners` |
| `LayerTransform` | `transform(&Pos)` | Translate all coordinates |
| `LayerScale` | `scale(x, y)` | Multiply X and Y coordinates independently |
| `LayerMerge` | `merge(&Self) -> Result<(), MergeError>` | Append another layer/board; apertures, macros and tools are remapped; on error nothing changes |
| `LayerStepAndRepeat` | `step_and_repeat(nx, ny, offset)` | Grid replication |

All traits are implemented for `Board`, `GerberLayerData`, `ExcellonLayerData`, and `LayerData`.

### `Board`

```text
Board::from_folder(path)          // load all recognised layers from a directory
Board::load(vec![("name.gbr", reader), ...]) // load from in-memory readers
board.add_layer(layer)?           // merge if type exists, insert otherwise
board.get_layer(&LayerType::Top)  // look up a layer by type
board.write_to_folder(path, &opts)  // write all layers; unedited ones verbatim
```

### `GerberLayerData`

```text
GerberLayerData::empty(layer_type)          // blank layer ready for commands
GerberLayerData::from_type(ty, reader)      // parse with explicit type
GerberLayerData::from_commands(reader)      // infer type from FileAttribute
layer.write_to(&mut writer)                 // serialise to RS-274X
```

### `AsciiText`

```text
AsciiText::new(size_mm)           // character height in mm
    .ratio(0.8)                   // line thickness as fraction of size (default 1.0)
    .h_align(HAlign::Center)      // Left (default) | Center | Right
    .v_align(VAlign::Middle)      // Bottom (default) | Middle | Top
    .build("Hello", LayerType::SilkScreenTop)
```

The origin `(0, 0)` of the returned layer corresponds to the chosen alignment anchor.

### Flattening

```rust
use lib_gerber_edit::board::Board;
use lib_gerber_edit::flatten::Shape;
use lib_gerber_edit::layer::LayerData;
use std::path::Path;

# fn main() -> Result<(), Box<dyn std::error::Error>> {
let board = Board::from_folder(Path::new("test/mobo"))?.board;
for layer in board.layers() {
    let LayerData::Gerber(gerber) = &layer.data else { continue };
    let flat = gerber.flatten()?;
    let flashes = flat
        .iter_expanded()
        .filter(|o| o.object.shape == Shape::Flash)
        .count();
    println!("{}: {flashes} flashes, {} issues", layer.name, flat.issues.len());
}
# Ok(())
# }
```

`FlatLayer::iter_expanded` yields every object in drawing order with its
final transform and polarity; `FlatLayer::root` / `definitions` keep
step-and-repeat and aperture blocks instanced for renderers that reuse
meshes. All coordinates are mm; aperture images (`FlatLayer::apertures`)
are polygons with outer contours counter-clockwise and holes clockwise
(NonZero fill). Only these aperture images are approximated (chord
tolerance `FlattenOptions::tolerance_mm`, default 0.002 mm); paths and
regions keep exact arcs.

---

## Supported spec features

| Feature | Parse | Edit / write | Flatten |
|---------|-------|--------------|---------|
| Standard apertures (C, R, O, P) incl. holes | ✓ | ✓ | ✓ |
| Aperture macros (primitives 1, 4, 5, 7, 20, 21, variables, expressions) | ✓ | ✓ (rotate, unit conversion) | ✓ |
| Macro primitives 2, 6 (moiré), 22 | needs a gerber_parser release | – | 6 ready |
| Arcs G02/G03, G74 / G75 | ✓ | ✓ | ✓ exact |
| Regions G36/G37 | ✓ | ✓ | ✓ |
| Polarity LPD / LPC | ✓ | ✓ | ✓ |
| Step and repeat (SR) | ✓ | ✓ | ✓ instanced |
| Aperture blocks (AB) | ✓ | ✓ (merge remaps) | ✓ instanced |
| IPNEG | ✓ | ✓ | ✓ flag |
| LM / LR / LS | ✓ | kept | reported as unsupported |
| Deprecated image transforms (MI, OF, SF, IR, AS) | ✓ | kept | reported as unsupported |
| Excellon hits, tools, slots (G85), repeat codes, M15/M16/M17 | ✓ | ✓ | ✓ |
| Excellon circular routes (`A`, `I`/`J`) | ✓ | ✓ | ✓ exact |

Unsupported content shows up in `Layer::diagnostics()`; it is preserved when
the layer is saved unedited.

---

## Supported layer types

| Extension | `LayerType` |
|-----------|------------|
| `.gtl` | `Top` |
| `.gbl` | `Bottom` |
| `.glN` | `Inner(N)` |
| `.gts` / `.gbs` | `MaskTop` / `MaskBottom` |
| `.gto` / `.gbo` | `SilkScreenTop` / `SilkScreenBottom` |
| `.gtp` / `.gbp` | `PasteTop` / `PasteBottom` |
| `.gm1` | `Dimensions` |
| `.gm2` | `Milling` |
| `.gvc` | `VCut` |
| `.drd` / `.drl` | `Drill` (Excellon) |
| `.gbr` | `UndefinedGerber` (type read from FileAttribute) |

---

## Notes

- Tested primarily with output from **KiCad** and **Autodesk Eagle**.
- Depend on `lib_gerber_edit::gerber_types` (re-exported) rather than on
  `gerber-types` directly, so your types always match the ones this crate uses.
- Upgrading from 0.6? See [MIGRATING.md](MIGRATING.md).
- The library is functional but still evolving — contributions welcome.

---

## Sample boards

| Path | Source |
|------|--------|
| `test/mobo` | [opulo-inc/lumenpnp](https://github.com/opulo-inc/lumenpnp/) — CERN-OHL-W v2 |
