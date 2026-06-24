//! Wayland-native `wlr-layer-shell` voice indicator.
//!
//! Replaces the XWayland eframe wave on Wayland sessions: a software-
//! rendered overlay surface anchored bottom-center, taking no keyboard
//! focus (hence no compositor focus border) and click-through, showing a
//! voice-reactive pulse. The pure rendering/smoothing live in `render`
//! and `level`; the Wayland surface lifecycle lives in `surface`.

pub mod level;
pub mod render;

#[cfg(target_os = "linux")]
pub mod surface;

pub use level::LevelSmoother;
pub use render::{paint_wave, WAVE_H, WAVE_W};

#[cfg(target_os = "linux")]
pub use surface::{run, DEFAULT_BOTTOM_MARGIN};

/// Spawn the layer-shell indicator on its own thread, reading the shared
/// [`IndicatorState`](crate::IndicatorState). `bottom_margin` is the gap
/// (px) above the bottom screen edge. Returns the join handle; the
/// thread lives for the rest of the process.
#[cfg(target_os = "linux")]
pub fn spawn(
    state: std::sync::Arc<crate::IndicatorState>,
    bottom_margin: i32,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("f9-talk-indicator".into())
        .spawn(move || {
            if let Err(e) = surface::run(state, bottom_margin) {
                tracing::warn!("layer-shell indicator exited: {e:#}");
            }
        })
        .expect("spawn f9-talk-indicator thread")
}
