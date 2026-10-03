//! Flattening of RS-274X layers into a [`FlatLayer`].

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use gerber_parser::gerber_types::{
    Aperture, ApertureBlock, Command, CoordinateOffset, Coordinates, DCode, ExtendedCode,
    FunctionCode, GCode, ImagePolarity, InterpolationMode, Mirroring, Operation, QuadrantMode,
    StepAndRepeat, Unit,
};

use super::aperture::aperture_shapes;
use super::geom::{Point, single_quadrant_center};
use super::layer::{
    Affine2, ApertureImage, DefId, Definition, FlatLayer, FlattenError, FlattenIssue,
    FlattenIssueKind, FlattenOptions, GraphicsObject, Op, Path, Polarity, Segment, Shape,
    SourceRange, SpecFeature, TransformSet, push_op, same_point,
};
use crate::gerber::GerberLayerData;

impl GerberLayerData {
    /// Flattens the layer with default [`FlattenOptions`].
    pub fn flatten(&self) -> Result<FlatLayer, FlattenError> {
        self.flatten_with(&FlattenOptions::default())
    }

    /// Flattens the layer into ordered plain geometry in millimetres.
    ///
    /// Problems with single objects are reported in [`FlatLayer::issues`];
    /// only exceeded [`ResourceLimits`](super::ResourceLimits) fail the
    /// whole layer.
    pub fn flatten_with(&self, options: &FlattenOptions) -> Result<FlatLayer, FlattenError> {
        // `to_unit` also rescales apertures and macros, so inch layers are
        // converted as a whole.
        let layer: Cow<GerberLayerData> = if *self.unit() == Unit::Millimeters {
            Cow::Borrowed(self)
        } else {
            Cow::Owned(self.clone().to_unit(&Unit::Millimeters))
        };
        let mut b = Builder::new(&layer, options);
        for command in &layer.header {
            if let Command::ExtendedCode(ext) = command {
                b.extended(0, ext)?;
            }
        }
        for (index, command) in layer.commands.iter().enumerate() {
            b.command(index, command)?;
        }
        b.finish(layer.commands.len())
    }
}

enum ScopeKind {
    Root,
    StepRepeat { nx: u32, ny: u32, dx: f64, dy: f64 },
    Block { code: i32 },
}

/// The root stream or an open SR / AB block collecting its ops.
struct Scope {
    kind: ScopeKind,
    start: usize,
    ops: Vec<Op>,
    /// Instance cells placed inside this scope (nested levels included).
    cells: usize,
}

struct Region {
    start: usize,
    contours: Vec<Path>,
    contour: Option<Path>,
}

struct Builder<'a> {
    layer: &'a GerberLayerData,
    options: &'a FlattenOptions,
    out: FlatLayer,
    /// `scopes[0]` is the root stream and is never popped before `finish`.
    scopes: Vec<Scope>,
    /// Aperture-block D-codes and their definitions.
    blocks: HashMap<i32, DefId>,
    /// Instance cells per single placement of each definition.
    def_cells: Vec<usize>,
    /// D-codes already reported as undefined.
    missing: HashSet<i32>,
    aperture_points: usize,
    current: Point,
    aperture: Option<i32>,
    interpolation: InterpolationMode,
    quadrant: QuadrantMode,
    polarity: Polarity,
    region: Option<Region>,
    /// The move that positioned the next draw, if it came directly before.
    last_move: Option<usize>,
}

impl<'a> Builder<'a> {
    fn new(layer: &'a GerberLayerData, options: &'a FlattenOptions) -> Self {
        Builder {
            layer,
            options,
            out: FlatLayer::default(),
            scopes: vec![Scope {
                kind: ScopeKind::Root,
                start: 0,
                ops: Vec::new(),
                cells: 0,
            }],
            blocks: HashMap::new(),
            def_cells: Vec::new(),
            missing: HashSet::new(),
            aperture_points: 0,
            current: [0.0, 0.0],
            aperture: None,
            interpolation: InterpolationMode::Linear,
            quadrant: QuadrantMode::Multi,
            polarity: Polarity::Dark,
            region: None,
            last_move: None,
        }
    }

    fn feature(&mut self, feature: SpecFeature) {
        *self.out.features.entry(feature).or_default() += 1;
    }

    fn issue(&mut self, source: usize, kind: FlattenIssueKind) {
        self.out.issues.push(FlattenIssue { source, kind });
    }

    fn scope(&mut self) -> &mut Scope {
        self.scopes.last_mut().expect("root scope is always open")
    }

