//! Pure software rasterizer for the voice-reactive **wave** indicator.
//!
//! Writes **premultiplied** ARGB8888 (`0xAARRGGBB`) — `wl_shm` treats
//! ARGB buffers as premultiplied-alpha. Draws an anti-aliased glowing
//! red sine-wave line: a bright crisp core over a soft neon halo, whose
//! amplitude tracks the smoothed mic level. Background stays transparent
//! and the layer takes no focus, so there is no border.

use std::f32::consts::{FRAC_1_SQRT_2, TAU};

/// Logical surface size. Compact (~1/4 the original width) so it reads as
/// a small neat waveform rather than a long line.
pub const WAVE_W: u32 = 130;
pub const WAVE_H: u32 = 72;

/// Gain applied to the smoothed mic RMS before it drives the amplitude.
/// Speech RMS sits around 0.05–0.1, so ~12× lifts normal speech to a
/// near-full wave.
pub const LEVEL_GAIN: f32 = 12.0;

// Bright crisp core and deeper soft glow (same red hue family).
const CORE: (f32, f32, f32) = (255.0, 78.0, 92.0);
const GLOW: (f32, f32, f32) = (226.0, 38.0, 58.0);

/// Horizontal inset so the glow doesn't clip at the side edges.
const MARGIN: i32 = 10;
/// Crisp core half-thickness in px (anti-aliased over a 1px band).
const CORE_HALF: f32 = 1.6;
/// Soft glow half-reach in px.
const GLOW_REACH: f32 = 10.0;
/// Peak alpha of the soft glow (0..1).
const GLOW_PEAK: f32 = 0.5;
/// Amplitude fraction when silent, so the line still gently breathes.
const BASELINE: f32 = 0.15;

// --- Idle "ready" dot ---------------------------------------------------
/// Core radius of the idle dot (px) and the swell its breath adds.
const DOT_CORE_R: f32 = 3.4;
const DOT_BREATH_R: f32 = 0.8;
/// Soft glow reach beyond the core (px).
const DOT_GLOW_REACH: f32 = 6.0;
/// Peak alpha of the dot's soft glow (0..1).
const DOT_GLOW_PEAK: f32 = 0.45;
/// Overall opacity the breath swings between (low → high).
const DOT_ALPHA_LO: f32 = 0.55;
const DOT_ALPHA_HI: f32 = 0.95;
/// Seconds per breath cycle — slow and calm, not a flicker.
const DOT_BREATH_PERIOD: f32 = 2.6;

// --- Hover "close" button ----------------------------------------------
/// Radius of the solid disc shown when the dot is hovered (px).
const CLOSE_DISC_R: f32 = 6.5;
/// Half-length and half-thickness of the white "×" strokes (px).
const CLOSE_ARM: f32 = 3.8;
const CLOSE_STROKE_HALF: f32 = 1.15;
/// Colour of the "×" — near-white so it reads clearly over the red disc.
const CLOSE_X: (f32, f32, f32) = (250.0, 250.0, 255.0);

/// Wave displacement (~-1..1) at horizontal progress `p` (0..1) and time.
/// Sum of detuned sines — the original eframe wave shape.
fn wave_sample(p: f32, anim_t: f32) -> f32 {
    let t = anim_t * 5.5 + p * 7.0;
    0.55 * t.sin()
        + 0.30 * (t * 2.1 + 1.4).sin()
        + 0.18 * (t * 0.6 + 3.0).sin()
        + 0.10 * (t * 3.7 + 2.0).sin()
}

