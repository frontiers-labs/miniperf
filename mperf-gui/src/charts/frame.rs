use gpui::{App, BorderStyle, Bounds, Pixels, Window, fill, outline, point, px, size};

use super::text::shape_label;
use crate::ui::Theme;

/// The plot area inside a canvas: a left gutter for row labels and a small
/// right pad, with time mapped linearly across the remaining width.
#[derive(Clone, Copy)]
pub struct PlotFrame {
    pub bounds: Bounds<Pixels>,
    pub gutter: f32,
    pub pad_right: f32,
}

impl PlotFrame {
    pub fn new(bounds: Bounds<Pixels>, gutter: f32) -> Self {
        Self {
            bounds,
            gutter,
            pad_right: 4.0,
        }
    }

    pub fn left(&self) -> f32 {
        f32::from(self.bounds.left()) + self.gutter
    }

    pub fn width(&self) -> f32 {
        (f32::from(self.bounds.size.width) - self.gutter - self.pad_right).max(1.0)
    }

    pub fn top(&self) -> f32 {
        f32::from(self.bounds.top())
    }

    pub fn height(&self) -> f32 {
        f32::from(self.bounds.size.height)
    }

    pub fn x_for(&self, time: f64, duration: f64) -> f32 {
        self.left() + (time / duration.max(1e-9)) as f32 * self.width()
    }

    pub fn time_at(&self, x: Pixels, duration: f64) -> f64 {
        ((f64::from(f32::from(x) - self.left()) / self.width() as f64) * duration)
            .clamp(0.0, duration)
    }

    pub fn in_plot(&self, x: Pixels) -> bool {
        f32::from(x) >= self.left()
    }

    /// Baseline plus 1/2/5 × 10ⁿ time ticks, drawn under the plot at `axis_y`.
    pub fn paint_time_axis(
        &self,
        axis_y: f32,
        duration: f64,
        theme: &Theme,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.paint_quad(fill(
            Bounds::new(
                point(px(self.left()), px(axis_y)),
                size(px(self.width()), px(1.0)),
            ),
            theme.viz.grid,
        ));
        let step = tick_step(duration);
        let mut tick = 0.0;
        while tick <= duration + step * 0.01 {
            let x = self.x_for(tick, duration);
            let line = shape_label(&format_tick(tick, step), 9.0, theme.viz.muted, window);
            let _ = line.paint(point(px(x - 4.0), px(axis_y + 3.0)), px(10.0), window, cx);
            tick += step;
        }
    }

    /// Tinted band with a 1px outline marking a committed or in-flight time range.
    pub fn paint_selection(
        &self,
        range: (f64, f64),
        duration: f64,
        top: f32,
        height: f32,
        theme: &Theme,
        window: &mut Window,
    ) {
        let (t0, t1) = range;
        let x0 = self.x_for(t0, duration);
        let x1 = self.x_for(t1, duration);
        let rect = Bounds::new(
            point(px(x0), px(top)),
            size(px((x1 - x0).max(1.0)), px(height)),
        );
        window.paint_quad(fill(rect, theme.viz.series[0].opacity(0.12)));
        window.paint_quad(outline(rect, theme.viz.series[0], BorderStyle::Solid));
    }
}

/// Returns the next 1/2/5 × 10ⁿ tick step giving roughly six axis ticks.
pub fn tick_step(duration: f64) -> f64 {
    let minimum = (duration / 6.0).max(1e-9);
    let magnitude = 10f64.powf(minimum.log10().floor());
    for factor in [1.0, 2.0, 5.0] {
        if magnitude * factor >= minimum {
            return magnitude * factor;
        }
    }
    magnitude * 10.0
}

fn format_tick(value: f64, step: f64) -> String {
    if step >= 1.0 {
        format!("{value:.0}s")
    } else if step >= 0.1 {
        format!("{value:.1}s")
    } else {
        format!("{value:.2}s")
    }
}
