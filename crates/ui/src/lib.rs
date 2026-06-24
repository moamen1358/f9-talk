//! eframe/egui indicator (wave animation) + tray icon + keys dialog.
//!
//! Status: M3 in progress.

#![forbid(unsafe_code)]

pub mod indicator;
pub mod layer_indicator;
pub mod positioning;

pub use indicator::{IndicatorApp, IndicatorState, INDICATOR_H, INDICATOR_W};
pub use positioning::Positioner;
