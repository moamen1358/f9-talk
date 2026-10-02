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
                                 ┌── STT WS client ─────────┐
                                 │   tokio-tungstenite      │
                                 │   AssemblyAI U-3.6 Pro   │ ◄── frame_rx → send_audio()
                                 │   (or Deepgram Nova-3)   │
                                 │   end_session() → final  │
                                 └──────────────────────────┘
```

## Workspace layout

The workspace under `crates/` is organized as:

| Crate | Role |
|---|---|
| `f9-talk-core` | Shared constants (frame size, sample rate, channel capacity) |
| `f9-talk-input` | Hotkey-listener (F9) with 50 ms auto-repeat debounce; typer dispatcher (wl-copy paste, wtype, xdotool, uinput) |
| `f9-talk-audio` | cpal mic streamer with linear resampler and RMS extraction for the wave indicator |
| `f9-talk-stt` | `Stt` trait + AssemblyAI Universal-3.6 Pro (default) and Deepgram Nova-3 streaming WebSocket clients |
| `f9-talk-ui` | eframe wave indicator (X11) + native `wlr-layer-shell` overlay (Wayland) + X11 positioner |
| `f9-talk` (binary) | clap CLI, Settings window (egui), settings (`config.toml`, `keyterms.txt`) and keys (keyring / `secrets.env` / env), abstract-socket lock + reload channel, session loop, glue |

## Speech-to-text backends

The backend is chosen by `backend` in `~/.config/F9_talk/config.toml`
(`assemblyai` by default, `deepgram` as the fallback). Both send the
key terms from `~/.config/F9_talk/keyterms.txt`: AssemblyAI as
`keyterms_prompt`, Deepgram as repeated `keyterm` params (Nova-3 rejects
the old `keywords` param with HTTP 400).

### Release: wait for the final, never a fixed timeout

Up to v0.7.1 the release path waited 350 ms for the first final and
typed whatever had arrived. Deepgram (endpointing 25 ms) often sends a
segment final first and the rest of the sentence later, so the end of
the sentence was dropped; a large share of real presses hit the 350 ms
limit.
Now `end_session` asks the service to finalize and waits for its reply:

- **Deepgram**: sends `Finalize`, waits for the result flagged
  `from_finalize: true` (Deepgram sends it after every other final of
  the press, empty for a silent press).
  Known limit of this fallback: a pause of a second or so mid-sentence
  still comes back as two punctuated pieces at every `endpointing` value
  tried (25, 100, 300, false), and 100 or 300 make Nova-3 Title-Case
  whole segments when key terms are sent, so it stays at 25 ms.
- **AssemblyAI**: sends `ForceEndpoint`. While a turn is open (speech
  heard, or a partial with no final) it waits for that turn's formatted
  final. With no turn open it ends as soon as the reply to
  `ForceEndpoint` lands, or 0.7 s after it when nothing comes (the server
  answers nothing when no speech is pending).
- `finalize_timeout_ms` (4 s) is only a safety net. If it passes, the
  finals plus the open turn's latest partial are typed and a warning is
  logged.

Before finalizing, the session loop forwards any mic frames still queued
in the channel, so the last 25-50 ms of the press reach the service.
Measured on this machine (TTS clips through `f9-talk simulate`): release
to text 195-360 ms with AssemblyAI on clear TTS clips and 245-390 ms on
accented speech up to 59 s long, 210-380 ms with Deepgram.

### AssemblyAI session lifecycle

AssemblyAI bills streaming by how long the socket is open, idle time
included, so the session is not held open forever:

- A session opens at start-up, so the first press is warm.
- It stays open `assemblyai_warm_seconds` (60 s) after the last press,
  then closes with `Terminate`. The server is also told
  `inactivity_timeout` = warm + 30 s as a billing safety net.
- A press with no open session reconnects at once (socket ~0.3 s,
  `Begin` ~0.9 s). The press's audio is kept from its first frame and
  sent as soon as the socket opens, so no word is lost.
- If the socket drops mid-press, the whole press's audio is replayed
  into a new session.
- Audio goes out in 50 ms frames (two 25 ms mic frames): the server
  closes the socket (code 3007) on frames shorter than 50 ms, so the
  last odd frame of a press is padded with silence.
- `assemblyai_warm_seconds = 0` keeps one session open (with KeepAlive)
  for as long as the app runs.
- English is pinned (`language_codes=["en"]`).
- `min_turn_silence` and `max_turn_silence` are set to the 10 s maximum.
  With the server default (about 1.3 s) a pause to think mid-sentence
  ended the turn and each piece came back punctuated on its own ("The
  tokenizing model. Works correctly, ..."); now one press is one turn,
  ended by `ForceEndpoint`, and comes back as one sentence.

### Trying a clip without the mic

`f9-talk simulate <clip.wav> [--backend assemblyai|deepgram] [--warm-ms N]`
streams a 16 kHz mono WAV through the backend at real-time pace, the way
the mic loop does while F9 is held, and prints the text that would be
typed plus `release_to_text_ms` as one JSON line. The end-to-end tests
use it:

```bash
cargo test -p f9-talk --test e2e_dictation -- --ignored --nocapture
```

## Settings window and keys

`f9-talk settings` (`crates/app/src/settings_ui.rs`) is an egui window
that runs as its own short-lived process, so it never shares a thread
with the dictation loop or the Wayland indicator. It is opened by:

- right-clicking the red dot (the layer indicator calls back into the
  app, which spawns `f9-talk settings`);
- the apps-menu entry's **Settings** action (`Exec=... settings`);
- launching `f9-talk` while it already runs (the instance lock is taken,
  so the second launch shows Settings instead of exiting);
- the first run with no key, and an F9 press with no key (at most once
  per 5 s).

Save writes `config.toml` and `keyterms.txt`, stores changed keys, then
sends `reload` to the running app over its instance-lock socket
(abstract Unix datagram `@f9-talk-instance-lock`). The app stops the old
backend and starts one from the new settings; a reload that arrives
while F9 is held waits until that press is typed. If no instance is
running, Save starts one.

Keys (`crates/app/src/keys.rs`) are looked up in the environment, then
the desktop keyring (Secret Service via the `keyring` crate, service
`f9-talk`), then `secrets.env`. Save puts a key in the keyring and drops
its line from `secrets.env`, or writes `secrets.env` with mode 600 when
no keyring answers. Existing `secrets.env` keys keep working untouched.
**Test key** makes one free request: AssemblyAI's `GET /v3/token`
(mints a short-lived streaming token) or Deepgram's `GET /v1/projects`,
and shows "works" or the service's own error with its HTTP status.

## Reliability mechanisms

- WebSocket auto-reconnect on socket close and on send failures.
  Backoff resets after a healthy connection drops. AssemblyAI reconnects
  only when a press needs it (see above).
- Mic auto-restart on cpal stream errors with the same backoff.
- Wake-from-suspend detection via 5 s polling that flags clock drift
  greater than 30 s and reconnects the STT client.
- Permission preflight at startup that prints actionable instructions
  and exits non-zero if the `input` group or `/dev/uinput` access is
  missing.
- Single-instance lock on the abstract Unix socket
  `@f9-talk-instance-lock`, which also carries the Settings `reload`
  message.

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
| `/dev/uinput is not writable`, or F9 does nothing | `install --system` (or the `.deb`) adds you to the `input` group, but the desktop session must restart for it to apply. Log out and back in once; `groups` must list `input`. |
| Missing spaces / wrong characters when typing | The AppImage bundles `wl-clipboard`; for a source build install it (`sudo apt install wl-clipboard`). The typer then pastes the whole transcript at once: layout-independent, no dropped keystrokes. Without it the fallback is per-key injection, which some compositors mangle. |
| Indicator on the wrong height / overlapping app bars | Raise it with `--indicator-margin <px>` (default 20). |
| Indicator appears on the wrong monitor | The Wayland overlay is rebuilt each press onto the focused output; click into the target app first so it holds focus when you press F9. |
| `no speech detected` in the log | The service heard no words. Check the microphone: the red wave should move while you speak. |
| Settings shows "Does not work: HTTP 401" or "HTTP 404: Invalid API key" | The key is wrong or was copied with a missing character: copy it again from the service's dashboard. |
| The text is not typed into one particular app | On Wayland the text is pasted with Ctrl+Shift+V; an app that uses that shortcut for something else does not receive it. |
| No red dot on GNOME | GNOME has no `wlr-layer-shell`, so the dot cannot be drawn. Dictation still works; open Settings from the apps menu. |
| Launching opens Settings, but no red dot is visible | An old instance still holds the lock: `pkill -f f9-talk` and relaunch. |
| `wgpu` panic at startup | The shipped binary uses the OpenGL `glow` renderer. |

Logs are available via `journalctl --user -t f9-talk -f`. Per-press
latency lines use the target `f9_talk::press`.
