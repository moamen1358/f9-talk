//! eframe/egui indicator (wave animation) + tray icon + keys dialog.
//!
//! Status: M3 in progress.

#![forbid(unsafe_code)]

pub mod indicator;
pub mod keys_dialog;
pub mod positioning;
pub mod tray;

pub use indicator::{IndicatorApp, IndicatorState, INDICATOR_H, INDICATOR_W};
pub use keys_dialog::{KeysDialogState, KeysSaved};
pub use positioning::Positioner;
pub use tray::{TrayCommand, TrayHandle, VisualState as TrayVisualState};
