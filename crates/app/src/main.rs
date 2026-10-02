//! `f9-talk` binary entry point.
//!
//! Hold F9, speak, release: the transcript is typed at the cursor. That's
//! the whole tool. Speech-to-text is AssemblyAI Universal-3.6 Pro Realtime
//! by default, Deepgram Nova-3 by one line in `config.toml`.
//!
//! Threading:
//! - **Main thread**: drives the indicator - a Wayland `wlr-layer-shell`
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
mod ipc;
mod keycheck;
mod keys;
mod settings_ui;
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
    /// Open the Settings window: service, API keys, language, key terms.
    Settings(settings_ui::SettingsArgs),
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
    // runtime is set up - they're pure filesystem work.
    match cli.command.as_ref() {
        Some(Subcommand::Install(args)) => return install::run(args),
        Some(Subcommand::Uninstall(args)) => return install::uninstall(args),
        Some(Subcommand::Simulate(args)) => return simulate::run(args),
        Some(Subcommand::Settings(args)) => return settings_ui::run(args),
        None => {}
    }

    let lock = match ipc::acquire_instance_lock() {
        Ok(lock) => lock,
        Err(_) => {
            // Launching it again (apps menu, autostart) while it runs
            // opens the Settings window instead.
            eprintln!("f9-talk is already running; opening Settings.");
            return settings_ui::run(&settings_ui::SettingsArgs::default());
        }
    };

    if let Err(e) = typer_preflight() {
        eprintln!("\nf9-talk: {e}\n");
        std::process::exit(2);
    }

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
    // Right-clicking the red dot opens Settings.
    indicator_state.set_on_open_settings(Arc::new(|| open_settings_window(false)));

    // Save in the Settings window sends "reload" to the lock socket.
    let (control_tx, control_rx) = mpsc::channel::<ipc::Control>(8);
    ipc::spawn_listener(&lock, control_tx);

    let state_for_task = indicator_state.clone();
    runtime.spawn(async move {
        if let Err(e) = run_session_loop(frame_rx, state_for_task, control_rx).await {
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
    // Start hidden - IndicatorApp toggles visibility while F9 is held.
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
                // A fixed tag, so `journalctl --user -t f9-talk` works the
                // same for the AppImage (whose process is named
                // `f9-talk.AppImage`) as for a plain binary.
                let journald = journald.with_syslog_identifier("f9-talk".into());
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

/// API keys for both services, by variable name: environment, then the
/// desktop keyring, then `secrets.env` (see `keys`).
pub(crate) fn load_secrets() -> HashMap<String, String> {
    let store = keys::KeyStore::new(config::config_dir());
    let mut out = HashMap::new();
    for b in [Backend::AssemblyAi, Backend::Deepgram] {
        if let Some((key, source)) = store.get(b) {
            debug!("{} found ({})", b.key_var(), source.describe());
            out.insert(b.key_var().to_string(), key);
        }
    }
    out
}

/// Start another copy of this program (the AppImage when running from
/// one), detached, with `args`; e.g. `["settings"]`.
pub(crate) fn launch_self(args: &[&str]) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;
    let exe = std::env::var_os("APPIMAGE")
        .map(PathBuf::from)
        .map_or_else(std::env::current_exe, Ok)?;
    let mut child = std::process::Command::new(exe)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()?;
    // Reap it when it exits so no zombie is left behind.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

fn open_settings_window(first_run: bool) {
    let args: &[&str] = if first_run {
        &["settings", "--first-run"]
    } else {
        &["settings"]
    };
    if let Err(e) = launch_self(args) {
        warn!("could not open the Settings window: {e}");
    }
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
                "no API key yet: add one in Settings (right-click the red dot), \
                 or set {} in the environment",
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
                language: settings.language.clone(),
                warm_secs: settings.assemblyai_warm_seconds,
                ..Default::default()
            },
        )),
        Backend::Deepgram => Arc::new(f9_talk_stt::deepgram::Deepgram::new(
            key,
            f9_talk_stt::deepgram::Config {
                keyterms,
                language: settings.language.clone(),
                ..Default::default()
            },
        )),
    };
    Ok((backend, choice))
}

