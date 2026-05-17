//! Smart positioning for the indicator window.
//!
//! On Linux/X11: bottom-center of the focused window, with fallbacks
//! to mouse cursor and screen center.
//!
//! On macOS/Windows: screen-center-bottom fallback only (no X11).

// ── Linux (X11) ────────────────────────────────────────────────────
#[cfg(target_os = "linux")]
mod x11_impl {
    use std::time::Instant;

    use tracing::{debug, trace};
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt, Window};
    use x11rb::rust_connection::RustConnection;

    const MARGIN_PX: i32 = 8;
    const ABOVE_FOCUSED_BOTTOM: i32 = 24;
    const ABOVE_CURSOR: i32 = 28;
    const ABOVE_SCREEN_BOTTOM: i32 = 120;

    pub struct Positioner {
        conn: RustConnection,
        screen_num: usize,
        net_active_window: u32,
    }

    impl Positioner {
        pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
            let (conn, screen_num) = x11rb::connect(None)?;
            let net_active_window = conn
                .intern_atom(false, b"_NET_ACTIVE_WINDOW")
                .ok()
                .and_then(|c| c.reply().ok())
                .map(|r| r.atom)
                .unwrap_or(0);
            Ok(Self {
                conn,
                screen_num,
                net_active_window,
            })
        }

        pub fn compute_position(&self, indicator_w: i32, indicator_h: i32) -> Option<(i32, i32)> {
            let started = Instant::now();
            let screen_size = self.screen_size();

            // On Wayland the X11 active-window + cursor queries go through
            // XWayland and return stale/incorrect coords (no real "active
            // window" for Wayland-native apps, and the cursor pointer may
            // be locked into XWayland's own root). Skip those probes and
            // anchor at screen-bottom-center directly.
            let on_wayland = std::env::var_os("WAYLAND_DISPLAY").is_some()
                || std::env::var("XDG_SESSION_TYPE")
                    .map(|v| v.eq_ignore_ascii_case("wayland"))
                    .unwrap_or(false);

            let (raw_x, raw_y) = if on_wayland {
                self.screen_center_bottom(indicator_w, indicator_h, screen_size)?
            } else {
                self.focused_window_bottom(indicator_w, indicator_h)
                    .or_else(|| self.cursor_above(indicator_w, indicator_h))
                    .or_else(|| self.screen_center_bottom(indicator_w, indicator_h, screen_size))?
            };

            let pos = clamp_to_screen(raw_x, raw_y, indicator_w, indicator_h, screen_size);

            trace!(
                "indicator pos: ({},{}) (took {:.0?})",
                pos.0,
                pos.1,
                started.elapsed()
            );
            Some(pos)
        }

        fn screen_size(&self) -> Option<(i32, i32)> {
            let screen = self.conn.setup().roots.get(self.screen_num)?;
            Some((
                screen.width_in_pixels as i32,
                screen.height_in_pixels as i32,
            ))
        }

        fn root_window(&self) -> Option<Window> {
            Some(self.conn.setup().roots.get(self.screen_num)?.root)
        }

        fn active_window(&self) -> Option<Window> {
            if self.net_active_window == 0 {
                return None;
            }
            let root = self.root_window()?;
            let cookie = self
                .conn
                .get_property(false, root, self.net_active_window, AtomEnum::WINDOW, 0, 1)
                .ok()?;
            let reply = cookie.reply().ok()?;
            let mut iter = reply.value32()?;
            let win = iter.next()?;
            if win == 0 || win == root {
                None
            } else {
                Some(win)
            }
        }

        fn focused_window_bottom(&self, indicator_w: i32, indicator_h: i32) -> Option<(i32, i32)> {
            let win = self.active_window()?;
            let root = self.root_window()?;
            let geom = self.conn.get_geometry(win).ok()?.reply().ok()?;
            let xlate = self
                .conn
                .translate_coordinates(win, root, 0, 0)
                .ok()?
                .reply()
                .ok()?;
            let win_x = xlate.dst_x as i32;
            let win_y = xlate.dst_y as i32;
            let win_w = geom.width as i32;
            let win_h = geom.height as i32;
            if win_w < 60 || win_h < 60 {
                return None;
            }
            let x = win_x + (win_w - indicator_w) / 2;
            let y = win_y + win_h - indicator_h - ABOVE_FOCUSED_BOTTOM;
            debug!(
                "indicator anchored to focused window at ({x},{y}) \
                 [{win_w}x{win_h}@{win_x},{win_y}]"
            );
            Some((x, y))
        }

        fn cursor_above(&self, indicator_w: i32, indicator_h: i32) -> Option<(i32, i32)> {
            let root = self.root_window()?;
            let p = self.conn.query_pointer(root).ok()?.reply().ok()?;
            let x = p.root_x as i32 - indicator_w / 2;
            let y = p.root_y as i32 - indicator_h - ABOVE_CURSOR;
            debug!("indicator anchored above cursor at ({x},{y})");
            Some((x, y))
        }

        fn screen_center_bottom(
            &self,
            indicator_w: i32,
            indicator_h: i32,
            screen: Option<(i32, i32)>,
        ) -> Option<(i32, i32)> {
            let (sw, sh) = screen?;
            let x = (sw - indicator_w) / 2;
            let y = sh - indicator_h - ABOVE_SCREEN_BOTTOM;
            debug!("indicator anchored at screen-bottom-center ({x},{y})");
            Some((x, y))
        }
    }

    fn clamp_to_screen(x: i32, y: i32, w: i32, h: i32, screen: Option<(i32, i32)>) -> (i32, i32) {
        let (sw, sh) = match screen {
            Some(s) => s,
            None => return (x, y),
        };
        let max_x = sw - w - MARGIN_PX;
        let max_y = sh - h - MARGIN_PX;
        let x = x.clamp(MARGIN_PX, max_x.max(MARGIN_PX));
        let y = y.clamp(MARGIN_PX, max_y.max(MARGIN_PX));
        (x, y)
    }
}

// ── Non-Linux (macOS / Windows) ────────────────────────────────────
#[cfg(not(target_os = "linux"))]
mod fallback_impl {
    use tracing::info;

    pub struct Positioner;

    impl Positioner {
        pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
            info!("using screen-center positioning (no X11)");
            Ok(Self)
        }

        pub fn compute_position(&self, _indicator_w: i32, _indicator_h: i32) -> Option<(i32, i32)> {
            None
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub use fallback_impl::Positioner;
#[cfg(target_os = "linux")]
pub use x11_impl::Positioner;
