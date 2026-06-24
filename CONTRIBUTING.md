# Contributing to f9-talk

Thank you for your interest. This guide covers everything you need to submit a quality contribution to the v0.4 Rust codebase.

## Development setup

```bash
git clone https://github.com/moamen1358/f9-talk.git
cd f9-talk

# Rust toolchain (rustup); skip if already installed
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y

# Linux build deps
sudo apt install build-essential pkg-config \
    libasound2-dev libdbus-1-dev libudev-dev libevdev-dev \
    libxcb1-dev libxcb-render0-dev libxcb-shape0-dev \
    libxcb-xfixes0-dev libxkbcommon-dev libfontconfig1-dev \
    libxdo-dev
```

For runtime testing you also need to be in the `input` group + have the udev rule installed (see `packaging/README.md` and the **Install** section of the main `README.md`).

## Building + running

```bash
cargo build --release          # the only build
cargo run --release -- --help  # see all CLI flags
cargo run --release -- -v      # run with verbose logging
```

The binary lives at `target/release/f9-talk`.

## Workspace layout

```
crates/
├── core/       FRAME_BYTES, SAMPLE_RATE_HZ, FRAME_CHANNEL_CAPACITY constants
├── input/      hotkey-listener wrapper (F9 + 50 ms debounce) + typer
├── audio/      cpal mic streamer with linear resampler + auto-restart
├── stt/        Stt trait + Deepgram Nova-3 streaming client
├── ui/         eframe IndicatorApp (X11) + wlr-layer-shell overlay (Wayland)
└── app/        clap CLI + abstract-socket lock + session loop + glue
```

## CI bar before opening a PR

```bash
# All four must pass green:
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --workspace --no-default-features --all-targets -- -D warnings
cargo test --workspace
cargo deny --all-features check       # licenses + advisories + bans
```

GitHub Actions runs the same checks on every push to `main` or PR — see `.github/workflows/rust-ci.yml`.

## Style + conventions

- Run `cargo fmt --all` before committing.
- Keep `#![forbid(unsafe_code)]` on every lib crate. The one place we use `unsafe` (the abstract-socket `libc::bind` call in `crates/app/src/main.rs`) is in the binary, not a library.
- Prefer `parking_lot::Mutex` over `std::sync::Mutex` for short critical sections inside the audio / paint loops.
- Logging: `tracing` everywhere; use the `f9_talk::press` target for per-press telemetry so it's `journalctl --user -t f9-talk -f` greppable.
- Add a `// Why:` comment on any `#[allow(...)]` so the next reader knows the trade-off.

## Reporting issues

Useful info to attach:

- **Distro + display server**: `lsb_release -ds`, `echo $XDG_SESSION_TYPE`.
- **Audio stack**: `pactl info | grep "Server"` and `cpal` startup log line (`mic: device=… native_rate=… channels=…`).
- **Per-press tracing line**: from `journalctl --user -t f9-talk -f`, the line with `press_to_release / first_byte_sent / release_to_final / transcript`.
- **The `f9_talk::press` log line** (`journalctl --user -t f9-talk -f`) — isolates STT pipeline vs typing issues.

## Releasing

1. Bump `[workspace.package].version` in `Cargo.toml`.
2. Add a section to `CHANGELOG.md`.
3. `cargo deb -p f9-talk` produces `target/debian/f9-talk_<version>-1_amd64.deb`.
4. Tag + push:
   ```bash
   git tag -a v0.X.Y -m "v0.X.Y"
   git push origin v0.X.Y
   ```
5. Create a GitHub release at the tag and attach the `.deb`.

## License

By contributing you agree your changes are released under the project's [MIT License](LICENSE).