    /// Appends an op to the open scope, extending a continued stroke.
    fn push(&mut self, op: Op) {
        push_op(&mut self.scope().ops, op);
    }

    /// An object at the current polarity.
    fn object(&mut self, shape: Shape, source: SourceRange, aperture: Option<i32>, at: Point) {
        self.push(Op::Object(GraphicsObject {
            shape,
            polarity: self.polarity,
            source,
            aperture,
            transform: Affine2::translate(at[0], at[1]),
        }));
    }

    fn command(&mut self, index: usize, command: &Command) -> Result<(), FlattenError> {
        match command {
            Command::FunctionCode(FunctionCode::DCode(DCode::Operation(op))) => {
                self.operation(index, op)?
            }
            Command::FunctionCode(FunctionCode::DCode(DCode::SelectAperture(code))) => {
                self.aperture = Some(*code);
            }
            Command::FunctionCode(FunctionCode::GCode(g)) => match g {
                GCode::InterpolationMode(mode) => self.interpolation = *mode,
                GCode::QuadrantMode(mode) => self.quadrant = *mode,
                GCode::RegionMode(true) => {
                    self.region = Some(Region {
                        start: index,
                        contours: Vec::new(),
                        contour: None,
                    });
                }
                GCode::RegionMode(false) => self.close_region(index),
                _ => {}
            },
            Command::ExtendedCode(ext) => self.extended(index, ext)?,
            _ => {}
        }
        Ok(())
    }

    fn extended(&mut self, index: usize, ext: &ExtendedCode) -> Result<(), FlattenError> {
        match ext {
            ExtendedCode::LoadPolarity(p) => {
                self.polarity = (*p).into();
                if self.polarity == Polarity::Clear {
                    self.feature(SpecFeature::ClearPolarity);
                }
            }
            ExtendedCode::ImagePolarity(ImagePolarity::Negative) if !self.out.negative => {
                self.out.negative = true;
                self.feature(SpecFeature::ImageNegative);
            }
            ExtendedCode::StepAndRepeat(StepAndRepeat::Open {
                repeat_x,
                repeat_y,
                distance_x,
                distance_y,
            }) => {
                // SR blocks do not nest; a new SR closes the previous one.
                self.close_top_if(index, |k| matches!(k, ScopeKind::StepRepeat { .. }))?;
                self.open_scope(
                    index,
                    ScopeKind::StepRepeat {
                        nx: *repeat_x,
                        ny: *repeat_y,
                        dx: *distance_x,
                        dy: *distance_y,
                    },
                );
            }
            ExtendedCode::StepAndRepeat(StepAndRepeat::Close) => {
                self.close_top_if(index, |k| matches!(k, ScopeKind::StepRepeat { .. }))?
            }
            ExtendedCode::ApertureBlock(ApertureBlock::Open { code }) => {
                let depth = self
                    .scopes
                    .iter()
                    .filter(|s| matches!(s.kind, ScopeKind::Block { .. }))
                    .count();
                if depth >= self.options.limits.max_block_depth {
                    return Err(FlattenError::ResourceLimit {
                        what: "aperture block nesting",
                        limit: self.options.limits.max_block_depth,
                    });
                }
                self.open_scope(index, ScopeKind::Block { code: *code });
            }
            ExtendedCode::ApertureBlock(ApertureBlock::Close) => {
                self.close_top_if(index, |k| matches!(k, ScopeKind::Block { .. }))?
            }
            ExtendedCode::LoadMirroring(m) if *m != Mirroring::None => {
                self.issue(index, FlattenIssueKind::Unsupported("LM (load mirroring)"))
            }
            ExtendedCode::LoadRotation(r) if r.rotation != 0.0 => {
                self.issue(index, FlattenIssueKind::Unsupported("LR (load rotation)"))
            }
            ExtendedCode::LoadScaling(s) if s.scale != 1.0 => {
                self.issue(index, FlattenIssueKind::Unsupported("LS (load scaling)"))
            }
            ExtendedCode::MirrorImage(_)
            | ExtendedCode::OffsetImage(_)
            | ExtendedCode::ScaleImage(_)
            | ExtendedCode::RotateImage(_)
            | ExtendedCode::AxisSelect(_) => self.issue(
                index,
                FlattenIssueKind::Unsupported("image transform (MI/OF/SF/IR/AS)"),
            ),
            _ => {}
        }
        Ok(())
    }

    fn open_scope(&mut self, index: usize, kind: ScopeKind) {
        self.scopes.push(Scope {
            kind,
            start: index,
            ops: Vec::new(),
            cells: 0,
        });
    }

