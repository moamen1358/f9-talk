//! `f9-talk` binary entry point.
//!
//! Hold F9, speak, release — the Deepgram Nova-3 transcript is typed at
//! the cursor. That's the whole tool.
//!
//! Threading:
//! - **Main thread**: drives the indicator — a Wayland `wlr-layer-shell`
//!   overlay (on its own thread) plus a Ctrl-C wait, or the eframe window
//!   on X11 / macOS / Windows.
//! - **Tokio runtime**: Deepgram WS client, hotkey listener, mic frame
//!   router, session loop, wake-from-suspend watcher.
//! - **cpal callback thread**: real-time, owned by cpal; pushes 25 ms
//!   frames + RMS into the shared `IndicatorState`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use f9_talk_input::{typer_preflight, HotkeyEvent, Typer};

mod install;
use f9_talk_stt::{BackendEvent, Stt};
use f9_talk_ui::{IndicatorApp, IndicatorState};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// The hotkey is fixed: hold F9 to dictate.
const HOTKEY: &str = "f9";

#[derive(Parser, Debug, Clone)]
#[command(
    name = "f9-talk",
    version,
    about = "Hold F9 to dictate (Deepgram Nova-3)"
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

fn load_secrets() -> HashMap<String, String> {
    let mut out = HashMap::new();
    if let Ok(v) = std::env::var("DEEPGRAM_API_KEY") {
        if !v.is_empty() {
            out.insert("DEEPGRAM_API_KEY".to_string(), v);
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
                    out.entry(k).or_insert(v);
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

async fn build_cloud_backend(secrets: &HashMap<String, String>) -> anyhow::Result<Arc<dyn Stt>> {
    let key = secrets.get("DEEPGRAM_API_KEY").cloned().unwrap_or_default();
    Ok(Arc::new(f9_talk_stt::deepgram::Deepgram::new(
        key,
        f9_talk_stt::deepgram::Config::default(),
    )))
}

async fn run_session_loop(
    secrets: HashMap<String, String>,
    mut frame_rx: mpsc::Receiver<f9_talk_audio::Frame>,
    indicator: Arc<IndicatorState>,
) -> anyhow::Result<()> {
    if !secrets.contains_key("DEEPGRAM_API_KEY") {
        eprintln!(
            "f9-talk: needs DEEPGRAM_API_KEY set in the environment or in \
             ~/.config/F9_talk/secrets.env"
        );
        std::process::exit(2);
    }

    let backend = build_cloud_backend(&secrets).await?;
    let (event_tx, mut event_rx) = mpsc::channel::<BackendEvent>(64);
    backend
        .start(event_tx)
        .await
        .map_err(|e| anyhow::anyhow!("could not start Deepgram backend: {e}"))?;
    info!("Deepgram backend ready");

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
                        let Some(sess) = session.take() else { continue; };
                        let release_at = Instant::now();
                        // Hide the indicator first so the compositor returns
                        // keyboard focus to the user's app before the typer's
                        // keys (or paste) land.
                        indicator.set_recording(false);
                        indicator.set_status_text(None);
                        std::thread::sleep(Duration::from_millis(100));
                        let result = backend.end_session(Duration::from_millis(350)).await;
                        let final_at = Instant::now();
                        info!(
                            target: "f9_talk::press",
                            "press_to_release={:.0?} frames={} first_byte_sent={:?} release_to_final={:.0?} transcript={:?}",
                            release_at.duration_since(sess.press_at),
                            sess.frames_sent,
                            sess.first_byte_sent.map(|t| t.duration_since(sess.press_at)),
                            final_at.duration_since(release_at),
                            result.transcript,
                        );
                        if result.transcript.is_empty() {
                            info!("(no speech detected)");
                        } else if let Err(e) = typer.type_text(&result.transcript) {
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