/// Paint one frame into `buf` (length `w*h`) as premultiplied ARGB. The
/// `level` is the gained smoothed mic level (0..1); `anim_t` is seconds.
pub fn paint_wave(buf: &mut [u32], w: u32, h: u32, level: f32, anim_t: f32) {
    for px in buf.iter_mut() {
        *px = 0; // transparent
    }
    let wi = w as i32;
    let hi = h as i32;
    let x0 = MARGIN;
    let x1 = wi - MARGIN;
    if x1 <= x0 {
        return;
    }
    let cy = h as f32 * 0.5;
    let span = (x1 - x0) as f32;
    let max_amp = (h as f32 * 0.5 - GLOW_REACH - 2.0).max(2.0);
    let amp_scale = (BASELINE + level.clamp(0.0, 1.0)).min(1.0);

    for x in x0..x1 {
        let p = (x - x0) as f32 / span;
        // Taper to nothing at both ends so the line reads as a pill.
        let envelope = 0.5 - 0.5 * (p * TAU).cos();
        let disp = max_amp * envelope * wave_sample(p, anim_t) * amp_scale;
        let yc = cy + disp;

        let y_lo = (yc - GLOW_REACH).floor().max(0.0) as i32;
        let y_hi = (yc + GLOW_REACH).ceil().min((hi - 1) as f32) as i32;
        for y in y_lo..=y_hi {
            let d = ((y as f32 + 0.5) - yc).abs();
            // Soft neon glow: quadratic falloff to GLOW_REACH.
            let g = (1.0 - d / GLOW_REACH).clamp(0.0, 1.0);
            let glow_a = g * g * GLOW_PEAK;
            // Crisp core: full alpha within CORE_HALF, 1px anti-aliased edge.
            let core_a = (CORE_HALF + 0.5 - d).clamp(0.0, 1.0);
            let a = glow_a.max(core_a);
            if a <= 0.004 {
                continue;
            }
            // Colour shifts from deep-glow red to bright core where solid.
            let cf = core_a;
            let rgb = (
                GLOW.0 + (CORE.0 - GLOW.0) * cf,
                GLOW.1 + (CORE.1 - GLOW.1) * cf,
                GLOW.2 + (CORE.2 - GLOW.2) * cf,
            );
            put_px(buf, wi, x, y, rgb, a);
        }
    }
}

/// Paint the idle "ready" dot: a small softly-glowing red dot at the
/// surface centre that gently breathes, so the user can see at a glance
/// the tool is alive and listening. Shown whenever we're not recording
/// (the surface vanishes only if the process dies). `anim_t` is seconds;
/// output is premultiplied ARGB like [`paint_wave`].
pub fn paint_dot(buf: &mut [u32], w: u32, h: u32, anim_t: f32) {
    for px in buf.iter_mut() {
        *px = 0; // transparent
    }
    let wi = w as i32;
    let hi = h as i32;
    let cx = w as f32 * 0.5;
    let cy = h as f32 * 0.5;

    // Slow breath: 0..1 swell driving radius and overall opacity.
    let breath = 0.5 - 0.5 * (anim_t * (TAU / DOT_BREATH_PERIOD)).cos();
    let core_r = DOT_CORE_R + DOT_BREATH_R * breath;
    let glow_r = core_r + DOT_GLOW_REACH;
    let alpha_mul = DOT_ALPHA_LO + (DOT_ALPHA_HI - DOT_ALPHA_LO) * breath;

    let x0 = (cx - glow_r).floor().max(0.0) as i32;
    let x1 = (cx + glow_r).ceil().min((wi - 1) as f32) as i32;
    let y0 = (cy - glow_r).floor().max(0.0) as i32;
    let y1 = (cy + glow_r).ceil().min((hi - 1) as f32) as i32;

    for y in y0..=y1 {
        for x in x0..=x1 {
            let dx = (x as f32 + 0.5) - cx;
            let dy = (y as f32 + 0.5) - cy;
            let d = (dx * dx + dy * dy).sqrt();
            // Soft neon glow: quadratic falloff from the core out to glow_r.
            let g = (1.0 - (d - core_r) / DOT_GLOW_REACH).clamp(0.0, 1.0);
            let glow_a = g * g * DOT_GLOW_PEAK;
            // Crisp core: solid within core_r, 1px anti-aliased edge.
            let core_a = (core_r + 0.5 - d).clamp(0.0, 1.0);
            let a = glow_a.max(core_a) * alpha_mul;
            if a <= 0.004 {
                continue;
            }
            // Bright core fading to deep-glow red at the rim.
            let cf = core_a;
            let rgb = (
                GLOW.0 + (CORE.0 - GLOW.0) * cf,
                GLOW.1 + (CORE.1 - GLOW.1) * cf,
                GLOW.2 + (CORE.2 - GLOW.2) * cf,
            );
            put_px(buf, wi, x, y, rgb, a);
        }
    }
}