    /// Closes the innermost scope if `is_kind` accepts it.
    fn close_top_if(
        &mut self,
        index: usize,
        is_kind: impl Fn(&ScopeKind) -> bool,
    ) -> Result<(), FlattenError> {
        if self.scopes.len() > 1 && is_kind(&self.scope().kind) {
            self.close_scope(index)?;
        }
        Ok(())
    }

    fn close_scope(&mut self, index: usize) -> Result<(), FlattenError> {
        let scope = self.scopes.pop().expect("caller checked an open scope");
        let def = self.out.definitions.len();
        self.out.definitions.push(Definition { ops: scope.ops });
        self.def_cells.push(scope.cells);
        match scope.kind {
            ScopeKind::Root => unreachable!("the root scope is never closed"),
            ScopeKind::Block { code } => {
                self.feature(SpecFeature::ApertureBlock);
                self.blocks.insert(code, def);
            }
            ScopeKind::StepRepeat { nx, ny, dx, dy } => {
                self.feature(SpecFeature::StepAndRepeat);
                let transforms = TransformSet::Grid {
                    nx,
                    ny,
                    dx,
                    dy,
                    origin: [0.0, 0.0],
                };
                let source = SourceRange {
                    start: scope.start,
                    end: index + 1,
                };
                self.instance(def, transforms, false, source)?;
            }
        }
        Ok(())
    }

    fn instance(
        &mut self,
        def: DefId,
        transforms: TransformSet,
        invert_polarity: bool,
        source: SourceRange,
    ) -> Result<(), FlattenError> {
        let cells = transforms
            .len()
            .saturating_mul(self.def_cells[def].saturating_add(1));
        let limit = self.options.limits.max_instances;
        let scope = self.scope();
        scope.cells = scope.cells.saturating_add(cells);
        if scope.cells > limit {
            return Err(FlattenError::ResourceLimit {
                what: "instance count",
                limit,
            });
        }
        self.push(Op::Instance {
            def,
            transforms,
            invert_polarity,
            source,
        });
        Ok(())
    }

    /// Makes sure `code` has an image in the output; false if undefined.
    fn ensure_aperture(&mut self, index: usize, code: i32) -> Result<bool, FlattenError> {
        if self.out.apertures.contains_key(&code) {
            return Ok(true);
        }
        let Some(aperture) = self.layer.apertures.get(&code) else {
            if self.missing.insert(code) {
                self.issue(index, FlattenIssueKind::UndefinedAperture(code));
            }
            return Ok(false);
        };
        if matches!(aperture, Aperture::Macro(..)) {
            self.feature(SpecFeature::MacroAperture);
        }
        let shapes = match aperture_shapes(aperture, &self.layer.macros, self.options.tolerance_mm)
        {
            Ok(shapes) => shapes,
            Err(e) => {
                self.issue(index, e.into());
                Vec::new()
            }
        };
        self.aperture_points += shapes.iter().flatten().map(Vec::len).sum::<usize>();
        if self.aperture_points > self.options.limits.max_aperture_points {
            return Err(FlattenError::ResourceLimit {
                what: "aperture vertex count",
                limit: self.options.limits.max_aperture_points,
            });
        }
        self.out.apertures.insert(
            code,
            ApertureImage {
                aperture: aperture.clone(),
                shapes,
            },
        );
        Ok(true)
    }

    fn target(&self, coords: &Option<Coordinates>) -> Point {
        let Some(c) = coords else {
            return self.current;
        };
        [
            c.x.map_or(self.current[0], f64::from),
            c.y.map_or(self.current[1], f64::from),
        ]
    }

    fn operation(&mut self, index: usize, op: &Operation) -> Result<(), FlattenError> {
        match op {
            Operation::Move(coords) => {
                self.current = self.target(coords);
                self.last_move = Some(index);
                if let Some(region) = &mut self.region
                    && let Some(contour) = region.contour.take()
                {
                    region.contours.push(contour);
                }
            }
            Operation::Flash(coords) => {
                self.current = self.target(coords);
                self.last_move = None;
                if self.region.is_some() {
                    self.issue(index, FlattenIssueKind::FlashInRegion);
                } else {
                    self.flash(index)?;
                }
            }
            Operation::Interpolate(coords, offset) => {
                let start = self.current;
                let end = self.target(coords);
                self.current = end;
                let segment = self.segment(index, start, end, offset.as_ref());
                let source = SourceRange {
                    start: self.last_move.take().unwrap_or(index),
                    end: index + 1,
                };
                if let Some(region) = &mut self.region {
                    let contour = region.contour.get_or_insert_with(|| Path {
                        start,
                        segments: Vec::new(),
                        sources: Vec::new(),
                    });
                    contour.segments.push(segment);
                    contour.sources.push(source);
                } else {
                    self.stroke(index, source, start, segment)?;
                }
            }
        }
        Ok(())
    }

