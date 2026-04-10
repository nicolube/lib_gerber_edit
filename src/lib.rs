//! # lib_gerber_edit
//!
//! Manipulation library for RS-274X (Extended Gerber) and Excellon drill files,
//! built on top of [`gerber_parser`].
//!
//! All lengths exposed through the public API are in **millimetres**.
//!
//! ## Key types
//!
//! | Type | Description |
//! |------|-------------|
//! | [`Board`](board::Board) | Collection of mixed Gerber/Excellon layers |
//! | [`GerberLayerData`](gerber::GerberLayerData) | Single RS-274X layer |
//! | [`AsciiText`](gerber_ascii::AsciiText) | Renders vector text into a Gerber layer |
//! | [`LayerType`](layer::LayerType) | Identifies each layer (copper, silkscreen, drill …) |
//!
//! ## Traits
//!
//! | Trait | Description |
//! |-------|-------------|
//! | [`LayerCorners`] | Bounding box (`get_corners`) and size (`get_size`) |
//! | [`LayerTransform`] | Translate by a [`Pos`] offset |
//! | [`LayerScale`] | Scale by independent X/Y factors |
//! | [`LayerMerge`] | Merge two layers or boards of the same type |
//! | [`LayerStepAndRepeat`] | Tile a pattern across a grid |
//!
//! ## Quick start
//!
//! ```no_run
//! use lib_gerber_edit::board::Board;
//! use lib_gerber_edit::{LayerTransform, Pos};
//! use std::path::Path;
//!
//! // Load every recognised layer from a directory.
//! let result = Board::from_folder(Path::new("gerbers/")).unwrap();
//! for (name, err) in &result.errors {
//!     eprintln!("Warning: failed to load '{}': {}", name, err);
//! }
//! let mut board = result.board;
//!
//! // Shift the whole board by (10 mm, 5 mm).
//! board.transform(&Pos { x: 10.0, y: 5.0 });
//!
//! // Write back to a different directory.
//! board.write_to_folder(Path::new("output/")).unwrap();
//! ```
//!
//! ### Rendering text
//!
//! ```no_run
//! use lib_gerber_edit::gerber_ascii::{AsciiText, HAlign, VAlign};
//! use lib_gerber_edit::layer::LayerType;
//!
//! let fmt = AsciiText::new(3.0)           // 3 mm character height
//!     .h_align(HAlign::Center)
//!     .v_align(VAlign::Middle);
//!
//! // Same format object — render two different strings.
//! let rev1 = fmt.build("Rev 1.0", LayerType::SilkScreenTop);
//! let rev2 = fmt.build("Rev 2.0", LayerType::SilkScreenTop);
//! ```

pub mod board;
pub mod error;
pub mod excellon_format;
pub mod gerber;
pub mod gerber_ascii;
pub mod layer;
pub mod unit_able;

pub use gerber_parser;
pub use gerber_parser::gerber_types;

use crate::layer::{LayerData, LayerType};
use derive_more::Display;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

pub type Result<T> = std::result::Result<T, error::Error>;

/// Bounding-box queries.
///
/// All coordinates are in millimetres.
pub trait LayerCorners {
    /// Returns the axis-aligned bounding box as `(min, max)`.
    ///
    /// Tool widths (aperture sizes) are taken into account, so the returned
    /// rectangle is the true ink boundary of the layer.
    fn get_corners(&self) -> (Pos, Pos);

    /// Returns the bounding-box dimensions derived from [`get_corners`](Self::get_corners).
    fn get_size(&self) -> Size {
        let (min, max) = self.get_corners();
        let width = max.x - min.x;
        let height = max.y - min.y;
        Size { width, height }
    }
}

/// Rigid translation.
pub trait LayerTransform {
    /// Shifts all coordinates by `transform` (mm).
    fn transform(&mut self, transform: &Pos);
}

/// Uniform or anisotropic scaling.
pub trait LayerScale {
    /// Multiplies every X coordinate by `x` and every Y coordinate by `y`.
    fn scale(&mut self, x: f64, y: f64);
}

/// Layer concatenation.
pub trait LayerMerge: Sized {
    /// Appends `other` into `self`.
    ///
    /// Aperture and tool IDs are remapped to avoid collisions.
    fn merge(&mut self, other: &Self);

    /// Converts `other` into `Self` and merges it.
    ///
    /// Convenience wrapper around [`merge`](Self::merge) that accepts any type
    /// that can be converted into `Self` via [`Into`].
    fn merge_from(&mut self, other: impl Into<Self>) {
        self.merge(&other.into());
    }
}

/// Grid replication.
pub trait LayerStepAndRepeat {
    /// Tiles the layer `x_repetitions × y_repetitions` times.
    ///
    /// The first copy is the original; subsequent copies are offset by
    /// multiples of `offset` (mm).
    fn step_and_repeat(&mut self, x_repetitions: u32, y_repetitions: u32, offset: &Pos);
}

/// Position in mm
#[derive(Debug, Clone, PartialEq, Display, Default)]
#[display("x: {x:.3}, y: {y:.3}")]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Pos {
    pub x: f64,
    pub y: f64,
}

/// Size in mm
#[derive(Debug, Clone, PartialEq, Display, Default)]
#[display("width: {width:.3}, height: {height:.3}")]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Size {
    pub width: f64,
    pub height: f64,
}

/// And macro to load a single layer statically
///
/// Used for loading static assets
#[macro_export]
macro_rules! load_layer_data {
    ($file:expr $(,)?) => {{
        let data = include_str!($file);
        let reader = std::io::BufReader::new(std::io::Cursor::new(data));
        let ty = LayerType::try_from($file.to_string().rsplitn(2, ".").next().unwrap()).unwrap();
        LayerData::parse(ty, reader).unwrap()
    }};
}

/// And macro to load a board statically
///
/// Used for loading static assets
#[macro_export]
macro_rules! load_board_data {
    ($path:expr, $(($name:literal, $ty:expr)),* $(,)?) => {{
        let mut board = Board::empty();
         $(
            board.add_layer(Layer {
                ty: $ty,
                name: $name.to_string(),
                data: load_layer_data!(concat!($path, $name)).1
            });
         )*
        board
    }};
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::Board;
    use std::fs;
    use std::path::Path;

    #[test]
    fn it_works() {
        let folders = ["mobo"];
        if Path::new("output").exists() {
            fs::remove_dir_all("output").unwrap();
        }
        for folder in folders {
            let in_path = Path::new("test").join(folder);
            let out_path = Path::new("output").join(folder);
            fs::create_dir_all(&out_path).unwrap();
            println!("Processing folder: {:?}", in_path);
            let result = Board::from_folder(&in_path).unwrap();
            for (name, err) in &result.errors {
                println!("Warning: failed to load '{}': {}", name, err);
            }
            let mut board = result.board;

            let (min, max) = board.get_corners();
            println!(
                "Transformed Corners: ({}, {}) - ({}, {})",
                min.x, min.y, max.x, max.y
            );

            let size = board.get_size();

            board.transform(&Pos {
                x: 100.0,
                y: -100.0,
            });

            board.transform(&Pos {
                x: -100.0,
                y: 100.0,
            });

            let mut copy = board.clone();
            copy.transform(&Pos {
                y: size.height + 5.0,
                x: 0.0,
            });
            board.merge(&copy);

            board.write_to_folder(&out_path).unwrap();
        }
    }
}
