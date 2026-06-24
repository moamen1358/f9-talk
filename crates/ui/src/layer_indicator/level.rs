//! Asymmetric-EMA level smoother shared by both indicator backends.

/// Smooths a raw `0..=1` audio level with a **fast attack / slow
/// release**, so the pulse jumps to speech immediately but decays
/// gently after each word. Extracted from the original eframe indicator
/// so the eframe (X11) and layer-shell (Wayland) paths share one tested
/// implementation.
#[derive(Debug, Clone, Default)]
pub struct LevelSmoother {
    level: f32,
}

impl LevelSmoother {
    pub fn new() -> Self {
        Self { level: 0.0 }
    }

    /// Feed a new raw level (negative values clamped to 0) and return
    /// the new smoothed level. Rise uses α=0.45, fall uses α=0.15.
    pub fn push(&mut self, raw: f32) -> f32 {
        let raw = raw.max(0.0);
        if raw > self.level {
            self.level = 0.55 * self.level + 0.45 * raw;
        } else {
            self.level = 0.85 * self.level + 0.15 * raw;
        }
        self.level
    }

    pub fn level(&self) -> f32 {
        self.level
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rises_fast_falls_slow() {
        let mut s = LevelSmoother::new();
        let up = s.push(1.0); // from 0.0, rise α=0.45
        assert!((up - 0.45).abs() < 1e-6, "rise: {up}");
        let down = s.push(0.0); // from 0.45, fall α=0.15 → 0.85*0.45
        assert!((down - 0.3825).abs() < 1e-6, "fall: {down}");
    }

    #[test]
    fn clamps_negative_to_zero() {
        let mut s = LevelSmoother::new();
        assert_eq!(s.push(-5.0), 0.0);
        assert_eq!(s.level(), 0.0);
    }
}
