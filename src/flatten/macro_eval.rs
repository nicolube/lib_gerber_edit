//! Evaluation of aperture macros (Gerber spec §4.5) into polygons.
//!
//! Primitives are processed in order. Exposure-on primitives are added to
//! the aperture image, exposure-off primitives erase what earlier primitives
//! of the same macro drew; they never affect anything outside the flash.

use gerber_parser::gerber_types::{MacroBoolean, MacroContent, MacroDecimal, MacroInteger};

use super::expr::{self, ExprError, Variables};
use super::geom::{self, Point, Shapes};

/// Most primitives evaluated per macro instance.
const MAX_MACRO_PRIMITIVES: usize = 10_000;
/// Most vertices accepted in one outline primitive.
const MAX_OUTLINE_POINTS: usize = 100_000;
/// Most rings a moiré primitive may draw.
const MAX_MOIRE_RINGS: u32 = 1_000;

/// Why an aperture could not be turned into a shape.
#[derive(thiserror::Error, Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum MacroError {
    #[error("aperture macro '{0}' is not defined")]
    UnknownMacro(String),
    #[error("aperture macro '{name}' argument {index}: {reason}")]
    InvalidArgument {
        name: String,
        index: usize,
        reason: String,
    },
    #[error("macro '{name}' primitive {index}: {source}")]
    Expr {
        name: String,
        index: usize,
        #[source]
        source: ExprError,
    },
    #[error("macro '{name}' primitive {index}: {reason}")]
    InvalidPrimitive {
        name: String,
        index: usize,
        reason: String,
    },
    #[error("macro '{name}' has more than {limit} primitives")]
    TooManyPrimitives { name: String, limit: usize },
}

/// Evaluates macro `name` with the aperture-definition arguments `args`.
///
/// `tolerance` is the maximum chord deviation (mm) for circles and arcs.
pub fn eval_macro(
    name: &str,
    content: &[MacroContent],
    args: &[f64],
    tolerance: f64,
) -> Result<Shapes, MacroError> {
    let mut vars = Variables::from_args(args);
    let mut image: Shapes = Vec::new();
    // Consecutive primitives with the same exposure are combined in one
    // boolean operation.
    let mut pending: Shapes = Vec::new();
    let mut pending_on = true;
    let mut primitives = 0;

    for (index, item) in content.iter().enumerate() {
        let ctx = Ctx {
            name,
            index,
            vars: &vars,
            tolerance,
        };
        let (on, shapes) = match item {
            MacroContent::Comment(_) => continue,
            MacroContent::VariableDefinition(def) => {
                let value = expr::eval(&def.expression, &vars).map_err(|e| ctx.expr_err(e))?;
                vars.set(def.number, value);
                continue;
            }
            primitive => ctx.primitive(primitive)?,
        };
        primitives += 1;
        if primitives > MAX_MACRO_PRIMITIVES {
            return Err(MacroError::TooManyPrimitives {
                name: name.to_string(),
                limit: MAX_MACRO_PRIMITIVES,
            });
        }
        if on != pending_on && !pending.is_empty() {
            image = apply(image, std::mem::take(&mut pending), pending_on);
        }
        pending_on = on;
        pending.extend(shapes);
    }
    Ok(apply(image, pending, pending_on))
}

fn apply(image: Shapes, shapes: Shapes, on: bool) -> Shapes {
    if shapes.is_empty() {
        image
    } else if on {
        geom::union(image, shapes)
    } else {
        geom::difference(image, &shapes)
    }
}

/// A macro parameter that evaluates to a number.
trait MacroValue {
    fn resolve(&self, vars: &Variables) -> Result<f64, ExprError>;
}

impl MacroValue for MacroDecimal {
    fn resolve(&self, vars: &Variables) -> Result<f64, ExprError> {
        match self {
            MacroDecimal::Value(v) => Ok(*v),
            MacroDecimal::Variable(n) => Ok(vars.get(*n)),
            MacroDecimal::Expression(e) => expr::eval(e, vars),
        }
    }
}

impl MacroValue for MacroBoolean {
    fn resolve(&self, vars: &Variables) -> Result<f64, ExprError> {
        match self {
            MacroBoolean::Value(v) => Ok(f64::from(u8::from(*v))),
            MacroBoolean::Variable(n) => Ok(vars.get(*n)),
            MacroBoolean::Expression(e) => expr::eval(e, vars),
        }
    }
}

