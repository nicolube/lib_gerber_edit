//! The flattened graphics stream of a layer (`FlatLayer`).
//!
//! A `FlatLayer` is the layer's image as plain geometry in millimetres, in
//! the order the objects are composited. Step-and-repeat blocks and aperture
//! blocks are kept once as [`Definition`]s and placed by [`Op::Instance`];
//! [`FlatLayer::iter_expanded`] walks the fully expanded stream for simple
//! consumers.

use std::collections::BTreeMap;

use gerber_parser::gerber_types::Aperture;

use super::geom::{Point, Shapes};
use super::macro_eval::MacroError;

/// 2D affine transform `p' = [a c; b d] p + [tx; ty]`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Affine2 {
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
    pub tx: f64,
    pub ty: f64,
}

impl Affine2 {
    pub const IDENTITY: Affine2 = Affine2 {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        tx: 0.0,
        ty: 0.0,
    };

    pub fn translate(tx: f64, ty: f64) -> Self {
        Affine2 {
            tx,
            ty,
            ..Self::IDENTITY
        }
    }

    pub fn apply(&self, p: Point) -> Point {
        [
            self.a * p[0] + self.c * p[1] + self.tx,
            self.b * p[0] + self.d * p[1] + self.ty,
        ]
    }

    /// `self ∘ inner`: applies `inner` first, then `self`.
    pub fn then_inner(&self, inner: &Affine2) -> Self {
        Affine2 {
            a: self.a * inner.a + self.c * inner.b,
            b: self.b * inner.a + self.d * inner.b,
            c: self.a * inner.c + self.c * inner.d,
            d: self.b * inner.c + self.d * inner.d,
            tx: self.a * inner.tx + self.c * inner.ty + self.tx,
            ty: self.b * inner.tx + self.d * inner.ty + self.ty,
        }
    }
}

impl Default for Affine2 {
    fn default() -> Self {
        Self::IDENTITY
    }
}

/// Dark adds to the image, clear erases what is below.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Polarity {
    Dark,
    Clear,
}

impl Polarity {
    /// The opposite polarity when `invert` is set.
    pub fn inverted(self, invert: bool) -> Self {
        match (self, invert) {
            (p, false) => p,
            (Polarity::Dark, true) => Polarity::Clear,
            (Polarity::Clear, true) => Polarity::Dark,
        }
    }
}

impl From<gerber_parser::gerber_types::Polarity> for Polarity {
    fn from(p: gerber_parser::gerber_types::Polarity) -> Self {
        match p {
            gerber_parser::gerber_types::Polarity::Dark => Polarity::Dark,
            gerber_parser::gerber_types::Polarity::Clear => Polarity::Clear,
        }
    }
}

/// Indices into the layer's command list that produced an object
/// (`start..end`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceRange {
    pub start: usize,
    pub end: usize,
}

impl SourceRange {
    pub fn single(index: usize) -> Self {
        SourceRange {
            start: index,
            end: index + 1,
        }
    }
}

/// One piece of a path, starting where the previous one ended.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum Segment {
    Line {
        to: Point,
    },
    /// Circular arc around `center`; a full circle when `to` equals the start.
    Arc {
        to: Point,
        center: Point,
        ccw: bool,
    },
}

impl Segment {
    pub fn end(&self) -> Point {
        match self {
            Segment::Line { to } | Segment::Arc { to, .. } => *to,
        }
    }
}

/// A connected run of segments from `start`.
#[derive(Debug, Clone, PartialEq)]
pub struct Path {
    pub start: Point,
    pub segments: Vec<Segment>,
}

impl Path {
    pub fn end(&self) -> Point {
        self.segments.last().map_or(self.start, Segment::end)
    }
}

/// Geometry of one graphics object, before its `transform`.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Shape {
    /// The image of `GraphicsObject::aperture`, see [`FlatLayer::apertures`].
    Flash,
    /// The aperture swept along the path (a stroke).
    Path(Path),
    /// A filled region; its closed contours are additive (each contour's
    /// own winding is irrelevant).
    Region { contours: Vec<Path> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct GraphicsObject {
    pub shape: Shape,
    pub polarity: Polarity,
    pub source: SourceRange,
    /// D-code of the aperture used (flashes and strokes).
    pub aperture: Option<i32>,
    /// Placement in the coordinates of the enclosing stream; identity for
    /// strokes and regions, the flash position for flashes.
    pub transform: Affine2,
}

/// Index into [`FlatLayer::definitions`].
pub type DefId = usize;

/// Placements of an instance.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum TransformSet {
    /// Step and repeat: cell `(i, j)` is offset by `(i·dx, j·dy)` from
    /// `origin`, X varying fastest.
    Grid {
        nx: u32,
        ny: u32,
        dx: f64,
        dy: f64,
        origin: Point,
    },
    List(Vec<Affine2>),
}

