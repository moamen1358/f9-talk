//! Linux text injection: xdotool → clipboard+Ctrl+V → uinput scancodes.

use std::path::Path;
use std::thread::sleep;
use std::time::Duration;

use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, EventType, InputEvent, KeyCode};
use tracing::{debug, info, warn};

use super::{PreflightError, PRE_TYPE_SLEEP};

const UINPUT_DEV: &str = "/dev/uinput";

const KEY_DELAY: Duration = Duration::from_micros(2_500);

// Inter-keystroke delay for wtype, in milliseconds. wtype defaults to 0,
// which floods the compositor's virtual-keyboard queue fast enough that
// cosmic-comp silently drops events (observed: missing spaces and
// punctuation mid-transcript). A few ms between keystrokes makes it
// reliable while staying imperceptibly fast for dictation-length text.
const WTYPE_KEY_DELAY_MS: &str = "8";

pub fn preflight() -> Result<(), PreflightError> {
    let dev = Path::new(UINPUT_DEV);
    if !dev.exists() {
        warn!("preflight: {} is missing", UINPUT_DEV);
        return Ok(());
    }
    match std::fs::OpenOptions::new().write(true).open(dev) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            warn!(
                "preflight: {} is not writable yet — \
                run `sudo usermod -aG input $USER` and install the udev rule \
                (`packaging/debian/udev/99-f9-talk.rules`), then log out + in",
                UINPUT_DEV
            );
            Ok(())
        }
        Err(e) => {
            warn!("preflight: unexpected open error on {}: {e}", UINPUT_DEV);
            Ok(())
        }
    }
}

pub struct Typer {
    device: VirtualDevice,
    clipboard: Option<arboard::Clipboard>,
    has_xdotool: bool,
    has_wtype: bool,
    has_wl_paste: bool,
}

impl Typer {
    pub fn new() -> anyhow::Result<Self> {
        let mut keys = AttributeSet::<KeyCode>::new();
        for k in EVERY_KEY_WE_USE {
            keys.insert(*k);
        }

        let device = VirtualDevice::builder()
            .map_err(|e| anyhow::anyhow!("could not open /dev/uinput: {e}"))?
            .name("f9-talk virtual keyboard")
            .with_keys(&keys)
            .map_err(|e| anyhow::anyhow!("uinput with_keys failed: {e}"))?
            .build()
            .map_err(|e| anyhow::anyhow!("uinput build failed: {e}"))?;

        sleep(Duration::from_millis(120));

        // xdotool only works on X11. On Wayland it returns exit 0 but
        // types into XWayland's void — Wayland-native apps see nothing.
        // Detect via XDG_SESSION_TYPE / WAYLAND_DISPLAY and skip it so
        // the clipboard+Ctrl+V (uinput-injected) path takes over.
        let on_wayland = std::env::var_os("WAYLAND_DISPLAY").is_some()
            || std::env::var("XDG_SESSION_TYPE")
                .map(|v| v.eq_ignore_ascii_case("wayland"))
                .unwrap_or(false);
        let has_xdotool = which("xdotool") && !on_wayland;
        if on_wayland && which("xdotool") {
            info!("Wayland session detected — skipping xdotool primary path (it silently no-ops on Wayland-native windows)");
        }

        // Atomic clipboard paste — the preferred Wayland path. `wl-copy`
        // publishes the whole transcript, then a single uinput Ctrl+V
        // pastes it. Because the text lands in ONE paste rather than
        // key-by-key, the compositor cannot drop characters. This matters
        // on cosmic-comp, which drops fast `zwp_virtual_keyboard` events
        // (wtype) and uinput scancodes alike — observed as missing spaces
        // mid-transcript. The previous clipboard is saved and restored so
        // dictation doesn't clobber it. Needs both `wl-copy` + `wl-paste`.
        let has_wl_paste = on_wayland && which("wl-copy") && which("wl-paste");

        // wtype: per-key injection via `zwp_virtual_keyboard_v1` with its
        // OWN keymap (layout-independent). Kept as the Wayland fallback
        // when wl-clipboard isn't installed; less reliable than paste
        // because the compositor can drop individual keystrokes.
        let has_wtype = which("wtype") && on_wayland;

        // arboard clipboard fallback (mainly X11; on Wayland it needs
        // wl-clipboard to actually publish, hence the gate).
        let has_wl_clipboard = which("wl-copy");
        let clipboard = if on_wayland && !has_wl_clipboard {
            None
        } else {
            match arboard::Clipboard::new() {
                Ok(c) => Some(c),
                Err(e) => {
                    warn!("could not open clipboard ({e})");
                    None
                }
            }
        };

        let primary = if has_wl_paste {
            "clipboard paste (wl-copy + Ctrl+V, atomic)"
        } else if has_wtype {
            "wtype (Wayland virtual-keyboard, layout-independent)"
        } else if has_xdotool {
            "xdotool"
        } else if clipboard.is_some() {
            "clipboard+Ctrl+V"
        } else {
            "scancode (en-US layout only)"
        };
        info!(
            "uinput typer ready (virtual device: 'f9-talk virtual keyboard'; \
             primary={primary})"
        );
        Ok(Typer {
            device,
            clipboard,
            has_xdotool,
            has_wtype,
            has_wl_paste,
        })
    }