/// Paint the "close" affordance shown while the pointer hovers the dot:
/// a brighter solid red disc with a white "×", so it's obvious a click
/// quits the tool. Premultiplied ARGB like [`paint_dot`].
pub fn paint_close(buf: &mut [u32], w: u32, h: u32) {
    for px in buf.iter_mut() {
        *px = 0; // transparent
    }
    let wi = w as i32;
    let hi = h as i32;
    let cx = w as f32 * 0.5;
    let cy = h as f32 * 0.5;
    let glow_r = CLOSE_DISC_R + DOT_GLOW_REACH;

    let x0 = (cx - glow_r).floor().max(0.0) as i32;
    let x1 = (cx + glow_r).ceil().min((wi - 1) as f32) as i32;
    let y0 = (cy - glow_r).floor().max(0.0) as i32;
    let y1 = (cy + glow_r).ceil().min((hi - 1) as f32) as i32;

    // Solid red disc with the same soft glow as the dot.
    for y in y0..=y1 {
        for x in x0..=x1 {
            let dx = (x as f32 + 0.5) - cx;
            let dy = (y as f32 + 0.5) - cy;
            let d = (dx * dx + dy * dy).sqrt();
            let g = (1.0 - (d - CLOSE_DISC_R) / DOT_GLOW_REACH).clamp(0.0, 1.0);
            let glow_a = g * g * DOT_GLOW_PEAK;
            let core_a = (CLOSE_DISC_R + 0.5 - d).clamp(0.0, 1.0);
            let a = glow_a.max(core_a);
            if a <= 0.004 {
                continue;
            }
            let cf = core_a;
            let rgb = (
                GLOW.0 + (CORE.0 - GLOW.0) * cf,
                GLOW.1 + (CORE.1 - GLOW.1) * cf,
                GLOW.2 + (CORE.2 - GLOW.2) * cf,
            );
            put_px(buf, wi, x, y, rgb, a);
        }
    }

    // White "×" composited over the disc.
    for y in y0..=y1 {
        for x in x0..=x1 {
            let dx = (x as f32 + 0.5) - cx;
            let dy = (y as f32 + 0.5) - cy;
            if dx.abs() > CLOSE_ARM + 1.0 || dy.abs() > CLOSE_ARM + 1.0 {
                continue;
            }
            // Distance to each diagonal through the centre; nearer one wins.
            let d1 = (dx - dy).abs() * FRAC_1_SQRT_2;
            let d2 = (dx + dy).abs() * FRAC_1_SQRT_2;
            let a = (CLOSE_STROKE_HALF + 0.5 - d1.min(d2)).clamp(0.0, 1.0);
            if a <= 0.004 {
                continue;
            }
            blend_px(buf, wi, x, y, CLOSE_X, a);
        }
    }
}

/// Blend a premultiplied pixel `(r,g,b)` at alpha `a` (0..1), keeping the
/// brighter of any overlapping contribution so columns merge seamlessly.
fn put_px(buf: &mut [u32], w: i32, x: i32, y: i32, (r, g, b): (f32, f32, f32), a: f32) {
    let idx = (y * w + x) as usize;
    let abyte = (a * 255.0).round().clamp(0.0, 255.0) as u32;
    if abyte <= (buf[idx] >> 24) {
        return;
    }
    let pr = (r * a).round() as u32;
    let pg = (g * a).round() as u32;
    let pb = (b * a).round() as u32;
    buf[idx] = (abyte << 24) | (pr << 16) | (pg << 8) | pb;
}

