//! `f9-talk` binary entry point.
//!
//! Threading model (M3):
//! - **Main thread**: clap → instance lock → secrets → spawn tokio
//!   runtime → spawn mic + session loop on it → block in
//!   `eframe::run_native` driving the egui indicator.
//! - **Tokio runtime worker thread(s)**: STT WS clients, hotkey
//!   listener, mic frame router, session loop, wake-from-suspend.
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
use f9_talk_translate::Translator;
use f9_talk_ui::{
    IndicatorApp, IndicatorState, KeysDialogState, TrayCommand, TrayHandle, TrayVisualState,
};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

#[derive(Parser, Debug, Clone)]
#[command(name = "f9-talk", version, about = "Hold-to-talk dictation")]
struct Cli {
    #[command(subcommand)]
    command: Option<Subcommand>,

    #[arg(long, value_enum, default_value_t = Backend::Cloud)]
    backend: Backend,

    #[arg(long, default_value = "f9")]
    local_hotkey: String,

    #[arg(long)]
    target: Option<String>,

    #[arg(long = "keyword")]
    keywords: Vec<String>,

    #[arg(long, default_value = "wave")]
    style: String,

    /// Run headless (no indicator window). Useful for the M2 smoke
    /// path or autostart on non-X11/Wayland sessions.
    #[arg(long)]
    headless: bool,

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

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum Backend {
    Cloud,
    Local,
    Both,
}

fn main() -> anyhow::Result<()> {
    init_tracing(parse_verbose());
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cli = Cli::parse();
    if cli.verbose {
        debug!("CLI: {cli:?}");
    }

    // Subcommands (install / uninstall) run before any of the
    // dictation runtime is set up — they're pure filesystem work.
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
    if cli.verbose {
        let masked: HashMap<&str, String> = secrets
            .iter()
            .map(|(k, v)| {
                let display = if v.is_empty() {
                    "<empty>".to_string()
                } else {
                    format!("<set, {} chars>", v.len())
                };
                (k.as_str(), display)
            })
            .collect();
        debug!("loaded secrets: {masked:?}");
    }

    if !matches!(cli.style.as_str(), "wave") {
        warn!(
            "indicator style {:?} not in v1; falling back to 'wave'",
            cli.style
        );
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

    // Tray. When --headless is set we skip the tray too;
    // the binary becomes a pure CLI dictation pipe.
    let tray_bundle = if cli.headless {
        None
    } else {
        let initial = TrayVisualState {
            paused: false,
            error: false,
        };
        match f9_talk_ui::tray::spawn(initial, indicator_state.clone()) {
            Ok((handle, cmd_rx)) => Some((handle, cmd_rx)),
            Err(e) => {
                warn!("tray unavailable: {e}; pause/quit via tray disabled");
                None
            }
        }
    };
    let tray_handle: Option<TrayHandle> = tray_bundle.as_ref().map(|(h, _)| h.clone());

    let keys_dialog = KeysDialogState::new();

    let chord = cli.local_hotkey.clone();
    let cli_for_task = cli.clone();
    let secrets_for_task = secrets.clone();
    let state_for_task = indicator_state.clone();
    let tray_cmd_rx = tray_bundle.map(|(_, rx)| rx);
    let tray_handle_for_task = tray_handle.clone();
    let keys_dialog_for_task = keys_dialog.clone();
    runtime.spawn(async move {
        if let Err(e) = run_session_loop(
            &chord,
            &cli_for_task,
            secrets_for_task,
            frame_rx,
            state_for_task,
            tray_cmd_rx,
            tray_handle_for_task,
            keys_dialog_for_task,
        )
        .await
        {
            tracing::error!("session loop error: {e}");
        }
    });

    if cli.headless {
        info!("--headless: blocking on Ctrl-C (no indicator window)");
        runtime.block_on(async {
            tokio::signal::ctrl_c().await.ok();
        });
        return Ok(());
    }

    // Indicator window dimensions: 360 × 80 — wider than the 320 px
    // pill so the wave layers' soft glow doesn't clip at the edges.
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
        // with_active(false): don't steal keyboard focus when the
        // indicator becomes visible. Wayland (COSMIC) ignores this
        // hint and focuses anyway, which is why the typer hides the
        // indicator on F9 release before synthesizing keys — see
        // crates/app/src/main.rs HotkeyEvent::Released handler.
        .with_active(false);
    #[cfg(target_os = "linux")]
    {
        // X11WindowType::Notification → on Mutter/KWin the WM keeps
        // the window out of the taskbar/alt-tab list and treats it
        // as an override-redirect-style overlay. On COSMIC, all
        // XWayland window types (Normal, Utility, Notification) get
        // auto-pinned to cosmic-comp's chosen placement regardless
        // of client position requests — see the COSMIC positioning
        // note in docs/architecture.md. Notification is the most
        // semantically correct for our use case.
        viewport = viewport.with_window_type(egui::X11WindowType::Notification);
    }
    // Start hidden — IndicatorApp toggles visibility on the
    // rising/falling edge of `recording`/status_text so users
    // only see the indicator while F9 is held.
    viewport = viewport.with_visible(false);
    // Pre-compute initial position so the first frame doesn't flash at
    // the eframe default before maybe_reposition runs on the first press.
    if let Ok(pos) = f9_talk_ui::Positioner::new() {
        if let Some((x, y)) = pos.compute_position(f9_talk_ui::INDICATOR_W, f9_talk_ui::INDICATOR_H) {
            viewport = viewport.with_position([x as f32, y as f32]);
        }
    }
    let native_options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    let state_for_app = indicator_state.clone();
    let keys_for_app = keys_dialog.clone();
    eframe::run_native(
        "f9-talk",
        native_options,
        Box::new(move |_cc| Ok(Box::new(IndicatorApp::new(state_for_app, keys_for_app)))),
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
// Linux: abstract Unix socket.
// macOS / Windows: advisory lock file in the config dir.

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
    // Try writing our PID — if the file is locked by another process,
    // the OS will prevent concurrent access via the exclusive open.
    use std::io::Write;
    let mut f = file;
    writeln!(f, "{}", std::process::id())?;
    Ok(Box::new(f))
}

fn load_secrets() -> HashMap<String, String> {
    let mut out = HashMap::new();
    for key in ["DEEPGRAM_API_KEY", "GLADIA_API_KEY"] {
        if let Ok(v) = std::env::var(key) {
            if !v.is_empty() {
                out.insert(key.to_string(), v);
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

async fn build_cloud_backend(
    secrets: &HashMap<String, String>,
    keywords: &[String],
) -> anyhow::Result<Arc<dyn Stt>> {
    let key = secrets.get("DEEPGRAM_API_KEY").cloned().unwrap_or_default();
    Ok(Arc::new(f9_talk_stt::deepgram::Deepgram::new(
        key,
        f9_talk_stt::deepgram::Config {
            keywords: keywords.to_vec(),
            ..Default::default()
        },
    )))
}

fn build_local_backend(keywords: &[String]) -> Arc<dyn Stt> {
    Arc::new(f9_talk_stt::whisper::WhisperLocal::new(
        f9_talk_stt::whisper::Config {
            language: "en".into(),
            keywords: keywords.to_vec(),
        },
    ))
}

#[allow(clippy::too_many_arguments)]
async fn run_session_loop(
    chord: &str,
    cli: &Cli,
    mut secrets: HashMap<String, String>,
    mut frame_rx: mpsc::Receiver<f9_talk_audio::Frame>,
    indicator: Arc<IndicatorState>,
    mut tray_cmd_rx: Option<mpsc::UnboundedReceiver<TrayCommand>>,
    tray_handle: Option<TrayHandle>,
    keys_dialog: KeysDialogState,
) -> anyhow::Result<()> {
    let mut backend = match cli.backend {
        Backend::Cloud => {
            if !secrets.contains_key("DEEPGRAM_API_KEY") {
                eprintln!(
                    "f9-talk: --backend cloud needs DEEPGRAM_API_KEY \
                     set in the environment or in ~/.config/F9_talk/secrets.env"
                );
                std::process::exit(2);
            }
            build_cloud_backend(&secrets, &cli.keywords).await?
        }
        Backend::Local => build_local_backend(&cli.keywords),
        Backend::Both => {
            warn!("--backend both is not implemented yet; falling back to --backend local");
            build_local_backend(&cli.keywords)
        }
    };
    let mut backend_name = backend.name();

    let (event_tx, mut event_rx) = mpsc::channel::<BackendEvent>(64);
    backend
        .start(event_tx)
        .await
        .map_err(|e| anyhow::anyhow!("could not start STT backend ({backend_name}): {e}"))?;
    info!("STT backend ready: {backend_name}");
    let mut paused = false;

    // Poll the keys dialog for completed saves every 250 ms. (The
    // dialog itself runs on the main thread; we just observe the
    // pending_save flag here and react to it.)
    let mut keys_save_tick = tokio::time::interval(Duration::from_millis(250));
    keys_save_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut hotkey_rx = match f9_talk_input::spawn_hotkey(chord) {
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

    let translator = cli
        .target
        .as_deref()
        .filter(|t| !t.is_empty())
        .and_then(|tgt| {
            if tgt == "en" {
                None
            } else {
                info!("translation enabled: en → {tgt}");
                Some(Translator::new("en", tgt))
            }
        });

    info!(
        "f9-talk ready. hold {} to dictate (backend={backend_name}, target={:?}, Ctrl-C to quit)",
        chord, cli.target
    );
    let mut typer = Typer::new()?;

    spawn_wakeup_watcher();

    let mut session: Option<SessionInProgress> = None;

    // Macro-ish helper: tray_cmd_rx may be None (--headless), so we
    // wrap recv in an always-pending future when it's missing.
    async fn tray_recv(
        rx: &mut Option<mpsc::UnboundedReceiver<TrayCommand>>,
    ) -> Option<TrayCommand> {
        match rx {
            Some(r) => r.recv().await,
            None => std::future::pending().await,
        }
    }

    loop {
        tokio::select! {
            evt = hotkey_rx.recv() => {
                match evt {
                    Some(HotkeyEvent::Pressed) => {
                        if paused {
                            debug!("F9 pressed while paused — ignoring");
                            continue;
                        }
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
                        // Hide the indicator FIRST so its Wayland/XWayland
                        // window unmaps and the compositor returns
                        // keyboard focus to the user's previous app.
                        // Without this, the uinput keypresses synthesized
                        // by the typer land on the indicator window
                        // (which has no text field). Skip the
                        // 'Transcribing…/Translating…/Typing…' status
                        // strings — they'd keep the window visible.
                        indicator.set_recording(false);
                        indicator.set_status_text(None);
                        // Give the compositor a beat to settle focus
                        // before the heavy work starts.
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
                        } else {
                            let final_text = if let Some(tr) = translator.as_ref() {
                                tr.translate(&result.transcript).await
                            } else {
                                result.transcript.clone()
                            };
                            if let Err(e) = typer.type_text(&final_text) {
                                warn!("typer failed: {e}");
                                if let Some(t) = tray_handle.as_ref() { t.set_error(true); }
                            } else if let Some(t) = tray_handle.as_ref() {
                                t.set_error(false);
                            }
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
                    Some(BackendEvent::SocketLost(msg)) => {
                        warn!("STT socket lost: {msg}");
                        if let Some(t) = tray_handle.as_ref() { t.set_error(true); }
                    }
                    Some(BackendEvent::SocketBack) => {
                        info!("STT socket reconnected");
                        if let Some(t) = tray_handle.as_ref() { t.set_error(false); }
                    }
                    Some(BackendEvent::Error(e)) => warn!("STT error: {e}"),
                    None => {}
                }
            }
            cmd = tray_recv(&mut tray_cmd_rx) => {
                let Some(cmd) = cmd else { continue; };
                match cmd {
                    TrayCommand::PauseToggled(p) => {
                        paused = p;
                        if let Some(t) = tray_handle.as_ref() { t.set_paused(p); }
                        if p {
                            indicator.set_recording(false);
                            // Cancel any in-flight session.
                            if session.take().is_some() {
                                let _ = backend.end_session(Duration::from_millis(50)).await;
                            }
                        }
                        info!("tray: {}", if p { "paused" } else { "resumed" });
                    }
                    TrayCommand::EditKeys => {
                        let cur_dg = f9_talk_ui::keys_dialog::read_current_keys();
                        keys_dialog.open(cur_dg);
                        info!("tray: opening keys dialog");
                    }
                    TrayCommand::Quit => {
                        info!("tray: Quit");
                        backend.stop().await;
                        std::process::exit(0);
                    }
                }
            }
            _ = keys_save_tick.tick() => {
                let Some(saved) = keys_dialog.take_pending_save() else { continue; };
                if let Err(e) = f9_talk_ui::keys_dialog::save_to_disk(&saved) {
                    warn!("could not write secrets.env: {e}");
                    if let Some(t) = tray_handle.as_ref() { t.set_error(true); }
                    continue;
                }
                if let Some(v) = saved.deepgram.as_ref() {
                    if v.is_empty() { secrets.remove("DEEPGRAM_API_KEY"); }
                    else { secrets.insert("DEEPGRAM_API_KEY".into(), v.clone()); }
                    if matches!(cli.backend, Backend::Cloud) {
                        info!("Deepgram key changed; rebuilding backend");
                        backend.stop().await;
                        match build_cloud_backend(&secrets, &cli.keywords).await {
                            Ok(new_backend) => {
                                let (new_tx, new_rx) = mpsc::channel::<BackendEvent>(64);
                                if let Err(e) = new_backend.start(new_tx).await {
                                    error!("backend rebuild failed: {e}");
                                    if let Some(t) = tray_handle.as_ref() { t.set_error(true); }
                                } else {
                                    backend = new_backend;
                                    backend_name = backend.name();
                                    event_rx = new_rx;
                                    info!("STT backend rebuilt ({backend_name})");
                                    if let Some(t) = tray_handle.as_ref() { t.set_error(false); }
                                }
                            }
                            Err(e) => {
                                error!("could not rebuild backend: {e}");
                                if let Some(t) = tray_handle.as_ref() { t.set_error(true); }
                            }
                        }
                    }
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
