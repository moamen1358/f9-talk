# f9-talk architecture

A single statically-linked Rust binary. Three thread categories
cooperate over `tokio::mpsc` and `Arc<Mutex>` channels.

```
main thread (indicator)            tokio runtime workers              cpal callback (RT)
───────────────────────            ─────────────────────              ──────────────────
Wayland → wlr-layer-shell        ┌─ hotkey-listener task ─┐           build_input_stream
  overlay thread (wave) +        │   evdev events on F9   │             down-mix to mono
  Ctrl-C wait                    │                        │             resample 44.1→16k
X11 → eframe wave window         ├─ session loop ─────────┤             s16le bytes
                                 │   tokio::select! over: │
  reads RmsHandle (Arc<Mutex>)   │   - hotkey events      │ ◄──── mpsc::channel(64)
                                 │   - mic frame_rx       │       drop-oldest on overflow
                                 │   - backend events     │
                                 │   - Ctrl-C             │
                                 └────────┬───────────────┘
                                          │
                                 ┌── STT WS client ───┐
                                 │   tokio-tungstenite│
                                 │   Deepgram Nova-3  │ ◄── frame_rx → send_audio()
                                 │   end_session()→fin │
                                 └────────────────────┘
```

## Workspace layout

The workspace under `crates/` is organized as:

| Crate | Role |
|---|---|
| `f9-talk-core` | Shared constants (frame size, sample rate, channel capacity) |
| `f9-talk-input` | Hotkey-listener (F9) with 50 ms auto-repeat debounce; typer dispatcher (wl-copy paste, wtype, xdotool, uinput) |
| `f9-talk-audio` | cpal mic streamer with linear resampler and RMS extraction for the wave indicator |
| `f9-talk-stt` | `Stt` trait + Deepgram Nova-3 streaming WebSocket client |
| `f9-talk-ui` | eframe wave indicator (X11) + native `wlr-layer-shell` overlay (Wayland) + X11 positioner |
| `f9-talk` (binary) | clap CLI, secrets loader, abstract-socket lock, session loop, glue |

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
    libxcb1-dev libxcb-render0-dev libxcb-shape0-dev \
    libxcb-xfixes0-dev libxkbcommon-dev libfontconfig1-dev \
    libxdo-dev

# Runtime: for layout-independent typing on Wayland
sudo apt install wl-clipboard wtype

cargo build --release
./target/release/f9-talk --help
```

`run.sh` rebuilds on demand and works around the `input`-group session
issue. Use it instead of reinstalling the `.deb` on every change:

```bash
./run.sh                       # launch the existing release binary
./run.sh --build               # rebuild first, then launch
./run.sh --indicator-margin 80 # any f9-talk flag is forwarded
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
| Missing spaces / wrong characters when typing | Install `wl-clipboard` (`sudo apt install wl-clipboard`). The typer then pastes the whole transcript atomically — layout-independent, no dropped keystrokes. Without it the fallback is per-key injection, which some compositors mangle. |
| Indicator on the wrong height / overlapping app bars | Raise it with `--indicator-margin <px>` (default 20). |
| Indicator appears on the wrong monitor | The Wayland overlay is rebuilt each press onto the focused output; click into the target app first so it holds focus when you press F9. |
| `no speech detected` | Hold F9 for at least 0.3 s before releasing. |
| "Already running" with no visible window | `pkill -f /usr/bin/f9-talk` and relaunch. |
| `wgpu` panic at startup | The shipped binary uses the OpenGL `glow` renderer. |

Logs are available via `journalctl --user -t f9-talk -f`. Per-press
latency lines use the target `f9_talk::press`.
