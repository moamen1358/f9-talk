//! `f9-talk` binary entry point.
//!
//! Hold F9, speak, release: the transcript is typed at the cursor. That's
//! the whole tool. Speech-to-text is AssemblyAI Universal-3.6 Pro Realtime
//! by default, Deepgram Nova-3 by one line in `config.toml`.
//!
//! Threading:
//! - **Main thread**: drives the indicator — a Wayland `wlr-layer-shell`
//!   overlay (on its own thread) plus a Ctrl-C wait, or the eframe window
//!   on X11 / macOS / Windows.
//! - **Tokio runtime**: STT WebSocket client, hotkey listener, mic frame
//!   router, session loop, wake-from-suspend watcher.
//! - **cpal callback thread**: real-time, owned by cpal; pushes 25 ms
//!   frames + RMS into the shared `IndicatorState`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use f9_talk_input::{typer_preflight, HotkeyEvent, Typer};

mod config;
mod install;
mod simulate;
use config::{Backend, Settings};
use f9_talk_stt::{BackendEvent, Stt};
use f9_talk_ui::{IndicatorApp, IndicatorState};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// The hotkey is fixed: hold F9 to dictate.
const HOTKEY: &str = "f9";

/// After the indicator hides on release, give the compositor this long to
/// hand keyboard focus back to the user's app before anything is typed.
const FOCUS_SETTLE: Duration = Duration::from_millis(100);

#[derive(Parser, Debug, Clone)]
#[command(
    name = "f9-talk",
    version,
    about = "Hold F9 to dictate (AssemblyAI Universal-3.6 Pro or Deepgram Nova-3)"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Subcommand>,

    /// Pixels the Wayland indicator sits above the bottom screen edge.
    /// Raise it to clear an app's own bottom bar.
    #[arg(long, default_value_t = 20)]
    indicator_margin: i32,

    #[arg(short, long)]
    verbose: bool,
}

#[derive(clap::Subcommand, Debug, Clone)]
enum Subcommand {
    /// Set up desktop integration: apps menu entry, autostart, udev rule, secrets stub.
    Install(install::InstallArgs),
    /// Remove what `install` set up (keeps your secrets.env in place).
    Uninstall(install::InstallArgs),
    /// Stream a 16 kHz mono WAV through the configured backend at real-time
    /// pace, as if F9 were held for its length, and print what would be typed.
    #[command(hide = true)]
    Simulate(simulate::SimulateArgs),
}

fn main() -> anyhow::Result<()> {
    init_tracing(parse_verbose());
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cli = Cli::parse();
    if cli.verbose {
        debug!("CLI: {cli:?}");
    }

    // Subcommands (install / uninstall) run before any of the dictation
    // runtime is set up — they're pure filesystem work.
    match cli.command.as_ref() {
        Some(Subcommand::Install(args)) => return install::run(args),
        Some(Subcommand::Uninstall(args)) => return install::uninstall(args),
        Some(Subcommand::Simulate(args)) => return simulate::run(args),
        None => {}
    }

    let _lock = match acquire_instance_lock() {
        Ok(lock) => lock,
        Err(_) => {
            eprintln!("f9-talk is already running.");
            std::process::exit(0);
        }
    };

    if let Err(e) = typer_preflight() {
        eprintln!("\nf9-talk: {e}\n");
        std::process::exit(2);
    }

    let secrets = load_secrets();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("f9-talk")
        .build()?;

    // Mic streamer must spawn inside a runtime context.
    let _guard = runtime.enter();
    let (frame_rx, rms_handle, _mic_task) =
        f9_talk_audio::spawn().map_err(|e| anyhow::anyhow!("could not start mic streamer: {e}"))?;
    drop(_guard);

    let indicator_state = Arc::new(IndicatorState::new(rms_handle));

    let secrets_for_task = secrets.clone();
    let state_for_task = indicator_state.clone();
    runtime.spawn(async move {
        if let Err(e) = run_session_loop(secrets_for_task, frame_rx, state_for_task).await {
            tracing::error!("session loop error: {e}");
        }
    });

    // Indicator. On Wayland it's a native wlr-layer-shell overlay on its
    // own thread (bottom-center, borderless, real transparency); the main
    // thread then just waits for Ctrl-C. On X11 / macOS / Windows the
    // eframe window draws the wave on the main thread.
    let on_wayland = std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var("XDG_SESSION_TYPE")
            .map(|v| v.eq_ignore_ascii_case("wayland"))
            .unwrap_or(false);

    #[cfg(target_os = "linux")]
    if on_wayland {
        let _indicator =
            f9_talk_ui::layer_indicator::spawn(indicator_state.clone(), cli.indicator_margin);
        info!("Wayland: wlr-layer-shell indicator (hold F9 to dictate, Ctrl-C to quit)");
        runtime.block_on(async {
            tokio::signal::ctrl_c().await.ok();
        });
        info!("shutting down");
        runtime.shutdown_timeout(Duration::from_secs(2));
        return Ok(());
    }

    run_eframe_indicator(indicator_state, runtime)
}

