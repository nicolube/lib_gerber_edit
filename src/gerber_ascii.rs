use crate::gerber::GerberLayerData;
use crate::{LayerCorners, LayerScale, LayerTransform, LayerType, Pos};
use gerber_parser::gerber_types::{
    Aperture, Circle, Command, DCode, FunctionCode, GCode, InterpolationMode, Operation, Unit,
};
use lazy_static::lazy_static;
use std::io::BufReader;

const CHAR_SIZE: u8 = 94;
const TEXT_HEIGHT: f64 = 9.6577;

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

/// Generates gerber commands for given text and size into a new gerber layer
pub fn write_ascii<T>(txt: T, size: f64, ratio: f64, layer_type: LayerType) -> GerberLayerData
where
    T: AsRef<str>,
{
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

    let txt = txt.as_ref();
    let lines = txt.lines().count();
    let mut pos = Pos {
        x: 0.0,
        y: (lines - 1) as f64 * height,
    };
    for c in txt.chars() {
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

    layer
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
        let layer = write_ascii(
            "~Hello \"World\" 0123456789".to_string(),
            3.0,
            1.0,
            LayerType::SilkScreenTop,
        );
        fs::create_dir_all("output")?;
        let file = fs::File::create("output/ascii.gbr")?;
        let mut writer = BufWriter::new(file);
        layer.write_to(&mut writer)?;
        Ok(())
    }
}
