//! Flattening of Excellon drill/route layers into a [`FlatLayer`].
//!
//! Each tool becomes a circular aperture keyed by its tool number. Drill
//! hits are flashes, routed moves and G85 slots are strokes.

use gerber_parser::gerber_types::{Aperture, Circle};

use super::aperture::aperture_shapes;
use super::geom::Point;
use super::layer::{
    Affine2, ApertureImage, FlatLayer, FlattenError, FlattenIssue, FlattenIssueKind,
    FlattenOptions, GraphicsObject, Op, Path, Polarity, Segment, Shape, SourceRange, SpecFeature,
    push_op,
};
use crate::Pos;
use crate::excellon_format::{
    Command, ExcellonLayerData, GeometricCode, MachineCode, Mode, resolve_point,
};
use crate::unit_able::UnitAble;

impl ExcellonLayerData {
    /// Flattens the layer with default [`FlattenOptions`].
    pub fn flatten(&self) -> Result<FlatLayer, FlattenError> {
        self.flatten_with(&FlattenOptions::default())
    }

    /// Flattens the layer into plain geometry in millimetres.
    ///
    /// Routing follows the plunge/retract codes when the program uses them
    /// (`M15` down, `M16`/`M17` up); otherwise every `G01`–`G03` move cuts.
    /// Circular routes (`A` radius or `I`/`J` centre) keep their exact arc;
    /// a circular move without arc parameters is drawn as a chord and
    /// reported.
    pub fn flatten_with(&self, options: &FlattenOptions) -> Result<FlatLayer, FlattenError> {
        let mut out = FlatLayer::default();
        let uses_router = self
            .commands
            .iter()
            .any(|c| matches!(c, Ok(Command::Machine(MachineCode::RouterDown))));
        // Aperture code of the selected tool; the parser rejects undefined
        // tools, so every selected tool has a diameter.
        let mut code: Option<i32> = None;
        let mut mode = Mode::DrillMode;
        let mut router_down = false;
        let mut current = Pos::default();
        let mut reported_no_tool = false;
        let mut reported_arc = false;

        for (index, command) in self.commands.iter().enumerate() {
            let command = match command {
                Ok(c) => c,
                Err(e) => {
                    issue(&mut out, index, FlattenIssueKind::Unparsed(e.to_string()));
                    continue;
                }
            };
            // `start` is `None` for a drill hit; `center` is set for arcs.
            let (start, end, center) = match command {
                Command::Tool(t) => {
                    code = self.tool_aperture(*t, &mut out, options);
                    continue;
                }
                Command::Geometric(GeometricCode::Mode(m)) => {
                    mode = *m;
                    continue;
                }
                Command::Machine(MachineCode::RouterDown) => {
                    router_down = true;
                    continue;
                }
                Command::Machine(MachineCode::RouterUp) => {
                    router_down = false;
                    continue;
                }
                Command::Coordinate(x, y, fmt) | Command::Arc { x, y, fmt, .. } => {
                    let start = current.clone();
                    current = resolve_point(*x, *y, &fmt.unit, &current);
                    let arc = match command {
                        Command::Arc { center, .. } => Some(center),
                        _ => None,
                    };
                    let cuts = match mode {
                        Mode::DrillMode | Mode::CannedDrill => None,
                        _ if uses_router => Some(router_down),
                        Mode::Route => Some(false),
                        Mode::Linear | Mode::CircularCW | Mode::CircularCWW => Some(true),
                    };
                    match cuts {
                        // A drill hit.
                        None => (None, point(&current), None),
                        Some(true) => {
                            *out.features.entry(SpecFeature::ExcellonRoute).or_default() += 1;
                            let (start, end) = (point(&start), point(&current));
                            let ccw = mode == Mode::CircularCWW;
                            let circular = matches!(mode, Mode::CircularCW | Mode::CircularCWW);
                            let center = arc
                                .filter(|_| circular)
                                .map(|c| c.center_mm(start, end, &fmt.unit, ccw));
                            if circular && center.is_none() && !reported_arc {
                                reported_arc = true;
                                issue(
                                    &mut out,
                                    index,
                                    FlattenIssueKind::Unsupported(
                                        "circular route without A or I/J (drawn straight)",
                                    ),
                                );
                            }
                            (Some(start), end, center.map(|c| (c, ccw)))
                        }
                        // Reposition with the router up.
                        Some(false) => continue,
                    }
                }
                Command::Slot {
                    from_x,
                    from_y,
                    to_x,
                    to_y,
                    fmt,
                } => {
                    let from = resolve_point(*from_x, *from_y, &fmt.unit, &current);
                    current = resolve_point(*to_x, *to_y, &fmt.unit, &from);
                    *out.features.entry(SpecFeature::ExcellonSlot).or_default() += 1;
                    (Some(point(&from)), point(&current), None)
                }
                _ => continue,
            };
            let Some(code) = code else {
                if !reported_no_tool {
                    reported_no_tool = true;
                    issue(&mut out, index, FlattenIssueKind::NoAperture);
                }
                continue;
            };
            let (shape, transform) = match start {
                None => (Shape::Flash, Affine2::translate(end[0], end[1])),
                Some(start) => {
                    let segment = match center {
                        Some((center, ccw)) => Segment::Arc {
                            to: end,
                            center,
                            ccw,
                        },
                        None => Segment::Line { to: end },
                    };
                    let path = Path {
                        start,
                        segments: vec![segment],
                        sources: vec![SourceRange::single(index)],
                    };
                    (Shape::Path(path), Affine2::IDENTITY)
                }
            };
            push_op(
                &mut out.root,
                Op::Object(GraphicsObject {
                    shape,
                    polarity: Polarity::Dark,
                    source: SourceRange::single(index),
                    aperture: Some(code),
                    transform,
                }),
            );
        }
        Ok(out)
    }