impl MacroValue for MacroInteger {
    fn resolve(&self, vars: &Variables) -> Result<f64, ExprError> {
        match self {
            MacroInteger::Value(v) => Ok(f64::from(*v)),
            MacroInteger::Variable(n) => Ok(vars.get(*n)),
            MacroInteger::Expression(e) => expr::eval(e, vars),
        }
    }
}

struct Ctx<'a> {
    name: &'a str,
    index: usize,
    vars: &'a Variables,
    tolerance: f64,
}

impl Ctx<'_> {
    fn expr_err(&self, source: ExprError) -> MacroError {
        MacroError::Expr {
            name: self.name.to_string(),
            index: self.index,
            source,
        }
    }

    fn invalid(&self, reason: impl Into<String>) -> MacroError {
        MacroError::InvalidPrimitive {
            name: self.name.to_string(),
            index: self.index,
            reason: reason.into(),
        }
    }

    fn num(&self, v: &impl MacroValue) -> Result<f64, MacroError> {
        v.resolve(self.vars).map_err(|e| self.expr_err(e))
    }

    /// A size parameter, which must not be negative.
    fn len(&self, v: &MacroDecimal, what: &str) -> Result<f64, MacroError> {
        let value = self.num(v)?;
        if value < 0.0 {
            return Err(self.invalid(format!("{what} {value} is negative")));
        }
        Ok(value)
    }

    fn point(&self, p: &(MacroDecimal, MacroDecimal)) -> Result<Point, MacroError> {
        Ok([self.num(&p.0)?, self.num(&p.1)?])
    }

    /// Returns the primitive's exposure and its shapes, rotated about the
    /// macro origin.
    fn primitive(&self, item: &MacroContent) -> Result<(bool, Shapes), MacroError> {
        // Moiré and thermal have no exposure parameter; they are always on.
        let (exposure, shapes, angle) = match item {
            MacroContent::Circle(c) => {
                let d = self.len(&c.diameter, "diameter")?;
                let angle = c.angle.as_ref().map(|a| self.num(a)).transpose()?;
                let contour = geom::circle(self.point(&c.center)?, d, self.tolerance);
                (
                    Some(&c.exposure),
                    geom::simple(contour),
                    angle.unwrap_or(0.0),
                )
            }
            MacroContent::VectorLine(l) => {
                let w = self.len(&l.width, "width")?;
                let (s, e) = (self.point(&l.start)?, self.point(&l.end)?);
                let (dx, dy) = (e[0] - s[0], e[1] - s[1]);
                let len = dx.hypot(dy);
                let shapes = if len == 0.0 || w == 0.0 {
                    Vec::new()
                } else {
                    // Unit normal scaled to half the width.
                    let (nx, ny) = (-dy / len * w / 2.0, dx / len * w / 2.0);
                    geom::simple(vec![
                        [s[0] - nx, s[1] - ny],
                        [e[0] - nx, e[1] - ny],
                        [e[0] + nx, e[1] + ny],
                        [s[0] + nx, s[1] + ny],
                    ])
                };
                (Some(&l.exposure), shapes, self.num(&l.angle)?)
            }
            MacroContent::CenterLine(l) => {
                let w = self.len(&l.dimensions.0, "width")?;
                let h = self.len(&l.dimensions.1, "height")?;
                let contour = geom::rect(self.point(&l.center)?, w, h);
                (
                    Some(&l.exposure),
                    geom::simple(contour),
                    self.num(&l.angle)?,
                )
            }
            MacroContent::Outline(o) => {
                if o.points.len() > MAX_OUTLINE_POINTS {
                    return Err(
                        self.invalid(format!("outline has more than {MAX_OUTLINE_POINTS} points"))
                    );
                }
                let mut contour = o
                    .points
                    .iter()
                    .map(|p| self.point(p))
                    .collect::<Result<Vec<_>, _>>()?;
                // The spec repeats the start point at the end.
                if contour.len() > 1 && contour.first() == contour.last() {
                    contour.pop();
                }
                if contour.len() < 3 {
                    return Err(self.invalid("outline needs at least 3 distinct points"));
                }
                (
                    Some(&o.exposure),
                    geom::simple(contour),
                    self.num(&o.angle)?,
                )
            }
            MacroContent::Polygon(p) => {
                let n = self.num(&p.vertices)?;
                if !(3.0..=12.0).contains(&n) || n.fract() != 0.0 {
                    return Err(self.invalid(format!("polygon vertex count {n} is not 3..12")));
                }
                let d = self.len(&p.diameter, "diameter")?;
                let contour = geom::regular_polygon(self.point(&p.center)?, d, n as u32, 0.0);
                (
                    Some(&p.exposure),
                    geom::simple(contour),
                    self.num(&p.angle)?,
                )
            }
            MacroContent::Moire(m) => {
                if m.max_rings > MAX_MOIRE_RINGS {
                    return Err(
                        self.invalid(format!("moiré has more than {MAX_MOIRE_RINGS} rings"))
                    );
                }
                let c = self.point(&m.center)?;
                let t = self.len(&m.ring_thickness, "ring thickness")?;
                let gap = self.len(&m.gap, "gap")?;
                let ch_t = self.len(&m.cross_hair_thickness, "cross hair thickness")?;
                let ch_l = self.len(&m.cross_hair_length, "cross hair length")?;
                // Concentric rings never overlap, so they need no boolean.
                let mut shapes: Shapes = Vec::new();
                let mut outer = self.len(&m.diameter, "diameter")?;
                for _ in 0..m.max_rings {
                    if outer <= 0.0 {
                        break;
                    }
                    let inner = outer - 2.0 * t;
                    shapes.extend(geom::annulus(c, outer, inner, self.tolerance));
                    outer = inner - 2.0 * gap;
                }
                shapes.extend(geom::simple(geom::rect(c, ch_l, ch_t)));
                shapes.extend(geom::simple(geom::rect(c, ch_t, ch_l)));
                (None, shapes, self.num(&m.angle)?)
            }
            MacroContent::Thermal(th) => {
                let c = self.point(&th.center)?;
                let outer = self.len(&th.outer_diameter, "outer diameter")?;
                let inner = self.len(&th.inner_diameter, "inner diameter")?;
                let gap = self.len(&th.gap, "gap")?;
                if inner >= outer {
                    return Err(self.invalid("thermal inner diameter must be below the outer"));
                }
                let span = outer * 2.0;
                let mut cut = geom::simple(geom::rect(c, span, gap));
                cut.extend(geom::simple(geom::rect(c, gap, span)));
                let ring = geom::annulus(c, outer, inner, self.tolerance);
                (None, geom::difference(ring, &cut), self.num(&th.angle)?)
            }
            MacroContent::Comment(_) | MacroContent::VariableDefinition(_) => {
                unreachable!("handled by eval_macro")
            }
        };
        let on = match exposure {
            Some(e) => self.num(e)? != 0.0,
            None => true,
        };
        Ok((on, geom::rotate(shapes, angle)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use MacroDecimal::{Expression, Value, Variable};
    use gerber_parser::gerber_types::{
        CenterLinePrimitive, CirclePrimitive, MoirePrimitive, OutlinePrimitive, PolygonPrimitive,
        ThermalPrimitive, VariableDefinition, VectorLinePrimitive,
    };
    use i_overlay::i_shape::float::area::Area;
    use std::f64::consts::PI;

    const TOL: f64 = 0.0001;

    fn eval(content: Vec<MacroContent>, args: &[f64]) -> Shapes {
        eval_macro("M", &content, args, TOL).unwrap()
    }

    fn bounds(s: &Shapes) -> (Point, Point) {
        s.iter()
            .flatten()
            .flatten()
            .fold(([f64::MAX; 2], [f64::MIN; 2]), |(lo, hi), p| {
                (
                    [lo[0].min(p[0]), lo[1].min(p[1])],
                    [hi[0].max(p[0]), hi[1].max(p[1])],
                )
            })
    }

    fn circle(on: bool, d: MacroDecimal) -> MacroContent {
        MacroContent::Circle(CirclePrimitive {
            exposure: MacroBoolean::Value(on),
            diameter: d,
            center: (Value(0.0), Value(0.0)),
            angle: None,
        })
    }

    fn center_line(w: f64, h: f64, angle: f64) -> MacroContent {
        MacroContent::CenterLine(CenterLinePrimitive {
            exposure: MacroBoolean::Value(true),
            dimensions: (Value(w), Value(h)),
            center: (Value(0.0), Value(0.0)),
            angle: Value(angle),
        })
    }

    #[test]
    fn circle_from_variable_and_definition() {
        let s = eval(vec![circle(true, Variable(1))], &[2.0]);
        assert!((s.area() - PI).abs() < 1e-3);

        let s = eval(
            vec![
                MacroContent::VariableDefinition(VariableDefinition::new(2, "$1x2")),
                circle(true, Expression("$2".into())),
            ],
            &[1.0],
        );
        assert!((s.area() - PI).abs() < 1e-3);
    }

    #[test]
    fn exposure_off_erases_earlier_primitives() {
        let s = eval(
            vec![center_line(2.0, 2.0, 0.0), circle(false, Value(1.0))],
            &[],
        );
        assert_eq!(s[0].len(), 2, "circle punches a hole");
        assert!((s.area() - (4.0 - PI / 4.0)).abs() < 1e-3);
        // Exposure off before anything is drawn leaves nothing.
        let s = eval(vec![circle(false, Value(1.0))], &[]);
        assert!(s.is_empty());
    }

    #[test]
    fn rotation_is_about_macro_origin() {
        let line = MacroContent::VectorLine(VectorLinePrimitive {
            exposure: MacroBoolean::Value(true),
            width: Value(1.0),
            start: (Value(0.0), Value(0.0)),
            end: (Value(2.0), Value(0.0)),
            angle: Value(90.0),
        });
        let (lo, hi) = bounds(&eval(vec![line], &[]));
        assert!((lo[0] + 0.5).abs() < 1e-9 && (hi[0] - 0.5).abs() < 1e-9);
        assert!(lo[1].abs() < 1e-9 && (hi[1] - 2.0).abs() < 1e-9);

        let (lo, hi) = bounds(&eval(vec![center_line(4.0, 1.0, 90.0)], &[]));
        assert!((hi[0] - lo[0] - 1.0).abs() < 1e-9 && (hi[1] - lo[1] - 4.0).abs() < 1e-9);
    }

    #[test]
    fn outline_drops_repeated_start_point() {
        let pts = [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 0.0)];
        let outline = MacroContent::Outline(OutlinePrimitive {
            exposure: MacroBoolean::Value(true),
            points: pts.iter().map(|&(x, y)| (Value(x), Value(y))).collect(),
            angle: Value(0.0),
        });
        let s = eval(vec![outline], &[]);
        assert_eq!(s[0][0].len(), 3);
        assert!((s.area() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn polygon_vertex_count_is_checked() {
        let poly = |n| {
            MacroContent::Polygon(PolygonPrimitive {
                exposure: MacroBoolean::Value(true),
                vertices: MacroInteger::Value(n),
                center: (Value(0.0), Value(0.0)),
                diameter: Value(2.0),
                angle: Value(0.0),
            })
        };
        let s = eval(vec![poly(4)], &[]);
        assert!((s.area() - 2.0).abs() < 1e-9);
        let err = eval_macro("M", &[poly(13)], &[], TOL).unwrap_err();
        assert!(matches!(err, MacroError::InvalidPrimitive { index: 0, .. }));
    }

    #[test]
    fn moire_rings_keep_their_holes() {
        let moire = MacroContent::Moire(MoirePrimitive {
            center: (Value(0.0), Value(0.0)),
            diameter: Value(2.0),
            ring_thickness: Value(0.2),
            gap: Value(0.2),
            max_rings: 2,
            cross_hair_thickness: Value(0.0),
            cross_hair_length: Value(0.0),
            angle: Value(0.0),
        });
        // Rings 2.0/1.6 and 1.2/0.8.
        let expected = PI * (1.0 - 0.64 + 0.36 - 0.16);
        assert!((eval(vec![moire], &[]).area() - expected).abs() < 1e-3);
    }

    #[test]
    fn thermal_has_four_spokes_cut() {
        let thermal = MacroContent::Thermal(ThermalPrimitive {
            center: (Value(0.0), Value(0.0)),
            outer_diameter: Value(2.0),
            inner_diameter: Value(1.0),
            gap: Value(0.2),
            angle: Value(0.0),
        });
        let s = eval(vec![thermal], &[]);
        assert_eq!(s.len(), 4);
        let annulus = PI * (1.0 - 0.25);
        let a = s.area();
        assert!(a < annulus - 0.35 && a > annulus - 0.45, "area {a}");
    }

    #[test]
    fn bad_expression_reports_primitive() {
        let err = eval_macro("M", &[circle(true, Expression("1/0".into()))], &[], TOL).unwrap_err();
        assert!(matches!(err, MacroError::Expr { index: 0, .. }));
    }
}
