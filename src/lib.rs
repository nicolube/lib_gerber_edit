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

pub trait LayerCorners {

    /// Returns size of layer calculated by corners diff
    fn get_size(&self) -> Size {
        let (min, max) = self.get_corners();
        let width = max.x - min.x;
        let height = max.y - min.y;
        Size { width, height }
    }

    /// Returns min (x, y) and max (x, y) position
    fn get_corners(&self) -> (Pos, Pos);
}

pub trait LayerTransform {

    /// Adds an offset to given data
    fn transform(&mut self, transform: &Pos);
}

pub trait LayerScale {

    /// Scales to given data by x and y
    fn scale(&mut self, x: f64, y: f64);
}

pub trait LayerMerge {

    /// Appends given data, tools need to be merged and remapped
    fn merge(&mut self, other: &Self);
}

pub trait LayerStepAndRepeat {

    /// Multiplies given data by x and y with offset
    fn step_and_repeat(&mut self, x_repetitions: u32, y_repetitions: u32, offset: &Pos);
}

/// Position in mm
#[derive(Debug, Clone, PartialEq, Display)]
#[display("x: {x:.2}, y: {y:.2}")]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Pos {
    pub x: f64,
    pub y: f64,
}

/// Size in mm
#[derive(Debug, Clone, PartialEq, Display)]
#[display("width: {width:.2}, height: {height:.2}")]
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
    use std::fs;
    use std::path::Path;
    use crate::board::Board;
    use super::*;

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
            let mut board = Board::from_folder(&in_path).unwrap();

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
