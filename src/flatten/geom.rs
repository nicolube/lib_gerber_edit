//! Polygon primitives and boolean helpers shared by the flattener.
//!
//! Contours are closed implicitly (the last point is not repeated). Outer
//! contours are counter-clockwise, holes clockwise, as i_overlay emits them.
//! With that orientation every shape has winding 0 or 1, so a single NonZero
//! overlay of any number of shapes is their union.

use i_overlay::core::fill_rule::FillRule;
use i_overlay::core::overlay_rule::OverlayRule;
use i_overlay::float::single::SingleFloatOverlay;
use i_overlay::i_shape::float::area::Area;

pub type Point = [f64; 2];
pub type Contour = Vec<Point>;
/// Shapes as i_overlay returns them: each entry is an outer contour followed
/// by its holes.
pub type Shapes = Vec<Vec<Contour>>;

/// Fewest segments used for a full circle, so tiny pads stay round-ish.
const MIN_CIRCLE_SEGMENTS: usize = 8;
/// Most segments used for a full circle, so huge arcs stay bounded.
const MAX_CIRCLE_SEGMENTS: usize = 4096;

/// Segments needed so a full circle of `radius` deviates from the true arc
/// by at most `tolerance`.
fn circle_segments(radius: f64, tolerance: f64) -> usize {
    if radius <= tolerance {
        return MIN_CIRCLE_SEGMENTS;
    }
    let step = 2.0 * (1.0 - tolerance / radius).acos();
    ((std::f64::consts::TAU / step).ceil() as usize).clamp(MIN_CIRCLE_SEGMENTS, MAX_CIRCLE_SEGMENTS)
}

/// Counter-clockwise circle approximation.
pub fn circle(center: Point, diameter: f64, tolerance: f64) -> Contour {
    let n = circle_segments(diameter / 2.0, tolerance);
    regular_polygon(center, diameter, n as u32, 0.0)
}

/// Counter-clockwise ring; a plain disc when `inner` is not positive.
pub fn annulus(center: Point, outer: f64, inner: f64, tolerance: f64) -> Shapes {
    let mut shape = vec![circle(center, outer, tolerance)];
    if inner > 0.0 {
        let mut hole = circle(center, inner, tolerance);
        hole.reverse();
        shape.push(hole);
    }
    vec![shape]
}

/// Counter-clockwise axis-aligned rectangle centred on `center`.
pub fn rect(center: Point, width: f64, height: f64) -> Contour {
    let (hw, hh) = (width / 2.0, height / 2.0);
    let [cx, cy] = center;
    vec![
        [cx - hw, cy - hh],
        [cx + hw, cy - hh],
        [cx + hw, cy + hh],
        [cx - hw, cy + hh],
    ]
}

/// Counter-clockwise stadium: a rectangle with semicircular ends on its
/// shorter sides. Degenerates to a circle when both sides are equal.
pub fn obround(center: Point, width: f64, height: f64, tolerance: f64) -> Contour {
    let r = width.min(height) / 2.0;
    // Half the distance between the two end-cap centres, along the long axis.
    let half = (width - height).abs() / 2.0;
    let n = circle_segments(r, tolerance).div_ceil(2).max(2);
    let cap = |cap_center: Point, start: f64| {
        (0..=n).map(move |i| {
            let a = start + std::f64::consts::PI * i as f64 / n as f64;
            [cap_center[0] + r * a.cos(), cap_center[1] + r * a.sin()]
        })
    };
    let [cx, cy] = center;
    let (first, second, start) = if width >= height {
        (
            [cx + half, cy],
            [cx - half, cy],
            -std::f64::consts::FRAC_PI_2,
        )
    } else {
        ([cx, cy + half], [cx, cy - half], 0.0)
    };
    let mut contour: Contour = cap(first, start)
        .chain(cap(second, start + std::f64::consts::PI))
        .collect();
    contour.dedup();
    contour
}

/// Counter-clockwise regular polygon inscribed in a circle of `diameter`,
/// first vertex at `rotation_deg` from the positive X axis.
pub fn regular_polygon(center: Point, diameter: f64, vertices: u32, rotation_deg: f64) -> Contour {
    let r = diameter / 2.0;
    let start = rotation_deg.to_radians();
    (0..vertices)
        .map(|i| {
            let a = start + std::f64::consts::TAU * i as f64 / vertices as f64;
            [center[0] + r * a.cos(), center[1] + r * a.sin()]
        })
        .collect()
}