    pub fn type_text(&mut self, text: &str) -> anyhow::Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        sleep(PRE_TYPE_SLEEP);

        // Atomic clipboard paste — preferred on Wayland; can't drop chars.
        if self.has_wl_paste {
            match self.type_via_wl_paste(text) {
                Ok(()) => return Ok(()),
                Err(e) => warn!("clipboard-paste path failed ({e}); falling back to wtype"),
            }
        }

        // wtype: layout-independent per-key Wayland injection (fallback).
        if self.has_wtype {
            match self.type_via_wtype(text) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    warn!("wtype path failed ({e}); falling back to clipboard/scancode")
                }
            }
        }

        if self.has_xdotool {
            match std::process::Command::new("xdotool")
                .args(["type", "--clearmodifiers", "--delay", "0", "--", text])
                .status()
            {
                Ok(s) if s.success() => {
                    debug!("xdotool typed {} chars", text.len());
                    return Ok(());
                }
                Ok(s) => warn!("xdotool exited non-zero ({s}); falling back to clipboard"),
                Err(e) => warn!("xdotool spawn failed ({e}); falling back to clipboard"),
            }
        }

        if let Some(cb) = self.clipboard.as_mut() {
            match cb.set_text(text) {
                Ok(()) => {
                    debug!("clipboard set ({} chars); sending Ctrl+V", text.len());
                    sleep(Duration::from_millis(80));
                    return self.send_ctrl_v();
                }
                Err(e) => {
                    warn!("clipboard set_text failed ({e}); falling back to scancode typing");
                }
            }
        }

        for c in text.chars() {
            if c == '\r' {
                continue;
            }
            if let Some((key, needs_shift)) = ascii_char_to_key(c) {
                self.tap(key, needs_shift)?;
            } else {
                self.type_unicode(c as u32)?;
            }
            sleep(KEY_DELAY);
        }
        Ok(())
    }

    /// Set the clipboard to `text` via `wl-copy`, paste it with a single
    /// uinput Ctrl+V, then restore the prior clipboard. The whole
    /// transcript is inserted atomically, so the compositor can't drop
    /// characters the way it does with per-key injection.
    fn type_via_wl_paste(&mut self, text: &str) -> anyhow::Result<()> {
        // Save the current clipboard text (best-effort) so we can restore
        // it; dictation shouldn't silently eat what the user had copied.
        let saved = std::process::Command::new("wl-paste")
            .arg("--no-newline")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| o.stdout);

        wl_copy(text.as_bytes())?;
        sleep(Duration::from_millis(40));
        self.send_ctrl_v()?;
        // Give the focused app time to consume the paste before we put the
        // old clipboard back.
        sleep(Duration::from_millis(200));

        match saved {
            Some(prev) if !prev.is_empty() => {
                if let Err(e) = wl_copy(&prev) {
                    warn!("could not restore clipboard: {e}");
                }
            }
            _ => {
                let _ = std::process::Command::new("wl-copy")
                    .arg("--clear")
                    .status();
            }
        }
        debug!("pasted {} chars via wl-copy + Ctrl+V", text.len());
        Ok(())
    }

    /// Inject `text` through wtype, reading it from stdin (`wtype -d N -`).
    /// stdin avoids wtype's argv flag-parsing entirely, so a transcript
    /// starting with '-' types literally; `-d` paces keystrokes so the
    /// compositor doesn't drop any.
    fn type_via_wtype(&self, text: &str) -> anyhow::Result<()> {
        use std::io::Write;
        use std::process::Stdio;

        let mut child = std::process::Command::new("wtype")
            .args(["-d", WTYPE_KEY_DELAY_MS, "-"])
            .stdin(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow::anyhow!("wtype spawn failed: {e}"))?;
        child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("wtype stdin unavailable"))?
            .write_all(text.as_bytes())
            .map_err(|e| anyhow::anyhow!("writing to wtype stdin failed: {e}"))?;
        let status = child
            .wait()
            .map_err(|e| anyhow::anyhow!("waiting on wtype failed: {e}"))?;
        if status.success() {
            debug!("wtype typed {} chars", text.len());
            Ok(())
        } else {
            Err(anyhow::anyhow!("wtype exited {status}"))
        }
    }

    fn send_ctrl_v(&mut self) -> anyhow::Result<()> {
        self.device.emit(&[
            key_event(KeyCode::KEY_LEFTCTRL, 1),
            key_event(KeyCode::KEY_V, 1),
            key_event(KeyCode::KEY_V, 0),
            key_event(KeyCode::KEY_LEFTCTRL, 0),
        ])?;
        Ok(())
    }

    fn tap(&mut self, key: KeyCode, needs_shift: bool) -> anyhow::Result<()> {
        let mut events: Vec<InputEvent> = Vec::with_capacity(4);
        if needs_shift {
            events.push(key_event(KeyCode::KEY_LEFTSHIFT, 1));
        }
        events.push(key_event(key, 1));
        events.push(key_event(key, 0));
        if needs_shift {
            events.push(key_event(KeyCode::KEY_LEFTSHIFT, 0));
        }
        self.device.emit(&events)?;
        Ok(())
    }

    fn type_unicode(&mut self, codepoint: u32) -> anyhow::Result<()> {
        let hex = format!("{codepoint:x}");
        debug!("typing unicode U+{hex} via Ctrl+Shift+U");
        self.device.emit(&[
            key_event(KeyCode::KEY_LEFTCTRL, 1),
            key_event(KeyCode::KEY_LEFTSHIFT, 1),
            key_event(KeyCode::KEY_U, 1),
            key_event(KeyCode::KEY_U, 0),
            key_event(KeyCode::KEY_LEFTSHIFT, 0),
            key_event(KeyCode::KEY_LEFTCTRL, 0),
        ])?;
        sleep(Duration::from_millis(5));
        for c in hex.chars() {
            if let Some((key, _shift)) = ascii_char_to_key(c) {
                self.tap(key, false)?;
                sleep(KEY_DELAY);
            }
        }
        self.tap(KeyCode::KEY_SPACE, false)?;
        Ok(())
    }
}

