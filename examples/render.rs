//! Renders a board to a PNG from its flattened layers.
//!
//! ```text
//! cargo run --example render -- <gerber-folder> <out.png> [px_per_mm]
//! cargo run --example render -- test/mobo /tmp/mobo.png 20
//! ```
//!
//! Every layer is flattened (`flatten()`), walked with `iter_expanded()` and
//! drawn with its own colour; clear polarity erases within its layer. Strokes
//! with non-circular apertures are drawn as the aperture stamped at each
//! vertex joined by a round stroke of the aperture's smaller side, which is
//! close enough for a preview.

use lib_gerber_edit::LayerCorners;
use lib_gerber_edit::board::Board;
use lib_gerber_edit::flatten::{Affine2, ExpandedObject, FlatLayer, Point, Polarity, Shape};
use lib_gerber_edit::gerber_types::Aperture;
use lib_gerber_edit::layer::{LayerData, LayerType};
use std::path::Path;
use tiny_skia::{
    BlendMode, Color, FillRule, LineCap, LineJoin, Paint, PathBuilder, Pixmap, PixmapPaint, Stroke,
    Transform,
};

/// Chord tolerance for arcs in mm.
const TOLERANCE: f64 = 0.005;
/// Largest image side in pixels.
const MAX_PIXELS: f64 = 8000.0;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let [_, input, output, rest @ ..] = args.as_slice() else {
        eprintln!("usage: render <gerber-folder> <out.png> [px_per_mm]");
        std::process::exit(2);
    };
    let px_per_mm: f64 = rest.first().map_or(Ok(20.0), |v| v.parse())?;

    let loaded = Board::from_folder(Path::new(input))?;
    for (file, err) in &loaded.errors {
        eprintln!("skipped {file}: {err}");
    }
    let mut layers: Vec<(LayerType, String, FlatLayer)> = Vec::new();
    for layer in loaded.board.layers() {
        let flat = match &layer.data {
            LayerData::Gerber(g) => g.flatten()?,
            LayerData::Excellon(e) => e.flatten()?,
            LayerData::Info(_) => continue,
        };
        for issue in &flat.issues {
            eprintln!("{}: {issue}", layer.name);
        }
        layers.push((layer.ty, layer.name.clone(), flat));
    }
    // Back to front: bottom side first, outline and drills on top.
    layers.sort_by_key(|(ty, _, _)| draw_order(ty));

    // Bounding box of the board, aperture sizes included.
    let (lo, hi) = loaded.board.get_corners();
    if lo.x > hi.x {
        return Err("nothing to draw".into());
    }
    let margin = 1.0;
    let (w, h) = (hi.x - lo.x + 2.0 * margin, hi.y - lo.y + 2.0 * margin);
    let scale = px_per_mm.min(MAX_PIXELS / w.max(h));
    // Gerber Y points up, image Y points down.
    let view = Transform::from_row(
        scale as f32,
        0.0,
        0.0,
        -scale as f32,
        ((margin - lo.x) * scale) as f32,
        ((hi.y + margin) * scale) as f32,
    );

    let (pw, ph) = ((w * scale).ceil() as u32, (h * scale).ceil() as u32);
    let mut image = Pixmap::new(pw, ph).ok_or("image too large")?;
    image.fill(Color::from_rgba8(20, 24, 28, 255));
    for (ty, name, flat) in &layers {
        let mut canvas = Pixmap::new(pw, ph).ok_or("image too large")?;
        let [r, g, b] = layer_color(ty);
        let mut dark = Paint::default();
        dark.set_color_rgba8(r, g, b, 255);
        dark.anti_alias = true;
        let clear = Paint {
            blend_mode: BlendMode::Clear,
            ..dark.clone()
        };
        for object in flat.iter_expanded() {
            let paint = match object.polarity {
                Polarity::Clear => &clear,
                Polarity::Dark => &dark,
            };
            draw(&mut canvas, flat, &object, paint, view);
        }
        let layer_paint = PixmapPaint {
            opacity: 0.75,
            ..PixmapPaint::default()
        };
        image.draw_pixmap(
            0,
            0,
            canvas.as_ref(),
            &layer_paint,
            Transform::identity(),
            None,
        );
        println!("drew {name}");
    }
    image.save_png(output)?;
    println!("wrote {pw}x{ph} px to {output}");
    Ok(())
}

