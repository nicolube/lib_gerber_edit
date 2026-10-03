//! Flattening of layer data into plain geometry.
//!
//! [`GerberLayerData::flatten`](crate::gerber::GerberLayerData::flatten)
//! turns a layer into a [`FlatLayer`]: strokes, flashes and regions in
//! millimetres, in compositing order, so renderers and hit-testers do not
//! need to know Gerber semantics. Aperture images (including macros) are
//! evaluated once per aperture into polygons.

mod aperture;
mod expr;
pub(crate) mod geom;
mod gerber;
mod layer;
mod macro_eval;

pub use aperture::aperture_shapes;
pub use expr::ExprError;
pub use geom::{Contour, Point, Shapes};
pub use layer::{
    Affine2, ApertureImage, DefId, Definition, ExpandedIter, ExpandedObject, FlatLayer,
    FlattenError, FlattenIssue, FlattenIssueKind, FlattenOptions, GraphicsObject, Op, Path,
    Polarity, ResourceLimits, Segment, Shape, SourceRange, SpecFeature, TransformSet,
};
pub use macro_eval::{MacroError, eval_macro};