/// Alpha-composite a straight (non-premultiplied) colour `(r,g,b)` at
/// coverage `sa` (0..1) **over** the existing premultiplied pixel —
/// `out = src + dst·(1−sa)`. Used to lay the white "×" over the red disc.
fn blend_px(buf: &mut [u32], w: i32, x: i32, y: i32, (r, g, b): (f32, f32, f32), sa: f32) {
    let idx = (y * w + x) as usize;
    let d = buf[idx];
    let da = ((d >> 24) & 0xFF) as f32 / 255.0;
    let dr = ((d >> 16) & 0xFF) as f32; // already premultiplied
    let dg = ((d >> 8) & 0xFF) as f32;
    let db = (d & 0xFF) as f32;
    let inv = 1.0 - sa;
    let out_a = ((sa + da * inv) * 255.0).round().clamp(0.0, 255.0) as u32;
    let pr = (r * sa + dr * inv).round().clamp(0.0, 255.0) as u32;
    let pg = (g * sa + dg * inv).round().clamp(0.0, 255.0) as u32;
    let pb = (b * sa + db * inv).round().clamp(0.0, 255.0) as u32;
    buf[idx] = (out_a << 24) | (pr << 16) | (pg << 8) | pb;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corners_transparent() {
        let mut buf = vec![0u32; (WAVE_W * WAVE_H) as usize];
        paint_wave(&mut buf, WAVE_W, WAVE_H, 1.0, 0.3);
        assert_eq!(buf[0] >> 24, 0, "top-left corner");
        assert_eq!(
            buf[(WAVE_W * WAVE_H - 1) as usize] >> 24,
            0,
            "bottom-right corner"
        );
    }

    #[test]
    fn draws_bright_red_core_when_active() {
        let mut buf = vec![0u32; (WAVE_W * WAVE_H) as usize];
        paint_wave(&mut buf, WAVE_W, WAVE_H, 1.0, 0.3);
        let lit = buf.iter().filter(|&&p| p >> 24 > 0).count();
        assert!(lit > 0, "wave should light pixels");
        // Somewhere there is a near-opaque, bright-red core pixel.
        assert!(
            buf.iter()
                .any(|&p| (p >> 24) > 230 && ((p >> 16) & 0xFF) > 200),
            "expected a bright red core pixel"
        );
    }

    #[test]
    fn premultiplied_alpha() {
        let mut buf = vec![0u32; 4];
        put_px(&mut buf, 2, 0, 0, (255.0, 0.0, 0.0), 0.5);
        assert_eq!(buf[0] >> 24, 128, "alpha ~0.5*255");
        // Premultiplied red ≈ 255 * 0.5.
        assert_eq!((buf[0] >> 16) & 0xFF, 128, "premultiplied red");
    }

    #[test]
    fn baseline_visible_when_silent() {
        let mut buf = vec![0u32; (WAVE_W * WAVE_H) as usize];
        paint_wave(&mut buf, WAVE_W, WAVE_H, 0.0, 0.3);
        assert!(
            buf.iter().any(|&p| p >> 24 > 0),
            "baseline wave should be visible"
        );
    }

    #[test]
    fn dot_lights_center_not_corners() {
        let mut buf = vec![0u32; (WAVE_W * WAVE_H) as usize];
        paint_dot(&mut buf, WAVE_W, WAVE_H, 0.0);
        let center = ((WAVE_H / 2) * WAVE_W + WAVE_W / 2) as usize;
        assert!(buf[center] >> 24 > 0, "dot centre should be lit");
        assert_eq!(buf[0] >> 24, 0, "top-left corner transparent");
        assert_eq!(
            buf[(WAVE_W * WAVE_H - 1) as usize] >> 24,
            0,
            "bottom-right corner transparent"
        );
    }

    #[test]
    fn close_button_draws_white_x_over_red() {
        let mut buf = vec![0u32; (WAVE_W * WAVE_H) as usize];
        paint_close(&mut buf, WAVE_W, WAVE_H);
        let center = ((WAVE_H / 2) * WAVE_W + WAVE_W / 2) as usize;
        let p = buf[center];
        assert_eq!(p >> 24, 255, "centre of close button is opaque");
        // The "×" passes through the centre: green+blue ride high there,
        // which the bare red disc (low green) could never produce.
        assert!(
            (p >> 8) & 0xFF > 180 && p & 0xFF > 180,
            "centre should read white (the ×), got {p:#010x}"
        );
        assert_eq!(buf[0] >> 24, 0, "corner transparent");
    }

    #[test]
    fn dot_stays_within_a_small_radius() {
        // The dot must read as a compact dot, not fill the surface: no lit
        // pixel should sit far from centre.
        let mut buf = vec![0u32; (WAVE_W * WAVE_H) as usize];
        paint_dot(&mut buf, WAVE_W, WAVE_H, 0.0);
        let (cx, cy) = (WAVE_W as f32 * 0.5, WAVE_H as f32 * 0.5);
        for (i, &p) in buf.iter().enumerate() {
            if p >> 24 > 0 {
                let x = (i as u32 % WAVE_W) as f32;
                let y = (i as u32 / WAVE_W) as f32;
                let d = ((x - cx).powi(2) + (y - cy).powi(2)).sqrt();
                assert!(d < 16.0, "lit pixel {d:.1}px from centre — dot too big");
            }
        }
    }
}
