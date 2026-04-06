//! Vector text rendering for Gerber layers.
//!
//! Converts ASCII strings into RS-274X draw commands using the bundled
//! KiCad stroke font (`gerber/kicad_font.gbr`).
//!
//! The entry point is [`AsciiText`]: create it once with the desired size and
//! alignment, then call [`AsciiText::build`] as many times as needed to produce
//! independent [`GerberLayerData`] layers.

use crate::gerber::GerberLayerData;
use crate::{LayerCorners, LayerScale, LayerTransform, LayerType, Pos};
use gerber_parser::gerber_types::{
    Aperture, Circle, Command, DCode, FunctionCode, GCode, InterpolationMode, Operation, Unit,
};
use lazy_static::lazy_static;
use std::io::BufReader;

const CHAR_SIZE: u8 = 94;
const TEXT_HEIGHT: f64 = 9.6577;

/// Horizontal text alignment relative to the origin (0, 0)
#[derive(Debug, Clone, PartialEq, Default)]
pub enum HAlign {
    /// Origin is at the left edge of the text block
    #[default]
    Left,
    /// Origin is at the horizontal center of the text block
    Center,
    /// Origin is at the right edge of the text block
    Right,
}

/// Vertical text alignment relative to the origin (0, 0)
#[derive(Debug, Clone, PartialEq, Default)]
pub enum VAlign {
    /// Origin is at the bottom edge of the text block
    #[default]
    Bottom,
    /// Origin is at the vertical center of the text block
    Middle,
    /// Origin is at the top edge of the text block
    Top,
}

/// Reusable format configuration for rendering ASCII text as a Gerber layer
///
/// # Example
/// ```ignore
/// let fmt = AsciiText::new(3.0).h_align(HAlign::Center);
/// let layer_a = fmt.build("Rev 1.0", LayerType::SilkScreenTop);
/// let layer_b = fmt.build("Rev 2.0", LayerType::SilkScreenBottom);
/// ```
#[derive(Clone)]
pub struct AsciiText {
    size: f64,
    ratio: f64,
    h_align: HAlign,
    v_align: VAlign,
}

impl AsciiText {
    /// Creates a new `AsciiText` format.
    ///
    /// - `size`: character height in mm
    pub fn new(size: f64) -> Self {
        Self {
            size,
            ratio: 1.0,
            h_align: HAlign::default(),
            v_align: VAlign::default(),
        }
    }

    /// Line thickness as a ratio of the text size (default: `1.0`)
    pub fn ratio(mut self, ratio: f64) -> Self {
        self.ratio = ratio;
        self
    }

    /// Horizontal alignment of the text relative to the origin (default: `Left`)
    pub fn h_align(mut self, align: HAlign) -> Self {
        self.h_align = align;
        self
    }

    /// Vertical alignment of the text relative to the origin (default: `Bottom`)
    pub fn v_align(mut self, align: VAlign) -> Self {
        self.v_align = align;
        self
    }

    /// Renders `text` into a Gerber layer of the given type.
    ///
    /// The resulting layer is positioned so that the origin (0, 0) corresponds
    /// to the alignment anchor specified by [`h_align`](Self::h_align) and
    /// [`v_align`](Self::v_align).
    pub fn build(&self, text: impl AsRef<str>, layer_type: LayerType) -> GerberLayerData {
        let Self {
            size,
            ratio,
            h_align,
            v_align,
        } = self;
        let (size, ratio) = (*size, *ratio);
        let text = text.as_ref();
        let height: f64 = TEXT_HEIGHT / 6.0 * size;

        let mut layer = GerberLayerData::empty(layer_type);
        layer.apertures.insert(
            10,
            Aperture::Circle(Circle {
                diameter: size / 10.0 * ratio,
                hole_diameter: None,
            }),
        );
        layer
            .commands
            .push(Command::FunctionCode(FunctionCode::DCode(
                DCode::SelectAperture(10),
            )));
        layer
            .commands
            .push(Command::FunctionCode(FunctionCode::GCode(
                GCode::InterpolationMode(InterpolationMode::Linear),
            )));

        let lines = text.lines().count();
        let mut pos = Pos {
            x: 0.0,
            y: lines.saturating_sub(1) as f64 * height,
        };
        for c in text.chars() {
            match c {
                ' ' => pos.x += 0.6 * size,
                '\n' => {
                    pos.y -= height;
                    pos.x = 0.0;
                }
                _ if ('!'..='~').contains(&c) => {
                    let i = c as usize - '!' as usize;
                    let mut char = CHARS[i].clone();
                    let scale = size / 3.0;
                    (&layer.coordinate_format, &mut char).scale(scale, scale);
                    let width = (&Unit::Millimeters, &char).get_size().width;
                    (&Unit::Millimeters, &mut char).transform(&pos);
                    layer.commands.extend(char);
                    pos.x += width + 0.3 * size;
                }
                _ => {}
            }
        }

        let (min, max) = layer.get_corners();
        let x_shift = match h_align {
            HAlign::Left => -min.x,
            HAlign::Center => -(min.x + max.x) / 2.0,
            HAlign::Right => -max.x,
        };
        let y_shift = match v_align {
            VAlign::Bottom => -min.y,
            VAlign::Middle => -(min.y + max.y) / 2.0,
            VAlign::Top => -max.y,
        };
        if x_shift != 0.0 || y_shift != 0.0 {
            layer.transform(&Pos {
                x: x_shift,
                y: y_shift,
            });
        }

        layer
    }
}

/// Loads font from gerber file
fn load_ascii() -> Vec<Vec<Command>> {
    let raw = include_str!("gerber/kicad_font.gbr");
    let reader = BufReader::new(raw.as_bytes());
    let data = gerber_parser::parse(reader).unwrap();

    let mut stokes = Vec::new();
    let mut current_stroke = Vec::new();
    for command in data.commands() {
        if let Command::FunctionCode(FunctionCode::DCode(DCode::Operation(op))) = command {
            match op {
                Operation::Interpolate(_, _) | Operation::Flash(_) => {
                    if !current_stroke.is_empty() {
                        current_stroke.push(command.clone());
                    }
                }
                Operation::Move(_) => {
                    if !current_stroke.is_empty() {
                        stokes.push(current_stroke)
                    }
                    current_stroke = vec![command.clone()];
                }
            }
        }
    }
    let mut chars: Vec<Vec<Command>> = vec![Vec::new(); CHAR_SIZE as usize];
    for mut stroke in stokes {
        let (min, _) = (&Unit::Millimeters, &stroke).get_corners();
        let key = ((min.y + 2.4) / TEXT_HEIGHT) as u8;
        (&Unit::Millimeters, &mut stroke).transform(&Pos {
            x: 0.0,
            y: key as f64 * -TEXT_HEIGHT,
        });
        let key = CHAR_SIZE - key - 1;
        chars[key as usize].extend(stroke);
    }
    for char in chars.iter_mut() {
        let x = -(&Unit::Millimeters, &*char).get_corners().0.x;
        (&Unit::Millimeters, char).transform(&Pos { x, y: 0.0 });
    }
    chars
}

lazy_static! {
    static ref CHARS: Vec<Vec<Command>> = load_ascii();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::BufWriter;

    #[test]
    fn test_ascii() -> Result<(), Box<dyn std::error::Error>> {
        let layer =
            AsciiText::new(3.0).build("~Hello \"World\" 0123456789", LayerType::SilkScreenTop);
        fs::create_dir_all("output")?;
        let file = fs::File::create("output/ascii.gbr")?;
        let mut writer = BufWriter::new(file);
        layer.write_to(&mut writer)?;
        Ok(())
    }
}
