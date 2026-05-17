//! Frameless always-on-top click-through indicator.
//!
//! Ports `f9_talk/ui/indicator.py:_paint_wave` (56-point wave path,
//! four-layer paint, asymmetric EMA on RMS, audio-reactive amplitude).

use std::sync::Arc;
use std::time::Instant;

use f9_talk_audio::RmsHandle;
use parking_lot::Mutex;
use tracing::warn;

use crate::keys_dialog::{maybe_show_dialog, KeysDialogState};
use crate::positioning::Positioner;

/// Indicator runtime state shared with the app loop. The audio callback
/// thread updates `rms` (sub-µs uncontested mutex). The app loop flips
/// `recording` on press/release, and the egui paint pass reads both.
pub struct IndicatorState {
    pub rms: RmsHandle,
    pub recording: Arc<Mutex<bool>>,
    pub status_text: Arc<Mutex<Option<String>>>,
}

impl IndicatorState {
    pub fn new(rms: RmsHandle) -> Self {
        Self {
            rms,
            recording: Arc::new(Mutex::new(false)),
            status_text: Arc::new(Mutex::new(None)),
        }
    }

    pub fn set_recording(&self, on: bool) {
        *self.recording.lock() = on;
    }

    pub fn set_status_text(&self, msg: Option<String>) {
        *self.status_text.lock() = msg;
    }
}

/// Indicator window dimensions. Width chosen so the wave's ~320 px
/// pill leaves ≥20 px of soft-glow margin on each side.
// Slim "indicator bar" form factor. On XWayland under Wayland
// compositors (COSMIC) RGBA transparency isn't honored, so the
// window rectangle always renders as an opaque box. By making the
// window very thin we trade the "transparent wave" look for a clean
// slim bar that still shows the wave — visually intentional instead
// of looking like broken transparency.
pub const INDICATOR_W: i32 = 320;
pub const INDICATOR_H: i32 = 22;

/// eframe app: owns the smoothed audio level (asymmetric EMA), the
/// animation clock, the X11 positioner, and the paint loop.
pub struct IndicatorApp {
    state: Arc<IndicatorState>,
    smoothed_level: f32,
    anim_t0: Instant,
    positioner: Option<Positioner>,
    last_recording: bool,
    /// Frames remaining over which we'll keep re-asserting the
    /// OuterPosition. Some window managers (notably GNOME-shell on
    /// Pop!_OS) silently drop the first cross-monitor move, so we
    /// resend the position for ~5 consecutive 16 ms frames after a
    /// press starts.
    reposition_frames_left: u32,
    pending_position: Option<egui::Pos2>,
    last_visible: bool,
    keys_dialog: KeysDialogState,
}

impl IndicatorApp {
    pub fn new(state: Arc<IndicatorState>, keys_dialog: KeysDialogState) -> Self {
        let positioner = match Positioner::new() {
            Ok(p) => Some(p),
            Err(e) => {
                warn!(
                    "could not open X11 connection for smart positioning: {e:?}; \
                    indicator will stay at the eframe default position"
                );
                None
            }
        };
        Self {
            state,
            smoothed_level: 0.0,
            anim_t0: Instant::now(),
            positioner,
            last_recording: false,
            reposition_frames_left: 0,
            pending_position: None,
            last_visible: true,
            keys_dialog,
        }
    }

    fn update_smoothed_level(&mut self, raw: f32) {
        let raw = raw.max(0.0);
        // Asymmetric EMA — fast rise (α=0.45), slow fall (α=0.15).
        // Matches Python `f9_talk/ui/indicator.py:_on_audio_level`.
        if raw > self.smoothed_level {
            self.smoothed_level = 0.55 * self.smoothed_level + 0.45 * raw;
        } else {
            self.smoothed_level = 0.85 * self.smoothed_level + 0.15 * raw;
        }
    }

    fn maybe_reposition(&mut self, ctx: &egui::Context, recording: bool) {
        // Rising edge of `recording`: compute the desired position and
        // re-assert it for a few frames so X11 / mutter / KDE WMs that
        // ignore the very first request pick it up. On COSMIC (Wayland)
        // the compositor pins XWayland windows to its own placement
        // policy regardless of client requests — see the
        // troubleshooting section of README. Nothing to do about that
        // here; the user sees the indicator wherever cosmic-comp puts
        // it (typically top-center).
        if recording && !self.last_recording {
            if let Some(positioner) = self.positioner.as_ref() {
                if let Some((x, y)) = positioner.compute_position(INDICATOR_W, INDICATOR_H) {
                    self.pending_position = Some(egui::pos2(x as f32, y as f32));
                    self.reposition_frames_left = 5;
                }
            }
        }
        self.last_recording = recording;

        if self.reposition_frames_left > 0 {
            if let Some(pos) = self.pending_position {
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(pos));
                self.reposition_frames_left -= 1;
            }
        }
    }
}