/// Drive the eframe wave indicator (X11 / macOS / Windows) on the main
/// thread. Blocks until the window closes.
fn run_eframe_indicator(
    indicator_state: Arc<IndicatorState>,
    runtime: tokio::runtime::Runtime,
) -> anyhow::Result<()> {
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("f9-talk")
        .with_app_id("f9-talk")
        .with_inner_size([320.0, 22.0])
        .with_decorations(false)
        .with_transparent(true)
        .with_always_on_top()
        .with_resizable(false)
        .with_taskbar(false)
        .with_mouse_passthrough(true)
        .with_active(false);
    #[cfg(target_os = "linux")]
    {
        viewport = viewport.with_window_type(egui::X11WindowType::Notification);
    }
    // Start hidden — IndicatorApp toggles visibility while F9 is held.
    viewport = viewport.with_visible(false);
    if let Ok(pos) = f9_talk_ui::Positioner::new() {
        if let Some((x, y)) = pos.compute_position(f9_talk_ui::INDICATOR_W, f9_talk_ui::INDICATOR_H)
        {
            viewport = viewport.with_position([x as f32, y as f32]);
        }
    }
    let native_options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "f9-talk",
        native_options,
        Box::new(move |_cc| Ok(Box::new(IndicatorApp::new(indicator_state)))),
    )
    .map_err(|e| anyhow::anyhow!("eframe error: {e}"))?;

    info!("indicator closed; shutting down");
    runtime.shutdown_timeout(Duration::from_secs(2));
    Ok(())
}

fn parse_verbose() -> bool {
    std::env::args().any(|a| a == "-v" || a == "--verbose")
}

fn init_tracing(verbose: bool) {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new(if verbose { "debug" } else { "info" })
    });
    let registry = tracing_subscriber::registry().with(env_filter);

    #[cfg(target_os = "linux")]
    {
        match tracing_journald::layer() {
            Ok(journald) => {
                registry
                    .with(journald)
                    .with(tracing_subscriber::fmt::layer().with_target(false))
                    .init();
                return;
            }
            Err(e) => {
                eprintln!("journald layer unavailable ({e}); logging to stderr only");
            }
        }
    }

    // Fallback (non-Linux, or journald unavailable on Linux).
    #[allow(unreachable_code)]
    {
        registry
            .with(tracing_subscriber::fmt::layer().with_target(false))
            .init();
    }
}

// ── Instance lock ──────────────────────────────────────────────────
// Linux: abstract Unix socket. macOS / Windows: advisory lock file.

#[cfg(target_os = "linux")]
fn acquire_instance_lock() -> anyhow::Result<Box<dyn std::any::Any>> {
    use std::os::unix::net::UnixDatagram;
    const INSTANCE_LOCK_NAME: &[u8] = b"\0f9-talk-instance-lock";

    let socket = UnixDatagram::unbound()?;
    bind_abstract(&socket, INSTANCE_LOCK_NAME)?;
    Ok(Box::new(socket))
}

