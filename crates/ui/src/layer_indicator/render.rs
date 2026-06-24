//! Pure software rasterizer for the voice-reactive **wave** indicator.
//!
//! Writes **premultiplied** ARGB8888 (`0xAARRGGBB`) — `wl_shm` treats
//! ARGB buffers as premultiplied-alpha. Draws an anti-aliased glowing
//! red sine-wave line: a bright crisp core over a soft neon halo, whose
//! amplitude tracks the smoothed mic level. Background stays transparent
//! and the layer takes no focus, so there is no border.

use std::f32::consts::TAU;

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
}
