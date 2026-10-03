//! Panelises a board into an `nx` × `ny` grid with a gap between copies.
//!
//! ```text
//! cargo run --example panelize -- <gerber-folder> <output-folder> [nx] [ny] [gap_mm]
//! cargo run --example panelize -- test/mobo /tmp/panel 2 1 2.0
//! ```

use lib_gerber_edit::board::Board;
use lib_gerber_edit::error::WriteOptions;
use lib_gerber_edit::{LayerCorners, LayerMerge, LayerTransform, Pos};
use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let [_, input, output, rest @ ..] = args.as_slice() else {
        eprintln!("usage: panelize <gerber-folder> <output-folder> [nx] [ny] [gap_mm]");
        std::process::exit(2);
    };
    let nx: u32 = rest.first().map_or(Ok(2), |v| v.parse())?;
    let ny: u32 = rest.get(1).map_or(Ok(1), |v| v.parse())?;
    let gap: f64 = rest.get(2).map_or(Ok(2.0), |v| v.parse())?;

    let loaded = Board::from_folder(Path::new(input))?;
    for (file, err) in &loaded.errors {
        eprintln!("skipped {file}: {err}");
    }
    let single = loaded.board;
    let size = single.get_size();

    let mut panel = single.clone();
    for ix in 0..nx {
        for iy in 0..ny {
            if ix == 0 && iy == 0 {
                continue;
            }
            let mut copy = single.clone();
            copy.transform(&Pos {
                x: ix as f64 * (size.width + gap),
                y: iy as f64 * (size.height + gap),
            });
            panel.merge(&copy)?;
        }
    }
    panel.write_to_folder(Path::new(output), &WriteOptions::default())?;
    let panel_size = panel.get_size();
    println!(
        "wrote {nx}x{ny} panel ({:.2} x {:.2} mm) to {output}",
        panel_size.width, panel_size.height
    );
    Ok(())
}