#[cfg(target_os = "linux")]
fn bind_abstract(sock: &std::os::unix::net::UnixDatagram, name: &[u8]) -> anyhow::Result<()> {
    use std::os::fd::AsRawFd;
    if name.len() > 107 {
        anyhow::bail!("abstract socket name too long: {} bytes", name.len());
    }
    let fd = sock.as_raw_fd();
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (i, b) in name.iter().enumerate() {
        addr.sun_path[i] = *b as libc::c_char;
    }
    let addrlen = (std::mem::size_of::<libc::sa_family_t>() + name.len()) as libc::socklen_t;
    let rc = unsafe { libc::bind(fd, &addr as *const _ as *const libc::sockaddr, addrlen) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        anyhow::bail!("bind on abstract socket failed: {err}");
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn acquire_instance_lock() -> anyhow::Result<Box<dyn std::any::Any>> {
    let lock_dir = dirs::config_dir()
        .ok_or_else(|| anyhow::anyhow!("could not determine config directory"))?;
    let lock_path = lock_dir.join("F9_talk").join(".instance.lock");
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&lock_path)?;
    use std::io::Write;
    let mut f = file;
    writeln!(f, "{}", std::process::id())?;
    Ok(Box::new(f))
}

/// API keys from the environment, then `secrets.env` (first occurrence
/// wins). The installer's `PASTE_...` placeholders count as no key.
pub(crate) fn load_secrets() -> HashMap<String, String> {
    let mut out = HashMap::new();
    for var in ["ASSEMBLYAI_API_KEY", "DEEPGRAM_API_KEY"] {
        if let Ok(v) = std::env::var(var) {
            if is_real_key(&v) {
                out.insert(var.to_string(), v);
            }
        }
    }
    if let Some(path) = secrets_path() {
        if let Ok(text) = std::fs::read_to_string(&path) {
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                if let Some((k, v)) = line.split_once('=') {
                    let k = k.trim().to_string();
                    let v = v.trim().trim_matches('"').trim_matches('\'').to_string();
                    if is_real_key(&v) {
                        out.entry(k).or_insert(v);
                    }
                }
            }
        }
    }
    out
}

fn secrets_path() -> Option<PathBuf> {
    let config = dirs::config_dir()?;
    Some(config.join("F9_talk").join("secrets.env"))
}

/// The text that gets typed. AssemblyAI's formatter writes em dashes
/// ("the dress of the\u{2014} after"), which dictated plain text rarely
/// wants, so each em or en dash becomes a plain hyphen: " - " between
/// words, "-" inside a number range ("10\u{2013}12" becomes "10-12").
pub(crate) fn tidy_transcript(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    for (i, &c) in chars.iter().enumerate() {
        if c != '\u{2014}' && c != '\u{2013}' {
            out.push(c);
            continue;
        }
        let prev = out.chars().last();
        let next = chars.get(i + 1).copied();
        if prev.is_some_and(|p| p.is_ascii_digit()) && next.is_some_and(|n| n.is_ascii_digit()) {
            out.push('-');
            continue;
        }
        while out.ends_with(' ') {
            out.pop();
        }
        if out.is_empty() {
            continue;
        }
        out.push_str(" -");
        if next.is_some_and(|n| n != ' ') {
            out.push(' ');
        }
    }
    out.trim().to_string()
}

fn is_real_key(v: &str) -> bool {
    !v.is_empty() && !v.starts_with("PASTE_")
}