impl TransformSet {
    pub fn len(&self) -> usize {
        match self {
            TransformSet::Grid { nx, ny, .. } => *nx as usize * *ny as usize,
            TransformSet::List(list) => list.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Transform of cell `index` (`index < len()`).
    pub fn get(&self, index: usize) -> Affine2 {
        match self {
            TransformSet::Grid {
                nx, dx, dy, origin, ..
            } => {
                let (i, j) = (index % *nx as usize, index / *nx as usize);
                Affine2::translate(origin[0] + i as f64 * dx, origin[1] + j as f64 * dy)
            }
            TransformSet::List(list) => list[index],
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Op {
    Object(GraphicsObject),
    /// The definition's stream, placed once per transform in order. Each
    /// copy is composited into the enclosing stream like inline objects;
    /// `invert_polarity` swaps dark and clear (an aperture block flashed
    /// under clear polarity).
    Instance {
        def: DefId,
        transforms: TransformSet,
        invert_polarity: bool,
        source: SourceRange,
    },
}

/// A block of ops placed by [`Op::Instance`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Definition {
    pub ops: Vec<Op>,
}

/// Polygon image of one aperture in its local frame.
#[derive(Debug, Clone, PartialEq)]
pub struct ApertureImage {
    pub aperture: Aperture,
    /// Outer contours counter-clockwise, holes clockwise; empty when the
    /// aperture could not be evaluated (see [`FlatLayer::issues`]).
    pub shapes: Shapes,
}

/// Spec features found while flattening, for diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum SpecFeature {
    MacroAperture,
    Region,
    CircularArc,
    SingleQuadrantArc,
    ExcellonRoute,
    ExcellonSlot,
    ClearPolarity,
    StepAndRepeat,
    ApertureBlock,
    ImageNegative,
}

/// A problem with one object; the rest of the layer is still flattened.
#[derive(Debug, Clone, PartialEq)]
pub struct FlattenIssue {
    /// Command index the issue refers to.
    pub source: usize,
    pub kind: FlattenIssueKind,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum FlattenIssueKind {
    #[error("D{0} is used but not defined")]
    UndefinedAperture(i32),
    #[error("draw or flash before any aperture was selected")]
    NoAperture,
    #[error(transparent)]
    Aperture(#[from] MacroError),
    #[error("no single-quadrant arc centre fits the offsets")]
    InvalidArc,
    #[error("region contour is not closed")]
    OpenContour,
    #[error("flash inside a region is ignored")]
    FlashInRegion,
    #[error("region (G36) is not closed")]
    UnclosedRegion,
    #[error("aperture block (AB) is not closed")]
    UnclosedApertureBlock,
    #[error("unparsed command: {0}")]
    Unparsed(String),
    #[error("{0} is not supported yet and was ignored")]
    Unsupported(&'static str),
}

impl std::fmt::Display for FlattenIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "command #{}: {}", self.source, self.kind)
    }
}

/// Structural limits that stop hostile files from exhausting memory.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ResourceLimits {
    /// Placed instance cells, summed over all nesting levels.
    pub max_instances: usize,
    /// Aperture-block nesting depth.
    pub max_block_depth: usize,
    /// Vertices across all aperture images of the layer.
    pub max_aperture_points: usize,
}

impl ResourceLimits {
    pub fn max_instances(mut self, value: usize) -> Self {
        self.max_instances = value;
        self
    }

    pub fn max_block_depth(mut self, value: usize) -> Self {
        self.max_block_depth = value;
        self
    }

    pub fn max_aperture_points(mut self, value: usize) -> Self {
        self.max_aperture_points = value;
        self
    }
}

impl Default for ResourceLimits {
    fn default() -> Self {
        ResourceLimits {
            max_instances: 1_000_000,
            max_block_depth: 64,
            max_aperture_points: 10_000_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct FlattenOptions {
    pub limits: ResourceLimits,
    /// Maximum chord deviation (mm) where arcs become polygons.
    pub tolerance_mm: f64,
}

impl Default for FlattenOptions {
    fn default() -> Self {
        FlattenOptions {
            limits: ResourceLimits::default(),
            tolerance_mm: 0.002,
        }
    }
}

impl FlattenOptions {
    pub fn limits(mut self, limits: ResourceLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn tolerance_mm(mut self, tolerance_mm: f64) -> Self {
        self.tolerance_mm = tolerance_mm;
        self
    }
}

/// Why a layer could not be flattened at all.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum FlattenError {
    #[error("{what} exceeds the limit of {limit}")]
    ResourceLimit { what: &'static str, limit: usize },
}

/// A layer's image as ordered plain geometry in millimetres.
#[derive(Debug, Clone, PartialEq, Default)]
#[non_exhaustive]
pub struct FlatLayer {
    /// Top-level stream, in compositing order.
    pub root: Vec<Op>,
    pub definitions: Vec<Definition>,
    /// Images of the apertures flashed in this layer, keyed by D-code.
    pub apertures: BTreeMap<i32, ApertureImage>,
    /// The image is negative (`%IPNEG*%`): objects are drawn into a dark
    /// background.
    pub negative: bool,
    /// How often each feature occurs (per command, aperture or block that
    /// uses it).
    pub features: BTreeMap<SpecFeature, usize>,
    pub issues: Vec<FlattenIssue>,
}

/// Appends `op` to `ops`. A stroke continuing the previous stroke (same
/// aperture and polarity, starting at its end) extends it instead.
pub(super) fn push_op(ops: &mut Vec<Op>, op: Op) {
    if let Op::Object(GraphicsObject {
        shape: Shape::Path(next),
        polarity,
        source,
        aperture,
        ..
    }) = &op
        && let Some(Op::Object(GraphicsObject {
            shape: Shape::Path(prev),
            polarity: prev_polarity,
            source: prev_source,
            aperture: prev_aperture,
            ..
        })) = ops.last_mut()
        && prev_aperture == aperture
        && prev_polarity == polarity
        && same_point(prev.end(), next.start)
    {
        prev.segments.extend_from_slice(&next.segments);
        prev_source.end = source.end;
        return;
    }
    ops.push(op);
}

/// Two points closer than this (mm) are the same point.
const SAME_POINT: f64 = 1e-6;

pub(super) fn same_point(a: Point, b: Point) -> bool {
    (a[0] - b[0]).abs() < SAME_POINT && (a[1] - b[1]).abs() < SAME_POINT
}

/// An object of the expanded stream, with its final placement.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExpandedObject<'a> {
    pub object: &'a GraphicsObject,
    /// Instance placements composed with `object.transform`.
    pub transform: Affine2,
    /// Polarity after instance inversion.
    pub polarity: Polarity,
}

impl FlatLayer {
    /// Every object in compositing order with instances expanded.
    pub fn iter_expanded(&self) -> ExpandedIter<'_> {
        ExpandedIter {
            layer: self,
            stack: vec![Frame::Ops {
                ops: &self.root,
                pos: 0,
                transform: Affine2::IDENTITY,
                invert: false,
            }],
        }
    }
}

enum Frame<'a> {
    Ops {
        ops: &'a [Op],
        pos: usize,
        transform: Affine2,
        invert: bool,
    },
    Cells {
        ops: &'a [Op],
        set: &'a TransformSet,
        next: usize,
        transform: Affine2,
        invert: bool,
    },
}

/// Iterator returned by [`FlatLayer::iter_expanded`].
pub struct ExpandedIter<'a> {
    layer: &'a FlatLayer,
    stack: Vec<Frame<'a>>,
}

impl<'a> Iterator for ExpandedIter<'a> {
    type Item = ExpandedObject<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let push = match self.stack.last_mut()? {
                Frame::Ops {
                    ops,
                    pos,
                    transform,
                    invert,
                } => {
                    let Some(op) = ops.get(*pos) else {
                        self.stack.pop();
                        continue;
                    };
                    *pos += 1;
                    match op {
                        Op::Object(object) => {
                            return Some(ExpandedObject {
                                object,
                                transform: transform.then_inner(&object.transform),
                                polarity: object.polarity.inverted(*invert),
                            });
                        }
                        Op::Instance {
                            def,
                            transforms,
                            invert_polarity,
                            ..
                        } => Frame::Cells {
                            ops: &self.layer.definitions[*def].ops,
                            set: transforms,
                            next: 0,
                            transform: *transform,
                            invert: *invert != *invert_polarity,
                        },
                    }
                }
                Frame::Cells {
                    ops,
                    set,
                    next,
                    transform,
                    invert,
                } => {
                    if *next >= set.len() {
                        self.stack.pop();
                        continue;
                    }
                    let cell = set.get(*next);
                    *next += 1;
                    Frame::Ops {
                        ops,
                        pos: 0,
                        transform: transform.then_inner(&cell),
                        invert: *invert,
                    }
                }
            };
            self.stack.push(push);
        }
    }
}
