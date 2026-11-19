//! Domain-free chart primitives (sparklines, stacked bars, meters, share bars) built on the
//! [`crate::Theme`] palette. Callers pass the styles; nothing here picks a color.

mod meter;
mod share_bar;
mod sparkline;
mod stacked_bars;

pub use meter::meter_spans;
pub use share_bar::ShareSegment;
pub use share_bar::share_bar_spans;
pub use share_bar::share_legend_lines;
pub use sparkline::sparkline;
pub use stacked_bars::Bar;
pub use stacked_bars::stacked_bar_rows;
