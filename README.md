<p align="center">
  <img src="assets/f9-talk-banner.png" alt="f9-talk" width="560" />
</p>

# f9-talk

[![Rust](https://img.shields.io/badge/Rust-1.78%2B-CE422B?logo=rust&logoColor=white)](https://www.rust-lang.org/) [![Platform](https://img.shields.io/badge/Platform-Linux%20(Wayland%20%2B%20X11)-FCC624?logo=linux&logoColor=black)](https://www.linux.org/) [![License](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

Hold-to-talk dictation for Linux. Press **F9**, speak, release — the
transcript types itself into whatever app you're focused on. Works
system-wide, in any text field. Powered by Deepgram Nova-3 streaming.

A single statically-linked Rust binary. On Wayland the indicator is a
native `wlr-layer-shell` voice wave; X11 is supported too.

## Install

The release ships a single self-contained **[AppImage](https://github.com/moamen1358/f9-talk/releases/latest)** —
the binary, its libraries, and the `wl-clipboard` / `wtype` typing tools,
all in one file. Five steps:

```bash
# 1. Download the latest f9-talk-*.AppImage from the releases page:
#    https://github.com/moamen1358/f9-talk/releases/latest
chmod +x f9-talk-*.AppImage

# 2. One-time setup — desktop integration, then uinput access (needs sudo):
./f9-talk-*.AppImage install --user         # apps menu + autostart
sudo ./f9-talk-*.AppImage install --system  # udev rule + adds you to the `input` group

# 3. Log out and back in once   ← so the `input`-group membership applies

# 4. Add a Deepgram API key (free tier: https://console.deepgram.com/signup):
echo 'DEEPGRAM_API_KEY=your_key_here' >> ~/.config/F9_talk/secrets.env

# 5. Run it — or just log back in, it autostarts:
./f9-talk-*.AppImage
```

Then **hold F9, speak, release** — the transcript types at your cursor.

The only host requirement is **`libfuse2`** (every AppImage needs it) —
`sudo apt install libfuse2` if it's missing. `f9-talk uninstall
[--user|--system]` reverses step 2; your API key is always kept.

### Build from source

```bash
git clone https://github.com/moamen1358/f9-talk.git
cd f9-talk
# One-time: Rust toolchain + Linux build deps (see docs/architecture.md).
./run.sh --build
```

## Use

Hold **F9**, speak, release. The transcript is typed at your cursor. A
red voice-wave appears at the bottom-center of the screen while you hold
F9, reacting to your voice.

## Options

| Command | Result |
|---|---|
| `f9-talk` | Run it — hold F9 to dictate |
| `f9-talk --indicator-margin 80` | Pixels the indicator sits above the bottom edge (default 20) |
| `f9-talk -v` | Verbose logging |
| `f9-talk install [--user\|--system]` | Set up desktop integration (apps menu, autostart, udev rule, `input` group) |
| `f9-talk uninstall [--user\|--system]` | Reverse it. Secrets are preserved. |

To make a flag permanent, edit `Exec=` in your autostart entry
(`~/.config/autostart/f9-talk.desktop`).

## Build, architecture, troubleshooting

See [docs/architecture.md](docs/architecture.md) for the workspace
layout, reliability mechanisms, build-from-source instructions, and the
troubleshooting table.

## License

MIT.