/// Build the backend `settings` asks for. When its key is missing but the
/// other backend's key is there, use the other one (and say so) rather
/// than not dictating at all.
pub(crate) fn build_backend(
    settings: &Settings,
    secrets: &HashMap<String, String>,
    keyterms: Vec<String>,
) -> anyhow::Result<(Arc<dyn Stt>, Backend)> {
    let mut choice = settings.backend;
    if !secrets.contains_key(choice.key_var()) {
        let other = choice.other();
        if secrets.contains_key(other.key_var()) {
            warn!(
                "config asks for {} but {} is not set; using {} instead",
                choice.label(),
                choice.key_var(),
                other.label()
            );
            choice = other;
        } else {
            anyhow::bail!(
                "needs {} set in the environment or in ~/.config/F9_talk/secrets.env",
                choice.key_var()
            );
        }
    }
    let key = secrets[choice.key_var()].clone();
    let backend: Arc<dyn Stt> = match choice {
        Backend::AssemblyAi => Arc::new(f9_talk_stt::assemblyai::AssemblyAi::new(
            key,
            f9_talk_stt::assemblyai::Config {
                keyterms,
                warm_secs: settings.assemblyai_warm_seconds,
                ..Default::default()
            },
        )),
        Backend::Deepgram => Arc::new(f9_talk_stt::deepgram::Deepgram::new(
            key,
            f9_talk_stt::deepgram::Config {
                keyterms,
                ..Default::default()
            },
        )),
    };
    Ok((backend, choice))
}

async fn run_session_loop(
    secrets: HashMap<String, String>,
    mut frame_rx: mpsc::Receiver<f9_talk_audio::Frame>,
    indicator: Arc<IndicatorState>,
) -> anyhow::Result<()> {
    let dir = config::config_dir();
    if let Some(dir) = dir.as_deref() {
        match config::seed_user_files(dir) {
            Ok(created) => {
                for path in created {
                    info!("wrote default {}", path.display());
                }
            }
            Err(e) => warn!(
                "could not write default settings into {}: {e}",
                dir.display()
            ),
        }
    }
    let settings = config::load_settings(dir.as_deref());
    let keyterms = config::load_keyterms(dir.as_deref());
    let (backend, choice) = match build_backend(&settings, &secrets, keyterms.clone()) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("f9-talk: {e}");
            std::process::exit(2);
        }
    };
    let (event_tx, mut event_rx) = mpsc::channel::<BackendEvent>(64);
    backend
        .start(event_tx)
        .await
        .map_err(|e| anyhow::anyhow!("could not start the {} backend: {e}", choice.label()))?;
    info!(
        "{} backend ready ({} key terms, finalize safety net {:?})",
        choice.label(),
        keyterms.len(),
        settings.finalize_timeout()
    );

    let mut hotkey_rx = match f9_talk_input::spawn_hotkey(HOTKEY) {
        Ok(rx) => rx,
        Err(e) => {
            eprintln!(
                "f9-talk: could not start hotkey listener: {e}\n\
                 Are you a member of the `input` group? Run:\n\
                 \tsudo usermod -aG input $USER\n\
                 then log out and back in once."
            );
            std::process::exit(2);
        }
    };

    info!("f9-talk ready. hold F9 to dictate (Ctrl-C to quit)");
    let mut typer = Typer::new()?;
    spawn_wakeup_watcher();

    let mut session: Option<SessionInProgress> = None;

    loop {
        tokio::select! {
            evt = hotkey_rx.recv() => {
                match evt {
                    Some(HotkeyEvent::Pressed) => {
                        let press_at = Instant::now();
                        backend.begin_session().await;
                        indicator.set_recording(true);
                        indicator.set_status_text(None);
                        info!("🎙  recording…");
                        session = Some(SessionInProgress {
                            press_at,
                            first_byte_sent: None,
                            frames_sent: 0,
                        });
                    }
                    Some(HotkeyEvent::Released) => {
                        let Some(mut sess) = session.take() else { continue; };
                        let release_at = Instant::now();
                        // Frames captured before the key-up can still be
                        // queued in the mic channel (select! may pick the
                        // hotkey branch first): forward them, or the tail
                        // of the last word is cut.
                        while let Ok(f) = frame_rx.try_recv() {
                            sess.frames_sent += 1;
                            backend.send_audio(&f.bytes).await;
                        }
                        // Hide the indicator first so the compositor returns
                        // keyboard focus to the user's app before the typer's
                        // keys (or paste) land. The finalize request goes out
                        // at once; the focus hand-back overlaps the wait.
                        indicator.set_recording(false);
                        indicator.set_status_text(None);
                        let result = backend.end_session(settings.finalize_timeout()).await;
                        let final_at = Instant::now();
                        let settled = release_at.elapsed();
                        if settled < FOCUS_SETTLE {
                            tokio::time::sleep(FOCUS_SETTLE - settled).await;
                        }
                        info!(
                            target: "f9_talk::press",
                            "backend={} press_to_release={:.0?} frames={} first_byte_sent={:?} release_to_final={:.0?} transcript={:?}",
                            backend.name(),
                            release_at.duration_since(sess.press_at),
                            sess.frames_sent,
                            sess.first_byte_sent.map(|t| t.duration_since(sess.press_at)),
                            final_at.duration_since(release_at),
                            result.transcript,
                        );
                        let text = tidy_transcript(&result.transcript);
                        if text.is_empty() {
                            info!("(no speech detected)");
                        } else if let Err(e) = typer.type_text(&text) {
                            warn!("typer failed: {e}");
                        }
                    }
                    None => {
                        warn!("hotkey channel closed; exiting");
                        backend.stop().await;
                        return Ok(());
                    }
                }
            }
            frame = frame_rx.recv() => {
                let Some(f) = frame else {
                    warn!("mic channel closed; exiting");
                    backend.stop().await;
                    return Ok(());
                };
                if let Some(sess) = session.as_mut() {
                    if sess.first_byte_sent.is_none() {
                        sess.first_byte_sent = Some(Instant::now());
                    }
                    sess.frames_sent += 1;
                    backend.send_audio(&f.bytes).await;
                }
            }
            evt = event_rx.recv() => {
                match evt {
                    Some(BackendEvent::SocketLost(msg)) => warn!("STT socket lost: {msg}"),
                    Some(BackendEvent::SocketBack) => info!("STT socket reconnected"),
                    Some(BackendEvent::Error(e)) => warn!("STT error: {e}"),
                    None => {}
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("Ctrl-C received; shutting down");
                backend.stop().await;
                return Ok(());
            }
        }
    }
}

