# Changelog

All notable changes are documented here. Versions follow [Semantic Versioning](https://semver.org/).

---

## [0.7.0] — 2026-06-25 — Idle dot + click-to-quit

### Added
- **Idle "ready" dot.** When you're not dictating, the bottom-center
  indicator now shows a small breathing red dot instead of nothing — so
  you can always see at a glance that f9-talk is running and listening.
  Hold F9 and it morphs into the voice wave; release and it settles back
  to the dot.
- **Click the dot to quit.** Hovering the dot turns it into a red "×"
  close button; a left-click exits the tool cleanly (relaunch from the
  apps menu, or it autostarts at next login). Only a small hotspot over
  the dot is clickable — the rest of the overlay stays click-through, and
  it still never takes keyboard focus.

---

## [0.6.2] — 2026-06-25 — Installed icon

### Fixed
- `install --user` now installs the app icon into the user's icon theme
  (the icon is embedded in the binary), so the AppImage and cargo
  installs show the f9-talk logo in the launcher instead of a blank
  generic icon. Previously only the `.deb` shipped the icon.

---

## [0.6.1] — 2026-06-25 — Terminal paste fix

### Fixed
- Paste the transcript with **Ctrl+Shift+V** instead of Ctrl+V, so
  dictation works in terminals — where Ctrl+V means "insert the next
  character literally," not paste. Browsers and most editors treat
  Ctrl+Shift+V as paste-plain-text, so the one shortcut covers both.

---

## [0.6.0] — 2026-06-25 — Lean Deepgram-only rebuild

A focused rewrite: hold F9 → Deepgram Nova-3 → type. ~2,900 fewer lines,
295 → 222 dependencies, 11 MB → 8.4 MB binary.

### Added
- **Native Wayland indicator** — a `wlr-layer-shell` overlay rendered in
  software: anchored bottom-center, no focus border, real transparency,
  an anti-aliased neon voice wave. Rebuilt per dictation so it appears on
  the focused monitor. Height tunable via `--indicator-margin`.
- **Reliable, layout-independent typing** — atomic clipboard paste
  (`wl-copy` + uinput Ctrl+V) on Wayland, so the compositor can't drop
  characters and the active xkb layout no longer matters; the prior
  clipboard is saved and restored.
- **Self-contained packaging** — the `.deb` now pulls `wl-clipboard` +
  `wtype`, and the AppImage bundles them, so one download works with
  nothing else installed.

### Removed
- Local whisper.cpp backend, the translate feature, the GTK system tray,
  and the API-key dialog. The hotkey is hardcoded to F9; the CLI is now
  just `--indicator-margin`, `-v`, and `install`/`uninstall`.

### Fixed
- Generated autostart/desktop `Exec=` no longer passes the removed
  `--backend` flag.

---

## [0.5.1] — 2026-05-09 — Docs + dev tooling

No runtime behaviour change. Same binary as 0.5.0.

### Added
- `run.sh` at the repo root — a thin dev launcher that rebuilds on
  demand (`--build`), kills any stale instance so the abstract-socket
  lock doesn't reject the new process, and `exec`s the binary via
  `sg input` so it works even when your GUI session predates joining
  the `input` group. Forwards any extra flags to `f9-talk`.

### Changed
- README restructured for clarity: Highlights callout near the top,
  numbered Quick start, run.sh documented under Build from source,
  redundant "Features" list folded into the relevant sections, and
  the Reliability bullet about backoff updated to reflect the
  reset-on-healthy-connect fix from 0.5.0.

---

## [0.5.0] — 2026-05-09 — Deepgram-only

### Removed
- **BREAKING**: AssemblyAI cloud backend removed (again — see 0.3.0). The
  Rust port in 0.4.0 brought it back, but Universal-3 Pro Streaming still
  doesn't fit hold-to-talk: sessions close after every `Terminate`,
  multi-turn finalization adds 1–2 s of latency, and long presses get
  truncated to the latest cumulative-replacement turn. Three rounds of
  patches (frame coalescing for the 50–1000 ms input rule, zero-backoff
  reconnect on clean close, multi-turn accumulator) made it work but
  Deepgram Nova-3 is structurally a better fit for press/release UX.
- `crates/stt/src/assemblyai.rs` deleted; `pub mod assemblyai` removed
  from `f9-talk-stt`.
- `--cloud-provider` CLI flag removed (only one cloud backend left).
- `--cloud-hotkey` CLI flag removed — it was declared but never wired
  to anything, so passing it had no effect.
- Cloud-provider submenu removed from the tray.
- AssemblyAI field removed from the **API Keys…** dialog.
- `ASSEMBLYAI_API_KEY` no longer read from environment or
  `secrets.env`. Existing entries in your `secrets.env` are ignored
  silently and can be deleted by hand.

### Changed
- Deepgram is now the only cloud backend; `--backend cloud` requires
  `DEEPGRAM_API_KEY`.
- Deepgram reconnect no longer suffers cascading backoff after long
  uptime. A session that opened successfully and then dropped (network
  blip, server restart, idle timeout) reconnects at the initial 1 s
  delay instead of doubling up to the 30 s cap. Fresh-connect failures
  still use exponential backoff.

### Internal
- Extracted `parse_final` from Deepgram's `handle_text` and added 9 unit
  tests covering finals, partials, missing fields, malformed JSON, and
  multi-alternative payloads.
- Removed dead `last_error` field from Deepgram's shared state.
- Removed the `_unused()` stub from `keys_dialog.rs` and the now-unused
  `warn` import.

### Migration from 0.4.0
- If you had `ASSEMBLYAI_API_KEY` in `~/.config/F9_talk/secrets.env`,
  it's safe to delete that line.
- If you used `--cloud-provider deepgram` in autostart, drop the flag —
  Deepgram is the default and only option now.
- The tray no longer has a "Cloud provider" submenu; pause/quit/keys
  are unchanged.

---

## [0.4.0] — 2026-05-08 — Rust rewrite

**BREAKING.** F9 Talk is now a single statically-linked Rust binary.
The Python implementation under `f9_talk/` is preserved on the
`v0.4-rust` branch's history but no longer ships in the .deb.

### Why
- Cold start drops from ~1 s to <100 ms.
- Idle RAM drops from ~300 MB to <60 MB.
- 13 MB single-binary install — no Python venv built in `postinst`,
  no system Python needed.
- Indicator + tray remain visually identical for the wave style.

### Added
- **AssemblyAI Universal-3 Pro Streaming** as the default cloud backend
  (~150 ms P50, half of Deepgram). Auto-selected when an `ASSEMBLYAI_API_KEY`
  is present in `secrets.env`; otherwise falls back to Deepgram.
- New `--backend local` path uses **whisper.cpp** via `whisper-rs`. The
  ~1.5 GB `ggml-large-v3-turbo.bin` model is downloaded lazily on the
  first F9 press in local mode, cached at `~/.cache/f9-talk/models/`.
  Default build is CPU-only; rebuild with `--features cuda` (after
  `sudo apt install nvidia-cuda-toolkit`) for GPU acceleration.
- New tray "API Keys…" dialog: paste keys without editing the file by
  hand; saving rebuilds the active backend in-place.
- New `--headless` flag: pure CLI mode with no indicator window.
- Pipeline tracing line per F9 press in `journalctl --user -t f9-talk`:
  `press_to_release=…  frames=…  first_byte_sent=…  release_to_final=…  transcript=…`.

### Changed
- **Hotkey listener** rewritten on top of `evdev` (via the
  `hotkey-listener` crate). Same chord syntax as before
  (`f9`, `<ctrl>+<alt>+space`, `ctrl+shift+a`); the 50 ms X11 auto-repeat
  debounce is preserved literally.
- **Microphone capture** rewritten on top of `cpal` instead of a
  `parec` subprocess. On stream errors the mic is reopened with
  exponential backoff (1 s → 30 s cap) — fixes the "PipeWire restart
  silently kills dictation until the app is restarted" failure mode.
- **Text injection** rewritten on direct `/dev/uinput` writes instead
  of `xdotool`. Works on X11 *and* Wayland identically. ASCII printable
  chars are mapped to scancodes; non-ASCII characters use the IBus
  Ctrl+Shift+U Unicode dance.
- **Deepgram WebSocket reconnect** ported from v0.3.1's Python fix.
- **Indicator animation**: the wave style is preserved (56-point Bézier
  path, four-layer paint, asymmetric EMA on RMS); the other 5 styles
  (bars, pulse, dots, ripple, blob) are not ported. The `--style` CLI
  flag is kept for compatibility but warn-logs and falls through to wave.
- **Translation**: Lingva (primary) → MyMemory (fallback) ported with
  the same fall-through semantics.

### Removed
- Gladia cloud backend (existing `GLADIA_API_KEY` in `secrets.env` is
  read but ignored).
- The 5 alternative indicator animation styles.
- Python source tree (`f9_talk/`), the `pip install` step in postinst,
  and the `/opt/f9-talk/.venv` runtime.

### New requirements
- `udev` rule for `/dev/uinput` (`KERNEL=="uinput", MODE="0660", GROUP="input"`)
  is installed by the package and applied via `udevadm trigger` in postinst.
- The installing user is auto-added to the `input` group; **you must
  log out and back in once** for `evdev` hotkey + `uinput` typer access
  to take effect.

### Migration from 0.3.x
- The .deb declares `Replaces:` and `Conflicts:` against `f9-talk (<= 0.3.99)`,
  so `sudo dpkg -i f9-talk_0.4.0_amd64.deb` cleanly removes the Python
  install before laying down the new binary.
- `~/.config/F9_talk/secrets.env` is preserved untouched.
- Autostart entry path is unchanged (`Exec=f9-talk --backend cloud`),
  so the next login boots straight into the Rust build.

### Rollback
- `sudo dpkg -i f9-talk_0.3.1_all.deb` (kept on disk in `~/Desktop/F9_talk/`)
  restores the Python install with no manual fix-up required;
  `secrets.env` is untouched.

---

## [0.3.1] — 2026-05-08

### Fixed
- **Deepgram WebSocket auto-reconnect**: after long uptime (~1 hour) the persistent Deepgram socket would close silently. F9 presses still showed the indicator animation but no audio reached the server (`finalize failed` in journalctl) and no text was typed — only an app restart recovered it. The close handler now kicks off a reconnect loop with exponential backoff (1 s → 30 s cap) and sessions resume automatically. `stop()` sets a shutdown flag so clean teardown does not trigger reconnect spam (`f9_talk/stt/deepgram.py`)

---

## [0.3.0] — 2026-05-08

### Removed
- **BREAKING**: AssemblyAI cloud backend removed entirely. Universal Streaming v3's end-of-turn detection requires sustained silence after speech, but our hold-to-talk sessions close as soon as F9 is released. Multiple workarounds (`speech_model="universal-streaming-english"`, `format_turns=True`, partial-transcript fallback, lower `min_end_of_turn_silence_when_confident`) didn't produce reliable results for sub-2-second holds. Removed rather than ship a flaky provider
- `assemblyai` Python dependency dropped from `pyproject.toml`
- `ASSEMBLYAI_API_KEY` no longer read or honored
- AssemblyAI option removed from the tray's **Cloud provider** submenu
- AssemblyAI field removed from the **API Keys…** dialog (two fields now: Deepgram + Gladia)

### Migration
- If you had `ASSEMBLYAI_API_KEY` in `~/.config/F9_talk/secrets.env`, it's safe to delete that line — the app ignores it
- Deepgram remains the default and recommended backend; Gladia stays as the alternative

### Added
- `websockets>=12` is now an explicit dependency (was previously transitive)

---

## [0.2.3] — 2026-05-08

### Fixed
- AssemblyAI: rejected our 25 ms mic frames with `Input Duration Violation: 25.0 ms. Expected between 50 and 1000 ms`. The AssemblyAI backend now buffers incoming audio frames until at least 50 ms (1600 bytes at 16 kHz mono int16) is available before yielding to the SDK. Deepgram/Gladia paths unaffected — they accept smaller chunks

---

## [0.2.2] — 2026-05-08

### Fixed
- AssemblyAI: `speech_model="universal"` was rejected as an invalid enum value in SDK 0.64. The accepted English value is `"universal-streaming-english"` (others: `universal-streaming-multilingual`, `u3-rt-pro`, `whisper-rt`, `u3-pro`)

---

## [0.2.1] — 2026-05-08

### Fixed
- AssemblyAI streaming sessions failed at connect with `1 validation error for StreamingParameters` after the `assemblyai` SDK 0.64 made `speech_model` a required field. v0.2.1 passed `speech_model="universal"` but that value is itself rejected — see v0.2.2 for the working fix

---

## [0.2.0] — 2026-05-08

### Added
- **Multi-provider cloud STT**: tray submenu lets you switch live between Deepgram Nova-3 (default), AssemblyAI Universal, and Gladia v2 — each backend implements the same `begin_session` / `send_audio` / `end_session` protocol
- **System tray icon** with three states (active / paused / error) and a right-click menu: Pause/Resume listening, Cloud provider ▶, API Keys…, Quit. Left-click toggles pause
- **API Keys dialog** reachable from the tray menu — three masked fields (Deepgram / AssemblyAI / Gladia) with per-field Show toggle. Save persists to `~/.config/F9_talk/secrets.env` preserving comments + unrelated entries; takes effect immediately (Deepgram reconnects, others pick up on next session)
- **Error feedback**: failed STT sessions now pop a desktop notification with the provider error message and turn the tray icon red until the next successful session
- **Spec-driven design docs** under `docs/superpowers/specs/` for tray icon, API Keys dialog

### Changed
- Default Deepgram model upgraded from `nova-2` to `nova-3` — ~30% lower WER, sub-300 ms streaming, marginal cost increase (~$0.46/hr)

### Fixed
- Recording indicator invisible on multi-monitor setups — async repositioning never dispatched back to the Qt main thread because `QTimer.singleShot` was called from a plain Python worker; reverted to synchronous `_reposition()` before `show()`
- Tray icon was empty / unrecognizable in GNOME — bundled the PNG/SVG assets into the package (`pyproject.toml` `package-data`), set `QIcon.fromTheme("f9-talk")` for AppIndicator extensions, set proper application name via `QApplication.setApplicationName("F9 Talk")`

### Tests
- Up to 109 unit tests (57 → 109): tray UI, app pause/provider/reload behavior, API Keys dialog, secrets file load/save, AssemblyAI/Gladia backends

---

## [0.1.0] — 2026-05-08

### Added
- Hold-to-talk dictation via Deepgram Nova-2 cloud STT (~300 ms latency)
- Local offline backend via faster-whisper (GPU required)
- Audio-reactive overlay indicator with six animation styles: `wave`, `bars`, `pulse`, `dots`, `ripple`, `blob`
- Real-time speech-to-text translation to Arabic and other languages
- Custom hotkey support — any key combination, not just F9
- Dual-backend mode: F9 = local Whisper, F8 = Deepgram cloud (side-by-side comparison)
- Domain keyword boosting for proper nouns and jargon
- Debian package with autostart, icon integration, and first-run API key setup
- Automated `.deb` releases via GitHub Actions on version tags
- 57 unit tests covering typer, session state machine, hotkey parsing, and translation backends
- CI pipeline: lint (ruff) + test matrix (Python 3.10–3.12) on every push and pull request
- Dependabot for automated dependency updates
- `SECURITY.md`, `CONTRIBUTING.md`, `CHANGELOG.md`

### Fixed
- **Critical:** `_acquire_lock()` socket was garbage collected immediately, allowing multiple instances to run simultaneously — socket now held at module level
- **Security:** language code path injection in Lingva translator URL — codes now validated against `[a-zA-Z]{2,5}`
- **Security:** API key input in setup dialog could corrupt `secrets.env` with embedded newlines or `=` characters
- Garbled text output when modifier keys (Shift, F9) were held during xdotool injection — fixed with `--clearmodifiers`
- Per-character typing delay (`--delay 20`) caused visible word-by-word output — reverted to `--delay 0`
- Audio glitches during long recordings — increased parec latency buffer from 15 ms to 100 ms
- Recording indicator invisible on multi-monitor setups — async repositioning attempt left the widget at default (0,0) on a different screen because `QTimer.singleShot` from a non-Qt worker thread never dispatched back to the main thread; reverted to synchronous `_reposition()` before `show()`