/// The live backend and the settings it was built from.
struct Active {
    backend: Arc<dyn Stt>,
    settings: Settings,
}

/// Read settings and keys and start the chosen backend. `None` when no
/// key is set yet (the caller opens Settings).
async fn start_backend(events: &mpsc::Sender<BackendEvent>) -> Option<Active> {
    let dir = config::config_dir();
    let settings = config::load_settings(dir.as_deref());
    let keyterms = config::load_keyterms(dir.as_deref());
    let secrets = load_secrets();
    let (backend, choice) = match build_backend(&settings, &secrets, keyterms.clone()) {
        Ok(b) => b,
        Err(e) => {
            warn!("{e}");
            return None;
        }
    };
    if let Err(e) = backend.start(events.clone()).await {
        warn!("could not start the {} backend: {e}", choice.label());
        return None;
    }
    info!(
        "{} backend ready (language {}, {} key terms, finalize safety net {:?})",
        choice.label(),
        settings.language,
        keyterms.len(),
        settings.finalize_timeout()
    );
    Some(Active { backend, settings })
}

/// Settings were saved: stop the old backend, start one from the new
/// settings and keys.
async fn reload(active: &mut Option<Active>, events: &mpsc::Sender<BackendEvent>) {
    info!("settings changed; reloading the speech-to-text backend");
    if let Some(old) = active.take() {
        old.backend.stop().await;
    }
    *active = start_backend(events).await;
}

async fn run_session_loop(
    mut frame_rx: mpsc::Receiver<f9_talk_audio::Frame>,
    indicator: Arc<IndicatorState>,
    mut control_rx: mpsc::Receiver<ipc::Control>,
) -> anyhow::Result<()> {
    if let Some(dir) = config::config_dir() {
        match config::seed_user_files(&dir) {
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
    let (event_tx, mut event_rx) = mpsc::channel::<BackendEvent>(64);
    let mut gate = settings_ui::OpenGate::new();
    let mut active = start_backend(&event_tx).await;
    if active.is_none() && gate.allow() {
        info!("no API key yet; opening Settings");
        open_settings_window(true);
    }

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
    let mut reload_pending = false;

    loop {
        tokio::select! {
            evt = hotkey_rx.recv() => {
                match evt {
                    Some(HotkeyEvent::Pressed) => {
                        let Some(act) = active.as_ref() else {
                            if gate.allow() {
                                info!("F9 pressed but no API key is set; opening Settings");
                                open_settings_window(true);
                            }
                            continue;
                        };
                        let press_at = Instant::now();
                        act.backend.begin_session().await;
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
                        let Some(act) = active.as_ref() else { continue; };
                        let backend = &act.backend;
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
                        let result = backend.end_session(act.settings.finalize_timeout()).await;
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
                        if std::mem::take(&mut reload_pending) {
                            reload(&mut active, &event_tx).await;
                        }
                    }
                    None => {
                        warn!("hotkey channel closed; exiting");
                        if let Some(act) = active.take() {
                            act.backend.stop().await;
                        }
                        return Ok(());
                    }
                }
            }
            frame = frame_rx.recv() => {
                let Some(f) = frame else {
                    warn!("mic channel closed; exiting");
                    if let Some(act) = active.take() {
                        act.backend.stop().await;
                    }
                    return Ok(());
                };
                if let (Some(sess), Some(act)) = (session.as_mut(), active.as_ref()) {
                    if sess.first_byte_sent.is_none() {
                        sess.first_byte_sent = Some(Instant::now());
                    }
                    sess.frames_sent += 1;
                    act.backend.send_audio(&f.bytes).await;
                }
            }
            ctl = control_rx.recv() => {
                if ctl == Some(ipc::Control::Reload) {
                    if session.is_some() {
                        // Never swap backends under a held F9: the press
                        // finishes on the old one, then this reload runs.
                        info!("settings changed during a dictation; reloading after it");
                        reload_pending = true;
                    } else {
                        reload(&mut active, &event_tx).await;
                    }
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
                if let Some(act) = active.take() {
                    act.backend.stop().await;
                }
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
