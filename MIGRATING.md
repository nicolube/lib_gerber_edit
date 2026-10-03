# Migrating

## 0.6 → 0.7

### `merge` returns `Result`

`LayerMerge::merge`, `merge_from` and `Board::add_layer` can fail (layers
ending inside an open region or aperture block, negative + positive image,
Gerber + Excellon data). A failed merge leaves the receiver unchanged; for a
`Board`, every layer is checked before any is changed.

```rust,ignore
// 0.6
board.merge(&copy);
board.add_layer(layer);

// 0.7
board.merge(&copy)?;          // MergeError
board.add_layer(layer)?;
board.check_merge(&copy)?;    // ask first, change nothing
```

### Building a `Layer`

`Layer` now remembers the file it was loaded from, in a private field, so it
can no longer be built with a struct literal.

```rust,ignore
// 0.6
let layer = Layer { ty, name: "top.gtl".into(), data: gerber.into() };

// 0.7
let layer = Layer::new(ty, "top.gtl", gerber);
let loaded = Layer::parse("top.gtl", LayerType::Top, reader)?; // keeps bytes
```

### Writing takes `WriteOptions`

Unedited layers are now written byte-for-byte as loaded. An edited layer
whose diagnostics show content the library did not understand is refused,
before any file is opened, unless you allow it.

```rust,ignore
// 0.6
board.write_to_folder(path)?;
board.write_to(&mut |layer| open(layer))?;

// 0.7
use lib_gerber_edit::error::WriteOptions;
board.write_to_folder(path, &WriteOptions::default())?;
board.write_to(&WriteOptions::default().allow_incomplete(true), &mut |layer| open(layer))?;
```

The error type is `WriteError` (was `GerberError` / `io::Error`); name
problems are `WriteError::InvalidName` and `WriteError::DuplicateName`, and
`WriteError::Incomplete` carries the layer's `LayerDiagnostics`.

### Non-exhaustive Excellon enums

`excellon_format::Command` gained `Command::Arc` (circular routes with `A`
or `I`/`J`) and, like `ExcellonError`, is now `#[non_exhaustive]`. Add a
wildcard arm to matches:

```rust,ignore
match command {
    Command::Coordinate(x, y, fmt) => { /* … */ }
    Command::Arc { x, y, center, fmt } => { /* … */ }
    _ => {}
}
```

### Behaviour changes worth checking

- Rotating a Gerber layer now also rotates macro apertures (previously left
  unrotated with a warning). Files rotated with 0.6 that use macros should be
  re-exported from the original.
- Merging now copies aperture macros and remaps aperture blocks; merged
  content starts with dark polarity and default modes.
- Excellon `G02`/`G03` lines with `A` or `I`/`J` used to be parse errors and
  are now arcs.