fn draw(
    canvas: &mut Pixmap,
    flat: &FlatLayer,
    object: &ExpandedObject,
    paint: &Paint,
    view: Transform,
) {
    let image = object
        .object
        .aperture
        .and_then(|code| flat.apertures.get(&code));
    let xf = object.transform;
    let place =
        |points: Vec<Point>| -> Vec<Point> { points.iter().map(|p| xf.apply(*p)).collect() };
    match &object.object.shape {
        Shape::Flash => {
            if let Some(image) = image {
                fill_shapes(canvas, &image.shapes, &xf, paint, view);
            }
        }
        Shape::Path(p) => {
            let Some(image) = image else { return };
            let points = place(p.to_polyline(TOLERANCE));
            let (width, round) = match &image.aperture {
                Aperture::Circle(c) => (c.diameter, true),
                Aperture::Rectangle(r) | Aperture::Obround(r) => (r.x.min(r.y), false),
                Aperture::Polygon(p) => (p.diameter, false),
                Aperture::Macro(..) => (0.0, false),
            };
            if !round {
                // Stamp the aperture at every vertex, then join them.
                for p in &points {
                    fill_shapes(
                        canvas,
                        &image.shapes,
                        &Affine2::translate(p[0], p[1]),
                        paint,
                        view,
                    );
                }
            }
            if let Some(p) = path(points, false) {
                let stroke = Stroke {
                    // Zero-width apertures (outlines) still show up.
                    width: width.max(0.05) as f32,
                    line_cap: LineCap::Round,
                    line_join: LineJoin::Round,
                    ..Stroke::default()
                };
                canvas.stroke_path(&p, paint, &stroke, view, None);
            }
        }
        Shape::Region { contours } => {
            // Every contour adds area, so each is filled on its own.
            for contour in contours {
                if let Some(p) = path(place(contour.to_polyline(TOLERANCE)), true) {
                    canvas.fill_path(&p, paint, FillRule::Winding, view, None);
                }
            }
        }
        _ => {}
    }
}

/// Fills aperture shapes (outer contours CCW, holes CW) placed by `xf`.
fn fill_shapes(
    canvas: &mut Pixmap,
    shapes: &[Vec<Vec<Point>>],
    xf: &Affine2,
    paint: &Paint,
    view: Transform,
) {
    for shape in shapes {
        let mut pb = PathBuilder::new();
        for contour in shape {
            add_contour(&mut pb, contour.iter().map(|p| xf.apply(*p)), true);
        }
        if let Some(p) = pb.finish() {
            canvas.fill_path(&p, paint, FillRule::Winding, view, None);
        }
    }
}

/// A path through `points`, closed when `close`.
fn path(points: Vec<Point>, close: bool) -> Option<tiny_skia::Path> {
    let mut pb = PathBuilder::new();
    add_contour(&mut pb, points, close);
    pb.finish()
}

fn add_contour(pb: &mut PathBuilder, points: impl IntoIterator<Item = Point>, close: bool) {
    let mut points = points.into_iter();
    let Some(first) = points.next() else { return };
    pb.move_to(first[0] as f32, first[1] as f32);
    for p in points {
        pb.line_to(p[0] as f32, p[1] as f32);
    }
    if close {
        pb.close();
    }
}

fn draw_order(ty: &LayerType) -> u8 {
    match ty {
        LayerType::SilkScreenBottom | LayerType::MaskBottom | LayerType::PasteBottom => 0,
        LayerType::Bottom => 1,
        LayerType::Inner(_) => 2,
        LayerType::Top => 3,
        LayerType::MaskTop | LayerType::PasteTop => 4,
        LayerType::SilkScreenTop => 5,
        LayerType::Drill => 7,
        LayerType::Dimensions | LayerType::Milling => 8,
        _ => 6,
    }
}

fn layer_color(ty: &LayerType) -> [u8; 3] {
    match ty {
        LayerType::Top => [200, 80, 60],
        LayerType::Bottom => [60, 110, 200],
        LayerType::Inner(_) => [170, 140, 60],
        LayerType::SilkScreenTop | LayerType::SilkScreenBottom => [235, 235, 235],
        LayerType::MaskTop | LayerType::MaskBottom => [40, 150, 80],
        LayerType::PasteTop | LayerType::PasteBottom => [150, 150, 160],
        LayerType::Drill => [10, 10, 10],
        LayerType::Dimensions | LayerType::Milling => [240, 200, 40],
        _ => [180, 120, 200],
    }
}
