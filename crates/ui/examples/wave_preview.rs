//! Offscreen preview of the indicator wave — renders several levels onto
//! a dark "content" background and saves a PNG, so the look can be judged
//! and iterated without screen-capturing a live surface.
//!
//! Run: `cargo run -p f9-talk-ui --example wave_preview -- /path/out.png`

use f9_talk_ui::layer_indicator::{paint_wave, WAVE_H, WAVE_W};
use image::{Rgb, RgbImage};

fn main() {
    let out = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "wave_preview.png".into());

    // A few representative levels and animation phases.
    let rows: &[(f32, f32)] = &[(0.12, 0.20), (0.45, 0.55), (0.85, 0.95), (1.0, 1.6)];
    let pad = 14u32;
    let w = WAVE_W + pad * 2;
    let h = (WAVE_H + pad) * rows.len() as u32 + pad;
    // Dark neutral background to mimic an editor/terminal.
    let mut img = RgbImage::from_pixel(w, h, Rgb([28, 28, 32]));

    let mut buf = vec![0u32; (WAVE_W * WAVE_H) as usize];
    for (i, &(level, anim_t)) in rows.iter().enumerate() {
        paint_wave(&mut buf, WAVE_W, WAVE_H, level, anim_t);
        let oy = pad + i as u32 * (WAVE_H + pad);
        for y in 0..WAVE_H {
            for x in 0..WAVE_W {
                let px = buf[(y * WAVE_W + x) as usize];
                let a = (px >> 24) & 0xFF;
                if a == 0 {
                    continue;
                }
                // Premultiplied src over background: out = src + bg*(255-a)/255.
                let (sr, sg, sb) = ((px >> 16) & 0xFF, (px >> 8) & 0xFF, px & 0xFF);
                let bg = img.get_pixel(pad + x, oy + y);
                let inv = 255 - a;
                let r = (sr + bg[0] as u32 * inv / 255).min(255) as u8;
                let g = (sg + bg[1] as u32 * inv / 255).min(255) as u8;
                let b = (sb + bg[2] as u32 * inv / 255).min(255) as u8;
                img.put_pixel(pad + x, oy + y, Rgb([r, g, b]));
            }
        }
    }
    img.save(&out).expect("save preview png");
    eprintln!("saved {out} ({w}x{h})");
}
