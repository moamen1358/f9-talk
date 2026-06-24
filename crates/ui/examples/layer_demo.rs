//! Standalone visual harness for the layer-shell voice indicator.
//!
//! Run: `cargo run -p f9-talk-ui --example layer_demo`
//!
//! Shows the bottom-center pulse driven by a synthetic voice-level sweep,
//! so position / no-border / click-through can be verified on screen
//! without the hotkey listener or `input`-group machinery.

#[cfg(target_os = "linux")]
fn main() {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use f9_talk_ui::IndicatorState;
    use parking_lot::Mutex;

    let rms = Arc::new(Mutex::new(0.0f32));
    let state = Arc::new(IndicatorState::new(rms.clone()));
    state.set_recording(true);

    // Synthetic voice level: a slow swell with texture so the bars
    // visibly rise and fall, in the realistic ~0..0.1 RMS range.
    let rms_sweep = rms.clone();
    std::thread::spawn(move || {
        let t0 = Instant::now();
        loop {
            let t = t0.elapsed().as_secs_f32();
            let env = 0.5 + 0.5 * (t * 1.5).sin();
            let tex = 0.5 + 0.5 * (t * 11.0).sin();
            *rms_sweep.lock() = (0.02 + 0.08 * env * tex).clamp(0.0, 0.12);
            std::thread::sleep(Duration::from_millis(16));
        }
    });

    eprintln!("layer_demo: wave should appear bottom-center, no border. Ctrl-C to quit.");
    if let Err(e) =
        f9_talk_ui::layer_indicator::run(state, f9_talk_ui::layer_indicator::DEFAULT_BOTTOM_MARGIN)
    {
        eprintln!("layer_demo error: {e:#}");
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("layer_demo is Linux/Wayland only");
}