    /// Aperture code for `tool`, creating its image on first use.
    fn tool_aperture(
        &self,
        tool: u32,
        out: &mut FlatLayer,
        options: &FlattenOptions,
    ) -> Option<i32> {
        // Excellon tool numbers have at most a few digits; anything larger
        // was rejected by the parser as an undefined tool.
        let code = i32::try_from(tool).ok()?;
        if let std::collections::btree_map::Entry::Vacant(slot) = out.apertures.entry(code) {
            let diameter = self.tools.get(&tool)?;
            let aperture = Aperture::Circle(Circle {
                diameter: diameter.to_mm(&self.unit.unit),
                hole_diameter: None,
            });
            let shapes = aperture_shapes(&aperture, &Default::default(), options.tolerance_mm)
                .expect("circle apertures always evaluate");
            slot.insert(ApertureImage { aperture, shapes });
        }
        Some(code)
    }
}

fn point(p: &Pos) -> Point {
    [p.x, p.y]
}

fn issue(out: &mut FlatLayer, source: usize, kind: FlattenIssueKind) {
    out.issues.push(FlattenIssue { source, kind });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::excellon_format::parse_excellon;
    use std::io::{BufReader, Cursor};

    fn flat(raw: &str) -> FlatLayer {
        parse_excellon(BufReader::new(Cursor::new(raw)))
            .unwrap()
            .flatten()
            .unwrap()
    }

    fn objects(layer: &FlatLayer) -> Vec<&GraphicsObject> {
        layer.iter_expanded().map(|e| e.object).collect()
    }

    #[test]
    fn drill_hits_are_flashes() {
        let layer = flat("M48\nINCH\nT01C0.1\n%\nG05\nT01\nX1.0Y1.0\nX2.0Y1.0\nM30\n");
        let objs = objects(&layer);
        assert_eq!(objs.len(), 2);
        assert_eq!(objs[1].shape, Shape::Flash);
        assert_eq!(objs[1].transform, Affine2::translate(50.8, 25.4));
        let Aperture::Circle(c) = &layer.apertures[&1].aperture else {
            panic!("expected a circle");
        };
        assert!((c.diameter - 2.54).abs() < 1e-9);
        assert!(layer.issues.is_empty(), "{:?}", layer.issues);
    }

    #[test]
    fn router_codes_control_cutting() {
        // Plunge, cut two segments, retract, reposition with G01, plunge again.
        let layer = flat(
            "M48\nMETRIC\nT01C0.6\n%\nT01\nG00X1.0Y1.0\nM15\nG01X2.0\nX2.0Y2.0\nM16\n\
             G01X5.0Y5.0\nM15\nG01X6.0\nM17\nM30\n",
        );
        let paths: Vec<_> = objects(&layer)
            .iter()
            .map(|o| match &o.shape {
                Shape::Path(p) => (p.start, p.end(), p.segments.len()),
                s => panic!("unexpected {s:?}"),
            })
            .collect();
        assert_eq!(
            paths,
            [([1.0, 1.0], [2.0, 2.0], 2), ([5.0, 5.0], [6.0, 5.0], 1)]
        );
    }

    #[test]
    fn g01_cuts_without_router_codes() {
        let layer = flat("M48\nMETRIC\nT01C0.6\n%\nT01\nG00X1.0Y1.0\nG01X2.0\nG00X3.0\nM30\n");
        assert_eq!(objects(&layer).len(), 1);
    }

    #[test]
    fn circular_routes_keep_exact_arcs() {
        let layer = flat(
            "M48\nMETRIC\nT01C0.6\n%\nT01\nG00X1.0Y0.0\nG03X0.0Y1.0A1.0\n\
             G02X1.0Y2.0I1.0J0.0\nG03X0.0Y3.0\nM30\n",
        );
        let Shape::Path(p) = &objects(&layer)[0].shape else {
            panic!("expected a path");
        };
        assert_eq!(p.segments.len(), 3);
        let Segment::Arc { center, ccw, .. } = p.segments[0] else {
            panic!("expected an arc");
        };
        assert!(
            ccw && center[0].abs() < 1e-12 && center[1].abs() < 1e-12,
            "{center:?}"
        );
        assert_eq!(
            p.segments[1],
            Segment::Arc {
                to: [1.0, 2.0],
                center: [1.0, 1.0],
                ccw: false
            }
        );
        assert!(matches!(p.segments[2], Segment::Line { .. }));
        assert_eq!(layer.issues.len(), 1);
    }

    #[test]
    fn slots_are_strokes() {
        let layer = flat("M48\nMETRIC\nT01C0.6\n%\nT01\nX1.0Y1.0G85X3.0Y1.0\nM30\n");
        let objs = objects(&layer);
        assert_eq!(objs.len(), 1);
        let Shape::Path(p) = &objs[0].shape else {
            panic!("expected a slot path");
        };
        assert_eq!((p.start, p.end()), ([1.0, 1.0], [3.0, 1.0]));
        assert_eq!(layer.features[&SpecFeature::ExcellonSlot], 1);
    }
}
