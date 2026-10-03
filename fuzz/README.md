# Fuzzing

`load_flatten` feeds arbitrary bytes through load (Gerber, Excellon or by
content), `flatten`, write and merge. Any panic is a bug.

```text
cargo install cargo-fuzz
cd fuzz
cargo +nightly fuzz run load_flatten -- -max_total_time=300 -max_len=20000
```

Seed the corpus from real files first, e.g. each `test/mobo/*.gbr` prefixed
with one byte `0x00` (the first byte selects Gerber / drill / by content).

## Parser fork

`Cargo.toml` patches `gerber_parser` to a local checkout of the fork at
`../../gerber-parser` (branch `fix/attribute-arg-bounds`), which fixes two
panics found here (a one-value `.GenerationSoftware`, and coordinate formats
with more than 6 decimals or overflowing values). Until those fixes are
released upstream, clone the fork next to this repository or remove the
`[patch.crates-io]` section to fuzz the released parser, which panics on
those inputs within seconds.

Builds with `panic = "abort"` (wasm) abort on such panics; native callers get
`ParseError::ParserPanic` from the library instead.