    fn flash(&mut self, index: usize) -> Result<(), FlattenError> {
        let Some(code) = self.aperture else {
            self.issue(index, FlattenIssueKind::NoAperture);
            return Ok(());
        };
        let source = SourceRange::single(index);
        if let Some(&def) = self.blocks.get(&code) {
            let transforms = TransformSet::Grid {
                nx: 1,
                ny: 1,
                dx: 0.0,
                dy: 0.0,
                origin: self.current,
            };
            let invert = self.polarity == Polarity::Clear;
            return self.instance(def, transforms, invert, source);
        }
        if self.ensure_aperture(index, code)? {
            self.object(Shape::Flash, source, Some(code), self.current);
        }
        Ok(())
    }

    fn stroke(
        &mut self,
        index: usize,
        source: SourceRange,
        start: Point,
        segment: Segment,
    ) -> Result<(), FlattenError> {
        let Some(code) = self.aperture else {
            self.issue(index, FlattenIssueKind::NoAperture);
            return Ok(());
        };
        if self.ensure_aperture(index, code)? {
            let path = Path {
                start,
                segments: vec![segment],
                sources: vec![source],
            };
            self.object(Shape::Path(path), source, Some(code), [0.0, 0.0]);
        }
        Ok(())
    }

    /// The segment from `start` to `end` in the current interpolation mode.
    fn segment(
        &mut self,
        index: usize,
        start: Point,
        end: Point,
        offset: Option<&CoordinateOffset>,
    ) -> Segment {
        let ccw = match self.interpolation {
            InterpolationMode::Linear => return Segment::Line { to: end },
            InterpolationMode::ClockwiseCircular => false,
            InterpolationMode::CounterclockwiseCircular => true,
        };
        self.feature(SpecFeature::CircularArc);
        let i = offset.and_then(|o| o.x).map_or(0.0, f64::from);
        let j = offset.and_then(|o| o.y).map_or(0.0, f64::from);
        let center = match self.quadrant {
            QuadrantMode::Multi => [start[0] + i, start[1] + j],
            QuadrantMode::Single => {
                self.feature(SpecFeature::SingleQuadrantArc);
                match single_quadrant_center(start, end, i.abs(), j.abs(), ccw) {
                    Some(center) => center,
                    None => {
                        self.issue(index, FlattenIssueKind::InvalidArc);
                        return Segment::Line { to: end };
                    }
                }
            }
        };
        Segment::Arc {
            to: end,
            center,
            ccw,
        }
    }

    fn close_region(&mut self, index: usize) {
        let Some(mut region) = self.region.take() else {
            return;
        };
        region.contours.extend(region.contour.take());
        let mut open = false;
        for contour in &mut region.contours {
            if !same_point(contour.start, contour.end()) {
                open = true;
                contour.segments.push(Segment::Line { to: contour.start });
                contour.sources.push(SourceRange::single(index));
            }
        }
        if open {
            self.issue(index, FlattenIssueKind::OpenContour);
        }
        if region.contours.is_empty() {
            return;
        }
        self.feature(SpecFeature::Region);
        let shape = Shape::Region {
            contours: region.contours,
        };
        let source = SourceRange {
            start: region.start,
            end: index + 1,
        };
        self.object(shape, source, None, [0.0, 0.0]);
    }

