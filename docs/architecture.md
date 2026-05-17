# f9-talk architecture

A single statically-linked Rust binary. Three thread categories
cooperate over `tokio::mpsc` and `Arc<Mutex>` channels.

```
main thread (winit/eframe)         tokio runtime workers              cpal callback (RT)
─────────────────────────          ─────────────────────              ──────────────────
ViewportApp::update              ┌─ hotkey-listener task ─┐           build_input_stream
  paint wave + status            │   evdev events on F9   │             down-mix to mono
  ViewportCommand::OuterPosition │                        │             resample 44.1→16k
  Visible(true/false)            ├─ session loop ─────────┤             s16le bytes
                                 │   tokio::select! over: │
  reads RmsHandle (Arc<Mutex>)   │   - hotkey events      │ ◄──── mpsc::channel(64)
                                 │   - mic frame_rx       │       drop-oldest on overflow
                                 │   - tray cmd_rx        │
                                 │   - keys_save_tick     │
                                 │   - backend events     │
                                 └────────┬───────────────┘
                                          │
                                 ┌── STT WS client ───┐
                                 │   tokio-tungstenite│
                                 │   Deepgram Nova-3  │ ◄── frame_rx → send_audio()
                                 │   (or local Whisper)│  end_session() → oneshot
                                 └────────────────────┘

GTK thread (tray-icon)
  gtk::main() loop, MenuEvent → tokio mpsc
```

## Workspace layout

The workspace under `crates/` is organized as:

| Crate | Role |
|---|---|
| `f9-talk-core` | Shared constants (frame size, sample rate, channel capacity) |
| `f9-talk-input` | Hotkey-listener chord parser with 50 ms auto-repeat debounce; typer dispatcher (xdotool, clipboard, uinput) |
| `f9-talk-audio` | cpal mic streamer with linear resampler and RMS extraction for the wave indicator |
| `f9-talk-stt` | `Stt` trait, Deepgram Nova-3 streaming client, whisper.cpp local backend |
| `f9-talk-ui` | egui indicator viewport, tray icon, API-keys dialog, X11 positioner |
| `f9-talk-translate` | Lingva primary client with MyMemory fallback |
| `f9-talk` (binary) | clap CLI, secrets loader, abstract-socket lock, glue |

## Reliability mechanisms

- WebSocket auto-reconnect on socket close and on three consecutive
  send failures. Backoff resets after a healthy connection drops.
- Mic auto-restart on cpal stream errors with the same backoff.
- Wake-from-suspend detection via 5 s polling that flags clock drift
  greater than 30 s and reconnects the STT client.
- Permission preflight at startup that prints actionable instructions
  and exits non-zero if the `input` group or `/dev/uinput` access is
  missing.
- Single-instance lock on the abstract Unix socket
  `\0f9-talk-instance-lock`.

## Building from source

```bash
git clone https://github.com/moamen1358/f9-talk.git
cd f9-talk

# Rust toolchain
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y

# Linux build dependencies
sudo apt install build-essential pkg-config \
    libasound2-dev libdbus-1-dev libudev-dev libevdev-dev \
    libgtk-3-dev libxcb1-dev libxcb-render0-dev libxcb-shape0-dev \
    libxcb-xfixes0-dev libxkbcommon-dev libfontconfig1-dev \
    libayatana-appindicator3-dev libssl-dev libxdo-dev libclang-dev

cargo build --release
./target/release/f9-talk --help
```

`run.sh` rebuilds on demand and works around the `input`-group session
issue. Use it instead of reinstalling the `.deb` on every change:

```bash
./run.sh             # launch the existing release binary
./run.sh --build     # rebuild first, then launch
./run.sh --target ar # any f9-talk flag is forwarded
```

To enable the local Whisper backend with CUDA:

```bash
sudo apt install nvidia-cuda-toolkit
cargo build --release --features cuda
```

To rebuild the `.deb`:

```bash
cargo install cargo-deb
cargo deb -p f9-talk
sudo dpkg -i target/debian/f9-talk_*.deb
```

## Troubleshooting

| Symptom | Resolution |
|---|---|
| `/dev/uinput is not writable` | The `.deb` adds the user to the `input` group, but the GUI session must restart for it to take effect. Log out and back in once. |
| Tray icon invisible on vanilla GNOME | `sudo apt install gnome-shell-extension-appindicator`. Pop!_OS (on GNOME), Ubuntu, and KDE ship native support. |
| Tray icon invisible on Pop!_OS COSMIC | COSMIC does not yet ship a StatusNotifierItem applet, so the tray menu has nowhere to render. Configure the Deepgram key directly in `~/.config/F9_talk/secrets.env`; everything else works without the tray. Track [pop-os/cosmic-applets](https://github.com/pop-os/cosmic-applets) for native support. |
| Indicator appears top-center on COSMIC (not bottom) | `cosmic-comp` auto-pins every XWayland window to its chosen placement and silently rejects client position requests (verified with a 67-call `xdotool windowmove` burst that had zero effect). The proper fix is a Wayland-native `wlr-layer-shell` surface; until then the indicator lives wherever cosmic-comp puts it. Typing is unaffected. |
| `no speech detected` | Hold F9 for at least 0.3 s before releasing. |
| Wrong characters under non-en-US layout | Verify `xdotool` is installed; the typer logs `primary=xdotool` at startup. |
| "Already running" with no visible window | `pkill -f /usr/bin/f9-talk` and relaunch. |
| `wgpu` panic at startup | The shipped binary uses the OpenGL `glow` renderer; do not mix CUDA and wgpu in custom builds. |

Logs are available via `journalctl --user -t f9-talk -f`. Per-press
latency lines use the target `f9_talk::press`.
