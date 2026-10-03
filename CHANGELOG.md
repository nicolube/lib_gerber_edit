# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed
- Coordinates are normalised on load: every Gerber operation and Excellon
  drill/slot coordinate is stored as an absolute value with both axes written.
  Incremental input (`%FSLI…`, `ICI`, `G91`) is resolved and switched to
  absolute. Transforms, merges and step-and-repeat no longer depend on earlier
  commands or on the input mode. Saved files may therefore list both axes
  where the source omitted an unchanged one.

### Fixed
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