impl eframe::App for IndicatorApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let recording = *self.state.recording.lock();
        let status = self.state.status_text.lock().clone();

        // Render the keys dialog when requested by the tray.
        maybe_show_dialog(ctx, &self.keys_dialog);

        self.maybe_reposition(ctx, recording);

        // Visibility tracks ONLY the recording state, not status_text.
        // On Wayland (COSMIC), every map/unmap shifts keyboard focus —
        // so we keep the window mapped strictly while F9 is held and
        // unmap the instant it's released. Transcribing/translating/
        // typing all happen with the indicator hidden so the typer's
        // uinput keys land on the user's previous focus.
        let want_visible = recording;
        if want_visible != self.last_visible {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(want_visible));
            self.last_visible = want_visible;
        }

        if !recording && status.is_none() {
            // Idle — drop to 1 fps so we wake quickly when state flips.
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
            return;
        }

        // Active — keep ~60 fps.
        ctx.request_repaint_after(std::time::Duration::from_millis(16));

        let raw_rms = if recording {
            *self.state.rms.lock()
        } else {
            0.0
        };
        self.update_smoothed_level(raw_rms);

        let anim_t = self.anim_t0.elapsed().as_secs_f32();

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ctx, |ui| {
                let avail = ui.available_rect_before_wrap();
                let pill_w = 320.0_f32.min(avail.width());
                let pill_h = 56.0_f32.min(avail.height());
                let pill = egui::Rect::from_center_size(avail.center(), egui::vec2(pill_w, pill_h));

                if status.is_some() {
                    // Status-text mode: dark pill + centered text.
                    ui.painter().rect_filled(
                        pill,
                        pill_h * 0.5,
                        egui::Color32::from_rgba_unmultiplied(10, 10, 10, 245),
                    );
                    ui.painter().text(
                        pill.center(),
                        egui::Align2::CENTER_CENTER,
                        status.as_deref().unwrap_or(""),
                        egui::FontId::proportional(18.0),
                        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 230),
                    );
                } else if recording {
                    // Wave mode: no background pill, four-layer wave.
                    let painter = ui.painter().clone();
                    paint_wave(&painter, pill, anim_t, self.smoothed_level);
                }
            });
    }
}

/// Build the 56-point wave path. Mirrors
/// `f9_talk/ui/indicator.py:_build_wave_path` — silence ≈ 0.08, normal
/// speech ≈ 1.0, loud ≈ 1.85 amplitude scale.
fn build_wave_path(
    rect: egui::Rect,
    anim_t: f32,
    time_offset: f32,
    amp_mult: f32,
    audio_level_smoothed: f32,
) -> Vec<egui::Pos2> {
    let x_start = rect.min.x + 8.0;
    let x_end = rect.max.x - 8.0;
    let cy = rect.center().y;
    let n: usize = 56;
    let t_anim = anim_t + time_offset;
    let level_scale = 0.08 + (audio_level_smoothed * 13.0).min(1.85);
    // Cap amplitude to ~40% of half-height so the wave never clips at
    // the window's top/bottom edge. The slim 22px indicator gives us
    // ~9px above and below cy; 40% of 9 ≈ 3.5px peak deviation.
    let max_amp = (rect.height() * 0.4).min(14.0);

    let mut pts = Vec::with_capacity(n);
    for i in 0..n {
        let progress = i as f32 / (n as f32 - 1.0);
        let x = x_start + (x_end - x_start) * progress;
        let envelope = 0.5 - 0.5 * (progress * std::f32::consts::TAU).cos();
        let t = t_anim * 5.5 + progress * 7.0;
        let wave = 0.55 * t.sin()
            + 0.30 * (t * 2.1 + 1.4).sin()
            + 0.18 * (t * 0.6 + 3.0).sin()
            + 0.10 * (t * 3.7 + 2.0).sin();
        let y = cy + max_amp * amp_mult * envelope * wave * level_scale;
        pts.push(egui::pos2(x, y));
    }
    pts
}

fn paint_wave(painter: &egui::Painter, rect: egui::Rect, anim_t: f32, level: f32) {
    let main = build_wave_path(rect, anim_t, 0.0, 1.0, level);
    let echo = build_wave_path(rect, anim_t, -0.18, 0.55, level);

    // Layer 1: outer wide soft glow
    painter.add(egui::Shape::line(
        main.clone(),
        egui::Stroke::new(11.0, egui::Color32::from_rgba_unmultiplied(255, 40, 60, 35)),
    ));
    // Layer 2: mid glow
    painter.add(egui::Shape::line(
        main.clone(),
        egui::Stroke::new(7.0, egui::Color32::from_rgba_unmultiplied(255, 50, 60, 70)),
    ));
    // Layer 3: echo wave (offset back in time)
    painter.add(egui::Shape::line(
        echo,
        egui::Stroke::new(1.6, egui::Color32::from_rgba_unmultiplied(255, 90, 100, 90)),
    ));
    // Layer 4: crisp red top line. egui Stroke is solid colour rather
    // than a Qt LinearGradient — close enough at this stroke width.
    painter.add(egui::Shape::line(
        main,
        egui::Stroke::new(2.6, egui::Color32::from_rgba_unmultiplied(238, 60, 80, 245)),
    ));
}