struct SessionInProgress {
    press_at: Instant,
    first_byte_sent: Option<Instant>,
    frames_sent: u32,
}

fn spawn_wakeup_watcher() {
    tokio::spawn(async move {
        let mut last = Instant::now();
        let interval = Duration::from_secs(5);
        let threshold = Duration::from_secs(30);
        loop {
            tokio::time::sleep(interval).await;
            let now = Instant::now();
            let drift = now.duration_since(last);
            if drift > threshold {
                warn!(
                    "WakeUp event: clock advanced {:.0?} (>{:.0?} threshold). \
                    Long-lived connections should reconnect.",
                    drift, threshold
                );
            }
            last = now;
        }
    });
}

#[cfg(target_os = "linux")]
extern crate libc;

#[cfg(test)]
mod tests {
    use super::tidy_transcript;

    #[test]
    fn em_and_en_dashes_become_hyphens() {
        assert_eq!(
            tidy_transcript("the dress of the\u{2014} after his opening"),
            "the dress of the - after his opening"
        );
        assert_eq!(tidy_transcript("a \u{2014} b"), "a - b");
        assert_eq!(tidy_transcript("a\u{2014}b"), "a - b");
        assert_eq!(tidy_transcript("pages 10\u{2013}12"), "pages 10-12");
        assert_eq!(tidy_transcript("wait \u{2013} no"), "wait - no");
        assert_eq!(tidy_transcript("\u{2014}Hello"), "Hello");
        assert_eq!(
            tidy_transcript("Plain text, untouched."),
            "Plain text, untouched."
        );
        assert_eq!(tidy_transcript(""), "");
    }
}
