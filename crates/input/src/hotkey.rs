//! Chord parser + push-to-talk hotkey listener with auto-repeat debounce.
//!
//! Translates the Python chord syntax we keep for backward compatibility
//! (`"f9"`, `"<ctrl>+<alt>+space"`, `"ctrl+shift+a"`) into the form
//! [`hotkey_listener::parse_hotkey`] accepts (capitalised tokens, e.g.
//! `"Ctrl+Alt+Space"`).
//!
//! The 50 ms X11 auto-repeat debounce that Python's `app.py:_on_release`
//! does is layered on top of the crate's pressed/released stream so a
//! re-press within 50 ms of a release cancels the pending end-session.
//!
//! Threading model:
//! - A dedicated `spawn_blocking` task owns the [`HotkeyListenerHandle`]
//!   and pumps `recv_timeout()` events into an unbounded tokio mpsc.
//! - A second tokio task applies the debounce and forwards clean
//!   [`HotkeyEvent`] values to the consumer.

use std::time::{Duration, Instant};

use hotkey_listener::{parse_hotkey, HotkeyEvent as RawEvent, HotkeyListenerBuilder};
use tokio::sync::mpsc;
use tracing::{debug, info};

/// Default X11 auto-repeat debounce. Re-press within this window cancels a
/// pending Release so a held key isn't truncated by the OS auto-repeat.
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(50);

/// Final, debounced press/release event the rest of the app consumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEvent {
    Pressed,
    Released,
}

/// Translate Python-style chord strings into the form `parse_hotkey` accepts.
///
/// Accepts `"f9"`, `"<ctrl>+<alt>+space"`, `"ctrl+shift+a"`, etc.
fn normalize_chord(spec: &str) -> String {
    spec.split('+')
        .map(|tok| {
            let stripped = tok.trim().trim_start_matches('<').trim_end_matches('>');
            let mut chars = stripped.chars();
            match chars.next() {
                Some(c) => c
                    .to_ascii_uppercase()
                    .to_string()
                    .chars()
                    .chain(chars.flat_map(|c| c.to_lowercase()))
                    .collect::<String>(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join("+")
}

/// Spawn a hotkey listener with the default 50 ms debounce.
pub fn spawn(chord: &str) -> anyhow::Result<mpsc::Receiver<HotkeyEvent>> {
    spawn_with_debounce(chord, DEFAULT_DEBOUNCE)
}

/// Variant with a configurable debounce window.
pub fn spawn_with_debounce(
    chord: &str,
    debounce: Duration,
) -> anyhow::Result<mpsc::Receiver<HotkeyEvent>> {
    let normalized = normalize_chord(chord);
    let parsed = parse_hotkey(&normalized).map_err(|e| {
        anyhow::anyhow!("could not parse hotkey {chord:?} (normalized to {normalized:?}): {e}")
    })?;

    let handle = HotkeyListenerBuilder::new()
        .add_hotkey(parsed)
        .build()?
        .start()?;

    let (raw_tx, mut raw_rx) = mpsc::unbounded_channel::<RawEvent>();
    let (out_tx, out_rx) = mpsc::channel::<HotkeyEvent>(8);

    // Pump raw events on a blocking thread so the runtime stays responsive.
    tokio::task::spawn_blocking(move || {
        loop {
            match handle.recv_timeout(Duration::from_millis(100)) {
                Ok(evt) => {
                    if raw_tx.send(evt).is_err() {
                        return; // consumer hung up
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if raw_tx.is_closed() {
                        return;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
    });

    // Debounce task: collapse Press→Release→Press(<50 ms) into a single Press.
    tokio::spawn(async move {
        let mut pending_release: Option<Instant> = None;
        let mut press_sent = false;
        loop {
            let timeout = match pending_release {
                Some(_) => Duration::from_millis(10),
                None => Duration::from_millis(100),
            };
            let raw = tokio::time::timeout(timeout, raw_rx.recv()).await;
            match raw {
                Ok(Some(RawEvent::Pressed(_))) => {
                    if pending_release.take().is_some() {
                        debug!("hotkey re-press within debounce window - cancelling release");
                        continue;
                    }
                    if press_sent {
                        continue;
                    }
                    press_sent = true;
                    if out_tx.send(HotkeyEvent::Pressed).await.is_err() {
                        return;
                    }
                }
                Ok(Some(RawEvent::Released(_))) => {
                    if !press_sent {
                        continue;
                    }
                    pending_release = Some(Instant::now());
                }
                Ok(None) => return,
                Err(_) => { /* timeout - fall through to debounce check */ }
            }

            if let Some(t) = pending_release {
                if t.elapsed() >= debounce {
                    pending_release = None;
                    press_sent = false;
                    if out_tx.send(HotkeyEvent::Released).await.is_err() {
                        return;
                    }
                }
            }
        }
    });

    info!("hotkey listener armed: {chord:?} (normalized {normalized:?})");
    Ok(out_rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_handles_python_syntax() {
        assert_eq!(normalize_chord("f9"), "F9");
        assert_eq!(normalize_chord("<ctrl>+<alt>+space"), "Ctrl+Alt+Space");
        assert_eq!(normalize_chord("ctrl+shift+a"), "Ctrl+Shift+A");
        assert_eq!(normalize_chord(" F8 "), "F8");
    }

    #[test]
    fn normalize_idempotent_on_canonical_input() {
        assert_eq!(normalize_chord("F9"), "F9");
        assert_eq!(normalize_chord("Ctrl+Alt+Space"), "Ctrl+Alt+Space");
    }
}
