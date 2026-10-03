//! Aperture images (Gerber spec §4.4) as polygons centred on the flash point.

use std::collections::HashMap;

use gerber_parser::gerber_types::{Aperture, MacroContent, MacroDecimal};

use super::expr::{self, Variables};
use super::geom::{self, Shapes};
use super::macro_eval::{self, MacroError};

/// Polygon image of `aperture`, including any round hole.
///
/// `macros` are the layer's aperture macros; `tolerance` is the maximum chord
/// deviation (mm) for circles.
pub fn aperture_shapes(
    aperture: &Aperture,
    macros: &HashMap<String, Vec<MacroContent>>,
    tolerance: f64,
) -> Result<Shapes, MacroError> {
    let origin = [0.0, 0.0];
    let (outline, hole) = match aperture {
        Aperture::Circle(c) => (geom::circle(origin, c.diameter, tolerance), c.hole_diameter),
        Aperture::Rectangle(r) => (geom::rect(origin, r.x, r.y), r.hole_diameter),
        Aperture::Obround(r) => (geom::obround(origin, r.x, r.y, tolerance), r.hole_diameter),
        Aperture::Polygon(p) => (
            geom::regular_polygon(
                origin,
                p.diameter,
                u32::from(p.vertices),
                p.rotation.unwrap_or(0.0),
            ),
            p.hole_diameter,
        ),
        Aperture::Macro(name, args) => {
            let content = macros
                .get(name)
                .ok_or_else(|| MacroError::UnknownMacro(name.clone()))?;
            let args = args
                .iter()
                .flatten()
                .enumerate()
                .map(|(index, a)| {
                    let reason = match a {
                        MacroDecimal::Value(v) => return Ok(*v),
                        MacroDecimal::Expression(e) => match expr::eval(e, &Variables::default()) {
                            Ok(v) => return Ok(v),
                            Err(err) => err.to_string(),
                        },
                        MacroDecimal::Variable(n) => format!("${n} is not a number"),
                    };
                    Err(MacroError::InvalidArgument {
                        name: name.clone(),
                        index,
                        reason,
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            return macro_eval::eval_macro(name, content, &args, tolerance);
        }
    };
    let image = geom::simple(outline);
    Ok(match hole {
        Some(d) if d > 0.0 => {
            geom::difference(image, &geom::simple(geom::circle(origin, d, tolerance)))
        }
        _ => image,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gerber_parser::gerber_types::{Circle, Polygon, Rectangular};
    use i_overlay::i_shape::float::area::Area;
    use std::f64::consts::PI;

    const TOL: f64 = 0.0001;

    fn shapes(ap: Aperture) -> Shapes {
        aperture_shapes(&ap, &HashMap::new(), TOL).unwrap()
    }

    #[test]
    fn circle_with_hole() {
        let s = shapes(Aperture::Circle(Circle {
            diameter: 2.0,
            hole_diameter: Some(1.0),
        }));
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].len(), 2);
        assert!((s.area() - PI * 0.75).abs() < 1e-3);
    }

    #[test]
    fn rectangle_and_obround() {
        let r = Rectangular {
            x: 2.0,
            y: 1.0,
            hole_diameter: None,
        };
        assert!((shapes(Aperture::Rectangle(r.clone())).area() - 2.0).abs() < 1e-9);
        let o = shapes(Aperture::Obround(r)).area();
        assert!((o - (1.0 + PI * 0.25)).abs() < 1e-3);
    }

    #[test]
    fn polygon_rotation() {
        let s = shapes(Aperture::Polygon(Polygon {
            diameter: 2.0,
            vertices: 4,
            rotation: Some(45.0),
            hole_diameter: None,
        }));
        // A square of circumradius 1 rotated 45° is axis-aligned with side √2.
        assert!((s.area() - 2.0).abs() < 1e-6);
        let max_x = s[0][0].iter().map(|p| p[0]).fold(f64::MIN, f64::max);
        assert!((max_x - std::f64::consts::FRAC_1_SQRT_2).abs() < 1e-6);
    }

    #[test]
    fn unknown_macro() {
        let err = aperture_shapes(&Aperture::Macro("NOPE".into(), None), &HashMap::new(), TOL);
        assert_eq!(err, Err(MacroError::UnknownMacro("NOPE".into())));
    }
}
