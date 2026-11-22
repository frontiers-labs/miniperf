use gpui::{Hsla, PathBuilder, Pixels, Point, Window, point, px};

/// Line-with-fill series painter. `points` are `(x in pixels, value)` in
/// ascending x; a `None` value breaks the line so gaps in the recording stay
/// visible instead of being interpolated over.
pub fn paint_area_series(
    top: f32,
    height: f32,
    points: &[(f32, Option<f64>)],
    max: f64,
    color: Hsla,
    window: &mut Window,
) {
    if max <= 0.0 || height <= 0.0 {
        return;
    }
    let baseline = top + height;
    let y_for = |value: f64| top + (1.0 - (value / max).clamp(0.0, 1.0) as f32) * height;

    let mut area: Option<(PathBuilder, f32)> = None;
    let mut stroke: Option<PathBuilder> = None;
    let mut last_x = 0.0f32;
    for (x, value) in points {
        match value {
            Some(value) => {
                let p: Point<Pixels> = point(px(*x), px(y_for(*value)));
                match (&mut area, &mut stroke) {
                    (Some((area, _)), Some(stroke)) => {
                        area.line_to(p);
                        stroke.line_to(p);
                    }
                    _ => {
                        let mut new_area = PathBuilder::fill();
                        new_area.move_to(p);
                        let mut new_stroke = PathBuilder::stroke(px(1.5));
                        new_stroke.move_to(p);
                        area = Some((new_area, *x));
                        stroke = Some(new_stroke);
                    }
                }
                last_x = *x;
            }
            None => flush(&mut area, &mut stroke, last_x, baseline, color, window),
        }
    }
    flush(&mut area, &mut stroke, last_x, baseline, color, window);
}

fn flush(
    area: &mut Option<(PathBuilder, f32)>,
    stroke: &mut Option<PathBuilder>,
    end_x: f32,
    baseline: f32,
    color: Hsla,
    window: &mut Window,
) {
    if let Some((mut path, start_x)) = area.take() {
        path.line_to(point(px(end_x), px(baseline)));
        path.line_to(point(px(start_x), px(baseline)));
        path.close();
        if let Ok(path) = path.build() {
            window.paint_path(path, color.opacity(0.1));
        }
    }
    if let Some(path) = stroke.take()
        && let Ok(path) = path.build()
    {
        window.paint_path(path, color);
    }
}