    fn finish(mut self, end: usize) -> Result<FlatLayer, FlattenError> {
        if self.region.is_some() {
            self.issue(end, FlattenIssueKind::UnclosedRegion);
            self.close_region(end);
        }
        while self.scopes.len() > 1 {
            // A file may end inside its final SR; the spec closes it there.
            if matches!(self.scope().kind, ScopeKind::Block { .. }) {
                self.issue(end, FlattenIssueKind::UnclosedApertureBlock);
            }
            self.close_scope(end)?;
        }
        self.out.root = self.scopes.pop().expect("root scope").ops;
        Ok(self.out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flatten::ResourceLimits;
    use crate::layer::LayerType;
    use std::io::BufReader;

    const HEADER: &str = "%FSLAX46Y46*%\n%MOMM*%\n%ADD10C,0.5*%\n%ADD11R,1.0X2.0*%\n";

    fn flat(body: &str) -> FlatLayer {
        try_flat(body, &FlattenOptions::default()).unwrap()
    }

    fn try_flat(body: &str, options: &FlattenOptions) -> Result<FlatLayer, FlattenError> {
        let gbr = format!("{HEADER}{body}M02*\n");
        let layer =
            GerberLayerData::from_type(LayerType::Top, BufReader::new(gbr.as_bytes())).unwrap();
        layer.flatten_with(options)
    }

    fn objects(layer: &FlatLayer) -> Vec<&GraphicsObject> {
        layer
            .root
            .iter()
            .filter_map(|op| match op {
                Op::Object(o) => Some(o),
                Op::Instance { .. } => None,
            })
            .collect()
    }

    #[test]
    fn consecutive_draws_merge_into_one_path() {
        let layer = flat("D10*\nX0Y0D02*\nX1000000Y0D01*\nY1000000D01*\nD11*\nX2000000Y0D03*\n");
        let objs = objects(&layer);
        assert_eq!(objs.len(), 2);
        let Shape::Path(path) = &objs[0].shape else {
            panic!("expected a path, got {:?}", objs[0].shape);
        };
        assert_eq!(path.start, [0.0, 0.0]);
        assert_eq!(path.segments.len(), 2);
        // Commands: D10, X0Y0D02, X1Y0D01, Y1D01 — the first draw owns the
        // move that positioned it.
        assert_eq!(
            path.sources,
            [SourceRange { start: 1, end: 3 }, SourceRange::single(3)]
        );
        assert_eq!(objs[0].source, SourceRange { start: 1, end: 4 });
        assert_eq!(path.end(), [1.0, 1.0]);
        assert_eq!(objs[0].aperture, Some(10));

        assert_eq!(objs[1].shape, Shape::Flash);
        assert_eq!(objs[1].transform, Affine2::translate(2.0, 0.0));
        assert_eq!(layer.apertures.len(), 2);
        assert!(layer.issues.is_empty(), "{:?}", layer.issues);
    }

    #[test]
    fn regions_keep_contours_and_exact_arcs() {
        let layer = flat(
            "G75*\nG36*\nX0Y0D02*\nG01*\nX2000000Y0D01*\nG03*\nX0Y0I-1000000J0D01*\n\
             X5000000Y0D02*\nG01*\nX6000000Y0D01*\nX6000000Y1000000D01*\nX5000000Y0D01*\nG37*\n",
        );
        let objs = objects(&layer);
        assert_eq!(objs.len(), 1);
        let Shape::Region { contours } = &objs[0].shape else {
            panic!("expected a region");
        };
        assert_eq!(contours.len(), 2);
        assert_eq!(
            contours[0].segments[1],
            Segment::Arc {
                to: [0.0, 0.0],
                center: [1.0, 0.0],
                ccw: true
            }
        );
        assert_eq!(objs[0].aperture, None);
        assert_eq!(layer.features[&SpecFeature::Region], 1);
    }

    #[test]
    fn open_region_contour_is_closed_and_reported() {
        let layer = flat("G36*\nX0Y0D02*\nG01*\nX1000000Y0D01*\nX1000000Y1000000D01*\nG37*\n");
        let Shape::Region { contours } = &objects(&layer)[0].shape else {
            panic!("expected a region");
        };
        assert_eq!(contours[0].end(), [0.0, 0.0]);
        assert_eq!(layer.issues[0].kind, FlattenIssueKind::OpenContour);
    }

    #[test]
    fn clear_polarity() {
        let layer = flat("D10*\nX0Y0D03*\n%LPC*%\nX0Y0D03*\n%LPD*%\nX0Y0D03*\n");
        let pol: Vec<_> = objects(&layer).iter().map(|o| o.polarity).collect();
        assert_eq!(pol, [Polarity::Dark, Polarity::Clear, Polarity::Dark]);
    }

    #[test]
    fn step_and_repeat_is_one_grid_instance() {
        let layer = flat("%SRX3Y2I5.0J4.0*%\nD10*\nX1000000Y0D03*\n%SR*%\n");
        assert_eq!(layer.root.len(), 1);
        let Op::Instance { transforms, .. } = &layer.root[0] else {
            panic!("expected an instance");
        };
        assert_eq!(transforms.len(), 6);
        let positions: Vec<Point> = layer
            .iter_expanded()
            .map(|e| e.transform.apply([0.0, 0.0]))
            .collect();
        assert_eq!(positions.len(), 6);
        assert_eq!(positions[0], [1.0, 0.0]);
        assert_eq!(positions[2], [11.0, 0.0]);
        assert_eq!(positions[5], [11.0, 4.0]);
    }

    #[test]
    fn aperture_block_flashes_place_and_invert() {
        let layer = flat(
            "%ABD12*%\nD10*\nX0Y0D03*\nX1000000Y0D03*\n%AB*%\nD12*\nX5000000Y5000000D03*\n\
             %LPC*%\nX0Y0D03*\n",
        );
        assert_eq!(layer.definitions.len(), 1);
        let expanded: Vec<_> = layer
            .iter_expanded()
            .map(|e| (e.transform.apply([0.0, 0.0]), e.polarity))
            .collect();
        assert_eq!(
            expanded,
            [
                ([5.0, 5.0], Polarity::Dark),
                ([6.0, 5.0], Polarity::Dark),
                ([0.0, 0.0], Polarity::Clear),
                ([1.0, 0.0], Polarity::Clear),
            ]
        );
    }

    #[test]
    fn instance_limit_fails_layer() {
        let limits = ResourceLimits {
            max_instances: 10,
            ..ResourceLimits::default()
        };
        let options = FlattenOptions::default().limits(limits);
        let err = try_flat("%SRX4Y3I1.0J1.0*%\nD10*\nX0Y0D03*\n%SR*%\n", &options).unwrap_err();
        assert!(matches!(err, FlattenError::ResourceLimit { limit: 10, .. }));
        assert!(try_flat("%SRX5Y2I1.0J1.0*%\nD10*\nX0Y0D03*\n%SR*%\n", &options).is_ok());
    }

    #[test]
    fn problems_become_issues() {
        let layer = flat("D99*\nX0Y0D03*\n%LR45.0*%\nD10*\nX0Y0D03*\n");
        let kinds: Vec<_> = layer.issues.iter().map(|i| &i.kind).collect();
        assert_eq!(
            kinds,
            [
                &FlattenIssueKind::UndefinedAperture(99),
                &FlattenIssueKind::Unsupported("LR (load rotation)"),
            ]
        );
        assert_eq!(objects(&layer).len(), 1);
    }

    #[test]
    fn single_quadrant_picks_matching_centre() {
        // Quarter circle from (1,0) to (0,1) around the origin, CCW.
        assert_eq!(
            single_quadrant_center([1.0, 0.0], [0.0, 1.0], 1.0, 0.0, true),
            Some([0.0, 0.0])
        );
        // Same chord clockwise needs the centre at (1,1).
        assert_eq!(
            single_quadrant_center([1.0, 0.0], [0.0, 1.0], 0.0, 1.0, false),
            Some([1.0, 1.0])
        );
    }

    #[test]
    fn inch_layer_flattens_in_mm() {
        let gbr = "%FSLAX25Y25*%\n%MOIN*%\n%ADD10C,0.1*%\nD10*\nX100000Y0D03*\nM02*\n";
        let layer =
            GerberLayerData::from_type(LayerType::Top, BufReader::new(gbr.as_bytes())).unwrap();
        let flat = layer.flatten().unwrap();
        let o = flat.iter_expanded().next().unwrap();
        assert!((o.transform.tx - 25.4).abs() < 1e-9);
        let gerber_parser::gerber_types::Aperture::Circle(c) = &flat.apertures[&10].aperture else {
            panic!("expected a circle");
        };
        assert!((c.diameter - 2.54).abs() < 1e-9);
    }

    #[test]
    fn kicad_board_flattens_without_issues() {
        let result = crate::board::Board::from_folder(std::path::Path::new("test/mobo")).unwrap();
        let mut flattened = 0;
        for layer in result.board.layers() {
            let crate::layer::LayerData::Gerber(g) = &layer.data else {
                continue;
            };
            let flat = g.flatten().unwrap();
            assert!(flat.issues.is_empty(), "{}: {:?}", layer.name, flat.issues);
            let draws = g.commands.iter().any(|c| {
                matches!(
                    c,
                    Command::FunctionCode(FunctionCode::DCode(DCode::Operation(
                        Operation::Interpolate(..) | Operation::Flash(_)
                    )))
                )
            });
            assert_eq!(
                draws,
                flat.iter_expanded().next().is_some(),
                "{}",
                layer.name
            );
            flattened += 1;
        }
        assert_eq!(flattened, 11);
    }
}