/// A hole-free outline as shapes, normalised to counter-clockwise.
pub fn simple(mut contour: Contour) -> Shapes {
    if contour.area() < 0.0 {
        contour.reverse();
    }
    vec![vec![contour]]
}

/// Rotates every point of `shapes` about the origin by `deg` degrees
/// counter-clockwise.
pub fn rotate(mut shapes: Shapes, deg: f64) -> Shapes {
    if deg != 0.0 {
        let (s, c) = deg.to_radians().sin_cos();
        for p in shapes.iter_mut().flatten().flatten() {
            *p = [p[0] * c - p[1] * s, p[0] * s + p[1] * c];
        }
    }
    shapes
}

/// Union of `a` and `b`, which must use the module's orientation.
pub fn union(a: Shapes, b: Shapes) -> Shapes {
    a.overlay(&b, OverlayRule::Union, FillRule::NonZero)
}

/// `subject` minus `clip`.
pub fn difference(subject: Shapes, clip: &Shapes) -> Shapes {
    if subject.is_empty() || clip.is_empty() {
        return subject;
    }
    subject.overlay(clip, OverlayRule::Difference, FillRule::NonZero)
}

/// Centre of a G74 (single-quadrant) arc: `i`/`j` are unsigned, so the
/// centre is the candidate equidistant from both ends with a sweep of at
/// most 90°.
pub(crate) fn single_quadrant_center(
    start: Point,
    end: Point,
    i: f64,
    j: f64,
    ccw: bool,
) -> Option<Point> {
    use std::f64::consts::{FRAC_PI_2, TAU};
    [(i, j), (-i, j), (i, -j), (-i, -j)]
        .into_iter()
        .map(|(di, dj)| [start[0] + di, start[1] + dj])
        .filter_map(|c| {
            let r0 = (start[0] - c[0]).hypot(start[1] - c[1]);
            let r1 = (end[0] - c[0]).hypot(end[1] - c[1]);
            let a0 = (start[1] - c[1]).atan2(start[0] - c[0]);
            let a1 = (end[1] - c[1]).atan2(end[0] - c[0]);
            let sweep = if ccw { a1 - a0 } else { a0 - a1 }.rem_euclid(TAU);
            (sweep <= FRAC_PI_2 + 1e-6).then_some((c, (r0 - r1).abs()))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(c, _)| c)
}

/// Centre of the arc of `radius` from `start` to `end`. A positive radius
/// gives the arc of at most 180°, a negative one the longer arc. A chord
/// longer than the diameter is treated as a half circle.
pub(crate) fn arc_center_from_radius(start: Point, end: Point, radius: f64, ccw: bool) -> Point {
    let mid = [(start[0] + end[0]) / 2.0, (start[1] + end[1]) / 2.0];
    let (dx, dy) = (end[0] - start[0], end[1] - start[1]);
    let chord = dx.hypot(dy);
    if chord == 0.0 {
        return mid;
    }
    let half = chord / 2.0;
    let h = (radius * radius - half * half).max(0.0).sqrt();
    // Left of the chord direction is the centre of a short CCW arc.
    let left = if ccw == (radius >= 0.0) { 1.0 } else { -1.0 };
    [
        mid[0] - dy / chord * h * left,
        mid[1] + dx / chord * h * left,
    ]
}

/// Bounding box of the arc from `start` to `end` around `center`; a full
/// circle when the ends meet.
pub(crate) fn arc_bounds(start: Point, end: Point, center: Point, ccw: bool) -> (Point, Point) {
    use std::f64::consts::{FRAC_PI_2, TAU};
    let r = (start[0] - center[0]).hypot(start[1] - center[1]);
    let a0 = (start[1] - center[1]).atan2(start[0] - center[0]);
    let a1 = (end[1] - center[1]).atan2(end[0] - center[0]);
    let mut sweep = if ccw { a1 - a0 } else { a0 - a1 }.rem_euclid(TAU);
    if sweep == 0.0 {
        sweep = TAU;
    }
    let mut lo = [start[0].min(end[0]), start[1].min(end[1])];
    let mut hi = [start[0].max(end[0]), start[1].max(end[1])];
    for k in 0..4 {
        let a = k as f64 * FRAC_PI_2;
        let from_start = if ccw { a - a0 } else { a0 - a }.rem_euclid(TAU);
        if from_start <= sweep {
            let p = [center[0] + r * a.cos(), center[1] + r * a.sin()];
            lo = [lo[0].min(p[0]), lo[1].min(p[1])];
            hi = [hi[0].max(p[0]), hi[1].max(p[1])];
        }
    }
    (lo, hi)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    #[test]
    fn circle_meets_tolerance() {
        let tol = 0.001;
        let c = circle([0.0, 0.0], 2.0, tol);
        // Sagitta of one segment stays within the tolerance.
        let step = 2.0 * PI / c.len() as f64;
        assert!(1.0 - (step / 2.0).cos() <= tol + 1e-12);
        assert!(c.area() > 0.0);
    }

    #[test]
    fn obround_area() {
        let tol = 0.0001;
        let expected = 2.0 * 1.0 + PI * 0.25;
        assert!((obround([0.0, 0.0], 3.0, 1.0, tol).area() - expected).abs() < 1e-3);
        assert!((obround([0.0, 0.0], 1.0, 3.0, tol).area() - expected).abs() < 1e-3);
    }

    #[test]
    fn difference_makes_hole() {
        let d = difference(
            simple(rect([0.0, 0.0], 4.0, 4.0)),
            &simple(rect([0.0, 0.0], 2.0, 2.0)),
        );
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].len(), 2);
        assert!(d[0][0].area() > 0.0 && d[0][1].area() < 0.0);
        assert!((d.area() - 12.0).abs() < 1e-9);
    }

    #[test]
    fn union_fills_holes_covered_by_other_shapes() {
        let ring = annulus([0.0, 0.0], 2.0, 1.0, 0.0001);
        // A square inside the hole stays a separate island.
        let u = union(ring, simple(rect([0.0, 0.0], 0.5, 0.5)));
        assert_eq!(u.len(), 2);
        let plug = union(
            annulus([0.0, 0.0], 2.0, 1.0, 0.0001),
            simple(rect([0.0, 0.0], 1.2, 1.2)),
        );
        assert_eq!(plug[0].len(), 1, "covered hole disappears");
    }

    #[test]
    fn simple_normalises_orientation() {
        let mut cw = rect([1.0, 0.0], 2.0, 2.0);
        cw.reverse();
        let u = union(simple(rect([0.0, 0.0], 2.0, 2.0)), simple(cw));
        assert!((u.area() - 6.0).abs() < 1e-9);
    }

    #[test]
    fn arc_center_from_radius_picks_side() {
        // Quarter arc from (1,0) to (0,1): CCW short arc is centred at origin.
        let c = arc_center_from_radius([1.0, 0.0], [0.0, 1.0], 1.0, true);
        assert!(c[0].abs() < 1e-12 && c[1].abs() < 1e-12, "{c:?}");
        let c = arc_center_from_radius([1.0, 0.0], [0.0, 1.0], 1.0, false);
        assert!(
            (c[0] - 1.0).abs() < 1e-12 && (c[1] - 1.0).abs() < 1e-12,
            "{c:?}"
        );
        let c = arc_center_from_radius([1.0, 0.0], [0.0, 1.0], -1.0, false);
        assert!(c[0].abs() < 1e-12 && c[1].abs() < 1e-12, "{c:?}");
    }

    #[test]
    fn arc_bounds_includes_extremes_in_sweep() {
        let near = |(lo, hi): (Point, Point), want: [f64; 4]| {
            let got = [lo[0], lo[1], hi[0], hi[1]];
            assert!(
                got.iter().zip(want).all(|(g, w)| (g - w).abs() < 1e-12),
                "{got:?}"
            );
        };
        near(
            arc_bounds([1.0, 0.0], [-1.0, 0.0], [0.0, 0.0], true),
            [-1.0, 0.0, 1.0, 1.0],
        );
        near(
            arc_bounds([1.0, 0.0], [-1.0, 0.0], [0.0, 0.0], false),
            [-1.0, -1.0, 1.0, 0.0],
        );
        near(
            arc_bounds([1.0, 0.0], [1.0, 0.0], [0.0, 0.0], true),
            [-1.0, -1.0, 1.0, 1.0],
        );
    }
}
