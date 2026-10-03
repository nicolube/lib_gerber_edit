# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- `flatten` module: `aperture_shapes` turns any aperture into polygons in mm,
  including holes and aperture macros (expressions, variables, all
  primitives, exposure off, rotation). Errors come back as `MacroError`.
- `GerberLayerData::flatten` / `flatten_with` return a `FlatLayer`: strokes,
  flashes and regions in mm in compositing order, with exact arcs (G74 and
  G75), clear polarity, step-and-repeat and aperture blocks kept once as
  instanced definitions, aperture images per D-code, per-object issues and
  resource limits. `FlatLayer::iter_expanded` walks the expanded stream.
- `ExcellonLayerData::flatten` / `flatten_with`: drill hits as flashes,
  routed moves and G85 slots as strokes (router codes respected), one
  circular aperture per tool.
- Excellon circular routes: `X..Y..A<r>` and `X..Y..I..J..` parse into the
  new `Command::Arc` (`ArcCenter::{Radius, Offset}`) and survive transform,
  rotate, scale, merge and write; `get_corners` and `flatten` use the exact
  arc.

### Changed
- `excellon_format::Command` and `ExcellonError` are `#[non_exhaustive]`;
  `Mode` is `Copy`.

### Fixed
- `get_corners` finds the real centre of single-quadrant (G74) arcs instead
  of adding the unsigned offsets.
- Excellon arc lines (`G02X..Y..A..`) no longer fail to parse.
- Rotating a Gerber layer rotates macro apertures too (the angle is added
  to each primitive's rotation) instead of leaving them unrotated with a
  warning.

## [0.6.1] - 2026-10-03

### Added
- `examples/panelize.rs`: `cargo run --example panelize -- <in> <out> [nx] [ny] [gap_mm]`.

### Changed
- README examples compile again (`Board::from_folder` returns `LoadResult`
  since 0.3) and run as doctests against `test/mobo`; the dependency line
  shows the current version.
- Coordinates are normalised on load: every Gerber operation and Excellon
  drill/slot coordinate is stored as an absolute value with both axes written.
  Incremental input (`%FSLI…`, `ICI`, `G91`) is resolved and switched to
  absolute. Transforms, merges and step-and-repeat no longer depend on earlier
  commands or on the input mode. Saved files may therefore list both axes
  where the source omitted an unchanged one.

### Fixed
- Excellon and info layers were never flushed explicitly, so a write error on
  the final buffered bytes was silently lost. Every layer write now flushes and
  reports the error.
- `Board::write_to` / `write_to_folder` now reject duplicate layer names
  (one file would silently overwrite the other) and names containing path
  separators, before anything is written.
- `Board::write_to_folder` writes to temporary files and renames them into
  place only after every layer was written, so a failed save no longer leaves
  a half-written set of files.
- Rotating a Gerber layer no longer corrupts operations that omit an unchanged
  axis (e.g. `X3000000D01*` after `X1000000Y2000000D02*`, common in Altium and
  Eagle output). The omitted axis was treated as 0 and only the axes present
  were written back. **Files rotated and saved with earlier versions may be
  corrupted; re-export them from the source.**
- Rotating an Excellon layer had the same omitted-axis bug.
- Incremental Excellon programs were translated and merged as if their deltas
  were absolute positions.
- Gerber files using incremental notation (`%FSLI…`) converted from inch to mm
  were marked absolute while the coordinates were still deltas.
- Inch Gerber files: aperture macro parameters given as `$n` variables or
  arithmetic expressions were not converted to mm, so such macro pads came
  out 25.4x too small. Length parameters are now scaled at the point of use
  (e.g. `$1x25.4`); variable definitions and rotations are left untouched.
- Inch Gerber files: the thermal macro primitive's outer diameter was not
  converted.
- `Aperture` unit conversion overwrote a rectangle/obround width with its
  converted height and left the height unconverted.
- A bare `ICI` Excellon header line (incremental on) was rejected as invalid.
- Altium tool definitions with feed/speed before the diameter
  (`T01F00S00C0.80`) failed to parse; selecting such a tool then aborted the
  whole drill file. Tool parameters (`B`, `C`, `F`, `H`, `S`, `Z`) are now
  accepted in any order and the diameter is kept.
- `M16` (router retract with clamping) was rejected; it now lifts the router
  like `M17` (written back as `M17`).
- `R<n>X..Y..` repeat codes were rejected; they are now expanded into `n`
  drill hits offset from the previous one.
