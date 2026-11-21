//! Chart framework (plan §5.2): plot geometry, canvas text, the shared time
//! brush and the sequential heat ramp. Charts know the theme and gpui, never
//! the session or the shell.

mod area;
mod brush;
mod frame;
mod ramp;
mod text;

pub use area::paint_area_series;
pub use brush::{Brush, register_time_brush};
pub use frame::PlotFrame;
pub use ramp::heat;
pub use text::{shape_label, truncate_label};
