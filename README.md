<p align="center">
  <img src="assets/f9-talk.png" alt="f9-talk" width="96" />
</p>

# f9-talk

[![Rust](https://img.shields.io/badge/Rust-1.78%2B-CE422B?logo=rust&logoColor=white)](https://www.rust-lang.org/) [![Platform](https://img.shields.io/badge/Platform-Linux%20%2B%20X11-FCC624?logo=linux&logoColor=black)](https://www.linux.org/) [![License](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

Hold-to-talk dictation for Linux. Press F9, speak, release — the
transcript types itself into whatever app you're focused on. Works
system-wide, on any text field. The default backend is Deepgram
Nova-3 streaming; an offline whisper.cpp backend is also available.

Single statically-linked Rust binary, distributed as a `.deb`.
Linux + X11 only for now.

## Install

Easiest path — install the prebuilt `.deb`:

```bash
# Download from https://github.com/moamen1358/f9-talk/releases/latest
sudo dpkg -i f9-talk_*.deb
sudo apt-get install -f
```

The `.deb` is fully automated (binary, apps menu, autostart, udev rule,
`input` group, secrets stub). The **AppImage** and **`curl | sh`**
(cargo-dist) paths leave the system untouched, so reach the same end
state with one extra `install` call:

```bash
# AppImage
./f9-talk-*.AppImage install --user        # apps menu, autostart, secrets stub
sudo ./f9-talk-*.AppImage install --system # udev rule + adds you to `input`

# cargo-dist
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/moamen1358/f9-talk/releases/latest/download/f9-talk-installer.sh | sh
f9-talk install --user
# `sudo` strips PATH; pass the absolute path or your $HOME for it to find the binary:
sudo "$(command -v f9-talk)" install --system
```

`f9-talk uninstall [--user|--system]` reverses either step; secrets are
always preserved. After any path, log out and back in once so the
`input` group membership takes effect.

Or run directly from this repo:

```bash
git clone https://github.com/moamen1358/f9-talk.git
cd f9-talk

# One-time: Rust toolchain + Linux build deps (see docs/architecture.md
# for the full apt install line)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y

# Build and launch (run.sh handles the input-group session quirk)
./run.sh --build
```

After install (either path), log out and back in once so the `input`
group membership takes effect, then right-click the tray icon →
**API Keys…** and paste a Deepgram key
([free tier here](https://console.deepgram.com/signup)).

## Use

Hold F9, speak, release. That's it.

The tray icon turns green when active, gray when paused, red after a
failed session. Left-click pauses or resumes; right-click opens the
menu.

## Common flags

| Command | Result |
|---|---|
| `f9-talk` | Default — Deepgram cloud STT |
| `f9-talk --backend local` | Offline whisper.cpp (downloads model on first press) |
| `f9-talk --backend both` | Run cloud and local in parallel for comparison |
| `f9-talk --target ar` | Translate to Arabic before typing |
| `f9-talk --keyword Anthropic --keyword kubectl` | Bias toward custom terms |
| `f9-talk --local-hotkey '<ctrl>+<alt>+space'` | Custom hotkey chord |

To make a flag permanent, edit `Exec=` in
`/etc/xdg/autostart/f9-talk.desktop`.

Run `f9-talk --help` for the full list.

## Build, architecture, troubleshooting

See [docs/architecture.md](docs/architecture.md) for the workspace
layout, reliability mechanisms, build-from-source instructions
(including CUDA), and the troubleshooting table.

## License

MIT.