fn key_event(key: KeyCode, value: i32) -> InputEvent {
    InputEvent::new(EventType::KEY.0, key.code(), value)
}

/// Publish `bytes` to the Wayland clipboard via `wl-copy`. `wl-copy`
/// forks a background server that serves the selection and the
/// foreground process exits, so `wait()` returns promptly.
fn wl_copy(bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = std::process::Command::new("wl-copy")
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("wl-copy spawn failed: {e}"))?;
    child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("wl-copy stdin unavailable"))?
        .write_all(bytes)
        .map_err(|e| anyhow::anyhow!("writing to wl-copy failed: {e}"))?;
    child
        .wait()
        .map_err(|e| anyhow::anyhow!("wl-copy wait failed: {e}"))?;
    Ok(())
}

fn which(cmd: &str) -> bool {
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            let p = std::path::Path::new(dir).join(cmd);
            if p.is_file() {
                return true;
            }
        }
    }
    false
}

fn ascii_char_to_key(c: char) -> Option<(KeyCode, bool)> {
    // Linux scancodes for letters follow the physical QWERTY layout, not
    // the alphabet — so e.g. KEY_B=48 and KEY_E=18, not KEY_A+1 / KEY_A+4.
    // Map each letter explicitly. Anything assuming alphabetic ordering
    // produces garbage like 'h' → KEY_GRAVE.
    fn letter_key(lower: char) -> KeyCode {
        match lower {
            'a' => KeyCode::KEY_A,
            'b' => KeyCode::KEY_B,
            'c' => KeyCode::KEY_C,
            'd' => KeyCode::KEY_D,
            'e' => KeyCode::KEY_E,
            'f' => KeyCode::KEY_F,
            'g' => KeyCode::KEY_G,
            'h' => KeyCode::KEY_H,
            'i' => KeyCode::KEY_I,
            'j' => KeyCode::KEY_J,
            'k' => KeyCode::KEY_K,
            'l' => KeyCode::KEY_L,
            'm' => KeyCode::KEY_M,
            'n' => KeyCode::KEY_N,
            'o' => KeyCode::KEY_O,
            'p' => KeyCode::KEY_P,
            'q' => KeyCode::KEY_Q,
            'r' => KeyCode::KEY_R,
            's' => KeyCode::KEY_S,
            't' => KeyCode::KEY_T,
            'u' => KeyCode::KEY_U,
            'v' => KeyCode::KEY_V,
            'w' => KeyCode::KEY_W,
            'x' => KeyCode::KEY_X,
            'y' => KeyCode::KEY_Y,
            'z' => KeyCode::KEY_Z,
            _ => unreachable!("letter_key called with non-lowercase-letter '{lower}'"),
        }
    }
    match c {
        'a'..='z' => Some((letter_key(c), false)),
        'A'..='Z' => Some((letter_key(c.to_ascii_lowercase()), true)),
        '0' => Some((KeyCode::KEY_0, false)),
        '1'..='9' => Some((
            KeyCode(KeyCode::KEY_1.code() + (c as u16 - b'1' as u16)),
            false,
        )),
        ' ' => Some((KeyCode::KEY_SPACE, false)),
        '\n' => Some((KeyCode::KEY_ENTER, false)),
        '\t' => Some((KeyCode::KEY_TAB, false)),
        '.' => Some((KeyCode::KEY_DOT, false)),
        ',' => Some((KeyCode::KEY_COMMA, false)),
        ';' => Some((KeyCode::KEY_SEMICOLON, false)),
        '\'' => Some((KeyCode::KEY_APOSTROPHE, false)),
        '"' => Some((KeyCode::KEY_APOSTROPHE, true)),
        '/' => Some((KeyCode::KEY_SLASH, false)),
        '?' => Some((KeyCode::KEY_SLASH, true)),
        '\\' => Some((KeyCode::KEY_BACKSLASH, false)),
        '|' => Some((KeyCode::KEY_BACKSLASH, true)),
        '-' => Some((KeyCode::KEY_MINUS, false)),
        '_' => Some((KeyCode::KEY_MINUS, true)),
        '=' => Some((KeyCode::KEY_EQUAL, false)),
        '+' => Some((KeyCode::KEY_EQUAL, true)),
        '!' => Some((KeyCode::KEY_1, true)),
        '@' => Some((KeyCode::KEY_2, true)),
        '#' => Some((KeyCode::KEY_3, true)),
        '$' => Some((KeyCode::KEY_4, true)),
        '%' => Some((KeyCode::KEY_5, true)),
        '^' => Some((KeyCode::KEY_6, true)),
        '&' => Some((KeyCode::KEY_7, true)),
        '*' => Some((KeyCode::KEY_8, true)),
        '(' => Some((KeyCode::KEY_9, true)),
        ')' => Some((KeyCode::KEY_0, true)),
        '[' => Some((KeyCode::KEY_LEFTBRACE, false)),
        ']' => Some((KeyCode::KEY_RIGHTBRACE, false)),
        '{' => Some((KeyCode::KEY_LEFTBRACE, true)),
        '}' => Some((KeyCode::KEY_RIGHTBRACE, true)),
        '`' => Some((KeyCode::KEY_GRAVE, false)),
        '~' => Some((KeyCode::KEY_GRAVE, true)),
        '<' => Some((KeyCode::KEY_COMMA, true)),
        '>' => Some((KeyCode::KEY_DOT, true)),
        ':' => Some((KeyCode::KEY_SEMICOLON, true)),
        _ => None,
    }
}

