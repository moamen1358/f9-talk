# f9-talk — project context for Claude

Hold **F9**, speak, release → the Deepgram Nova-3 transcript types at the cursor. That's the whole tool. Linux only (Wayland/COSMIC is the primary target, X11 supported). Single statically-linked Rust binary, Cargo workspace.

## Current state (2026-06-25)
- **v0.7.1**, shipped. Repo `https://github.com/moamen1358/f9-talk`. Work happens on branch `feat/lean-deepgram-dictation`, fast-forwarded into `main` at each release.
- **Indicator** (`crates/ui/src/layer_indicator/`, Wayland `wlr-layer-shell`, software-rendered): a breathing red **dot** at bottom-center when idle (shows the tool is alive) → morphs to the red voice **wave** while F9 is held → hover the dot to get a red **×** and click it to **quit** the app.
- **Logo**: hand-painted red brush **"F9"** — `logos/logo-01.png`, AI-generated with gpt-image-2. It is the app icon (`assets/f9-talk.png` + SVG wrapper `assets/f9-talk.svg`), the README banner, and is embedded in the binary via `include_bytes!` in `crates/app/src/install.rs`; `f9-talk install --user` writes it into the hicolor icon theme.

## Layout
- Crates: `core, input, audio, stt, ui, app`. Entry point `crates/app/src/main.rs`; desktop integration `crates/app/src/install.rs`.
- `logos/` — 10 AI logo candidates (transparent); `logos/source/` = raw green-key originals (gitignored). #1 is the chosen mark.
- `docs/architecture.md` — deeper design/reliability notes.

## Build / run / release / install
- **Build**: `cargo build --release --bin f9-talk` (or `./run.sh --build`).
- **Run** (needs `/dev/uinput`, i.e. the `input` group — the working shell often isn't in it): launch via `sg input -c 'exec env RUST_LOG=info ./target/release/f9-talk -v'`. Single-instance lock is an abstract unix socket; on restart it can linger — if it says "already running" but nothing's visible, an old `AppRun` PID (reparented to init) is squatting the lock; kill it, busy-wait, relaunch.
- **Release**: bump `Cargo.toml` version + add a `CHANGELOG.md` entry → commit → `git push origin <branch>` + `git push origin HEAD:main` → `git tag vX.Y.Z && git push origin vX.Y.Z`. The tag triggers the cargo-dist **Release** workflow (Linux-only) then the **AppImage** workflow; watch both with `gh run watch <id> --repo moamen1358/f9-talk --exit-status`. **CI flake**: the Release job sometimes fails at `apt-get update` with a Microsoft apt-repo **403** — transient, just `gh run rerun <id> --repo moamen1358/f9-talk --failed`.
- **Install/update on the laptop**: download the latest AppImage to `~/Applications/f9-talk.AppImage`, `chmod +x`, run `APPIMAGE_EXTRACT_AND_RUN=1 ./f9-talk.AppImage install --user` (writes desktop entry + autostart + icon, keeps `~/.config/F9_talk/secrets.env`).

## Gotchas (non-obvious)
- **Codex image generation** (gpt-image-2, the `$imagegen` skill) only works headlessly with **full network**: `codex exec --sandbox danger-full-access` (absolute path `/home/moamen/.local/bin/codex`). With `workspace-write` the image tool silently fails and Codex fakes it (reuses `~/.codex/generated_images/` cache or draws with Cairo). Generate on flat green `#00FF00`, then despill via `~/.codex/skills/.system/imagegen/scripts/remove_chroma_key.py --auto-key border --soft-matte --despill`, run with `uv run --with pillow` (no system pip). Codex auth is ChatGPT-OAuth — no `OPENAI_API_KEY`, so the `image_gen.py` CLI fallback is unusable.
- **Secrets**: `~/.config/F9_talk/secrets.env` holds `DEEPGRAM_API_KEY`. The installer seeds it with an *uncommented* placeholder and `load_secrets` takes the FIRST occurrence — set the key by **overwriting** the file (`>`), never appending (`>>`).
- **Drives**: the live project is on the **`moamen`** drive (`/media/moamen/moamen/F9_talk`). An OLD copy lives on the **`inVisA11`** drive, usually unmounted — if an editor shows an empty `/media/moamen/inVisA11/F9_talk`, that's the stale one.
- **Screenshots on COSMIC**: `grim` fails (no wlr-screencopy); use `cosmic-screenshot --interactive=false --save-dir <dir>`.
- **Tessera** (`~/Desktop/Tessera`) is the user's other app; its brush-"T" logo (made with Adobe Firefly) is the quality bar referenced for f9-talk's logo.
