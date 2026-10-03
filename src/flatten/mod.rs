//! Flattening of layer data into plain geometry.
//!
//! This module turns apertures, aperture macros and (later) whole layers into
//! polygons in millimetres, so renderers and hit-testers do not need to know
//! Gerber semantics.

mod aperture;
mod expr;
mod geom;
mod macro_eval;

pub use aperture::aperture_shapes;
pub use expr::ExprError;
pub use geom::{Contour, Point, Shapes};
pub use macro_eval::{MacroError, eval_macro};