const EVERY_KEY_WE_USE: &[KeyCode] = &[
    KeyCode::KEY_A,
    KeyCode::KEY_B,
    KeyCode::KEY_C,
    KeyCode::KEY_D,
    KeyCode::KEY_E,
    KeyCode::KEY_F,
    KeyCode::KEY_G,
    KeyCode::KEY_H,
    KeyCode::KEY_I,
    KeyCode::KEY_J,
    KeyCode::KEY_K,
    KeyCode::KEY_L,
    KeyCode::KEY_M,
    KeyCode::KEY_N,
    KeyCode::KEY_O,
    KeyCode::KEY_P,
    KeyCode::KEY_Q,
    KeyCode::KEY_R,
    KeyCode::KEY_S,
    KeyCode::KEY_T,
    KeyCode::KEY_U,
    KeyCode::KEY_V,
    KeyCode::KEY_W,
    KeyCode::KEY_X,
    KeyCode::KEY_Y,
    KeyCode::KEY_Z,
    KeyCode::KEY_0,
    KeyCode::KEY_1,
    KeyCode::KEY_2,
    KeyCode::KEY_3,
    KeyCode::KEY_4,
    KeyCode::KEY_5,
    KeyCode::KEY_6,
    KeyCode::KEY_7,
    KeyCode::KEY_8,
    KeyCode::KEY_9,
    KeyCode::KEY_SPACE,
    KeyCode::KEY_ENTER,
    KeyCode::KEY_TAB,
    KeyCode::KEY_BACKSPACE,
    KeyCode::KEY_DOT,
    KeyCode::KEY_COMMA,
    KeyCode::KEY_SEMICOLON,
    KeyCode::KEY_APOSTROPHE,
    KeyCode::KEY_SLASH,
    KeyCode::KEY_BACKSLASH,
    KeyCode::KEY_MINUS,
    KeyCode::KEY_EQUAL,
    KeyCode::KEY_LEFTBRACE,
    KeyCode::KEY_RIGHTBRACE,
    KeyCode::KEY_GRAVE,
    KeyCode::KEY_LEFTSHIFT,
    KeyCode::KEY_RIGHTSHIFT,
    KeyCode::KEY_LEFTCTRL,
    KeyCode::KEY_RIGHTCTRL,
    KeyCode::KEY_LEFTALT,
    KeyCode::KEY_RIGHTALT,
];
