//! Arbitrary bytes through load, flatten (default resource limits), write
//! and merge. Any panic is a bug; errors and issues are fine.
#![no_main]

use lib_gerber_edit::LayerMerge;
use lib_gerber_edit::error::WriteOptions;
use lib_gerber_edit::layer::{LayerData, LayerType};
use libfuzzer_sys::fuzz_target;
use std::io::BufWriter;

fuzz_target!(|data: &[u8]| {
    // The first byte picks how the file is read: Gerber, drill, or by content.
    let Some((&kind, bytes)) = data.split_first() else {
        return;
    };
    let ty = match kind % 3 {
        0 => LayerType::Top,
        1 => LayerType::Drill,
        _ => LayerType::UndefinedGerber,
    };
    let Ok((_, layer)) = LayerData::parse_bytes(ty, bytes) else {
        return;
    };
    let _ = match &layer {
        LayerData::Gerber(g) => g.flatten().map(drop),
        LayerData::Excellon(e) => e.flatten().map(drop),
        LayerData::Info(_) => Ok(()),
    };
    let _ = layer.write_to_with(&mut BufWriter::new(Vec::new()), &WriteOptions::default());
    let mut merged = layer.clone();
    let _ = merged.merge(&layer);
});
