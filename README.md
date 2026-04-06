# lib_gerber_edit

A Rust library for manipulating RS-274X (Extended Gerber) and Excellon drill files,
built on top of [`gerber-parser`](https://crates.io/crates/gerber_parser).

All lengths in the public API are in **millimetres**.

```toml
[dependencies]
lib_gerber_edit = "0.2"
```

---

## Features

- **Load** a full PCB stackup from a folder or individual file readers
- **Translate** layers or a whole board by any offset
- **Scale** layers by independent X/Y factors (includes unit conversion)
- **Merge** two boards or layers of the same type
- **Step-and-repeat** — tile a pattern across a grid
- **Bounding-box queries** — with correct tool-width and arc accounting
- **Vector text** — render ASCII strings into a Gerber silkscreen layer with configurable size, line thickness, and horizontal/vertical alignment
- **Error messages** include the file name and line number of the failing layer

---

## Quick start

### Panelise a board (2 × 1 grid)

```rust
use lib_gerber_edit::board::Board;
use lib_gerber_edit::{LayerCorners, LayerMerge, LayerTransform, Pos};
use std::path::Path;

let mut board = Board::from_folder(Path::new("gerbers/")).unwrap();
let size = board.get_size();

let mut copy = board.clone();
copy.transform(&Pos { x: size.width + 2.0, y: 0.0 }); // 2 mm gap

board.merge(&copy);
board.write_to_folder(Path::new("output/")).unwrap();
```

### Render text onto a silkscreen layer

```rust
use lib_gerber_edit::board::Board;
use lib_gerber_edit::gerber_ascii::{AsciiText, HAlign, VAlign};
use lib_gerber_edit::layer::{Layer, LayerType};
use lib_gerber_edit::{LayerTransform, Pos};
use std::path::Path;

// Build a reusable format: 3 mm tall, centred on the origin.
let fmt = AsciiText::new(3.0)
    .h_align(HAlign::Center)
    .v_align(VAlign::Middle);

// Produce two layers from the same format object.
let rev_layer = fmt.build("Rev 1.0", LayerType::SilkScreenTop);

let mut board = Board::from_folder(Path::new("gerbers/")).unwrap();
board.add_layer(Layer {
    ty: LayerType::SilkScreenTop,
    name: "board.gto".to_string(),
    data: rev_layer.into(),
});
board.write_to_folder(Path::new("output/")).unwrap();
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
| `LayerMerge` | `merge(&Self)` | Append another layer/board; aperture IDs are remapped |
| `LayerStepAndRepeat` | `step_and_repeat(nx, ny, offset)` | Grid replication |

All traits are implemented for `Board`, `GerberLayerData`, `ExcellonLayerData`, and `LayerData`.

### `Board`

```rust
Board::from_folder(path)          // load all recognised layers from a directory
Board::new(vec![("name.gbr", reader), ...])  // load from in-memory readers
board.add_layer(layer)            // merge if type exists, insert otherwise
board.get_layer(&LayerType::Top)  // look up a layer by type
board.write_to_folder(path)       // write all layers back to disk
```

### `GerberLayerData`

```rust
GerberLayerData::empty(layer_type)          // blank layer ready for commands
GerberLayerData::from_type(ty, reader)      // parse with explicit type
GerberLayerData::from_commands(reader)      // infer type from FileAttribute
layer.write_to(&mut writer)                 // serialise to RS-274X
```

### `AsciiText`

```rust
AsciiText::new(size_mm)           // character height in mm
    .ratio(0.8)                   // line thickness as fraction of size (default 1.0)
    .h_align(HAlign::Center)      // Left (default) | Center | Right
    .v_align(VAlign::Middle)      // Bottom (default) | Middle | Top
    .build("Hello", LayerType::SilkScreenTop)
```

The origin `(0, 0)` of the returned layer corresponds to the chosen alignment anchor.

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
- Arc bounding boxes follow RS-274X §5.3 (multi-quadrant G75). Single-quadrant G74 arcs may have imprecise bounding boxes.
- The library is functional but still evolving — contributions welcome.

---

## Sample boards

| Path | Source |
|------|--------|
| `test/mobo` | [opulo-inc/lumenpnp](https://github.com/opulo-inc/lumenpnp/) — CERN-OHL-W v2 |
