<p align="center">
  <img src="assets/f9-talk-banner.png" alt="f9-talk" width="560" />
</p>

# F9 Talk

[![Rust](https://img.shields.io/badge/Rust-1.85%2B-CE422B?logo=rust&logoColor=white)](https://www.rust-lang.org/) [![Platform](https://img.shields.io/badge/Platform-Linux%20(Wayland%20%2B%20X11)-FCC624?logo=linux&logoColor=black)](https://kernel.org/) [![License](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

Hold-to-talk dictation for Linux.
Hold **F9**, speak, release: your words are typed into the app you are working in, in browsers, editors, chat apps and terminals.

- **Accurate and fast.** Streams to AssemblyAI Universal-3.6 Pro Realtime (the default) or Deepgram Nova-3. The text lands about 0.2 to 0.4 s after you release F9, and a sentence is never cut off at the end.
- **Your words, spelled right.** A key-terms list for names, products and jargon (GitHub, Kubernetes, `.env`, your colleagues' names).
- **One clean sentence per press.** Pause to think mid-sentence and it still comes back as one naturally punctuated sentence.
- **Bring your own key.** No account, no F9 Talk server: your audio goes from your computer straight to the service you pick.
- **Small.** One Rust binary, shipped as a single AppImage. A red dot at the bottom of the screen shows it is running.

## Bring your own key

F9 Talk uses your own speech-to-text API key.
Both services give free credit when you sign up, so you can try it without paying (amounts as listed on their pricing pages in October 2026):

| Service | Free to start | Get a key |
|---|---|---|
| AssemblyAI Universal-3.6 Pro (default) | $50 of free credit, no card needed | [assemblyai.com](https://www.assemblyai.com/dashboard/signup) |
| Deepgram Nova-3 | $200 of free credit | [deepgram.com](https://console.deepgram.com/signup) |

On first run the Settings window opens by itself: paste your key, press **Test key**, then **Save**.

<p align="center">
  <img src="assets/settings-window.png" alt="The F9 Talk Settings window on first run" width="460" />
</p>

AssemblyAI bills streaming by how long the connection is open (about $0.45 an hour), so F9 Talk closes it 60 s after your last dictation and reopens it on the next press, without losing a word.
Prices and free credit are the services' own and can change: check [AssemblyAI pricing](https://www.assemblyai.com/pricing) and [Deepgram pricing](https://deepgram.com/pricing).

## Supported systems

- **Used every day and tested on Pop!_OS 24.04 with the COSMIC desktop (Wayland).**
- **Other Wayland desktops** (KDE Plasma, Sway, Hyprland, GNOME): supported in the code, not tested yet.
  The F9 key is read from the keyboard device and text is pasted through a virtual keyboard, both below the desktop, so these parts do not depend on it.
  The red dot needs the `wlr-layer-shell` protocol, which KDE Plasma, Sway and Hyprland have and GNOME does not: on GNOME there is no dot, dictation still works, and Settings opens from the apps menu.
- **X11:** supported in the code, less tested. The voice wave is a small always-on-top window that does not take clicks (use the apps menu for Settings).
  Text is typed with `xdotool` when it is installed (`sudo apt install xdotool`), otherwise pasted through the clipboard.
- **Requirements:** a 64-bit x86 PC, a microphone, an internet connection, and glibc 2.35 or newer (Ubuntu 22.04, Debian 12, Fedora 36 or later).
  The AppImage also needs FUSE (`fusermount`, from the `fuse3` package that most desktops already have); without it, run it with `--appimage-extract-and-run`.

On Wayland the text is pasted with **Ctrl+Shift+V**, which terminals, browsers and most editors treat as paste; your previous clipboard text is put back afterwards.
An app that uses Ctrl+Shift+V for something else will not receive the text.

## Install

The release is one self-contained **[AppImage](https://github.com/moamen1358/f9-talk/releases/latest)**: the program, its libraries and the `wl-clipboard` / `wtype` typing tools in one file.

```bash
# 1. Download the latest AppImage into ~/Applications:
mkdir -p ~/Applications && cd ~/Applications
curl -fL -o f9-talk.AppImage "$(curl -fsSL \
  https://api.github.com/repos/moamen1358/f9-talk/releases/latest \
  | grep browser_download_url | grep '\.AppImage' | cut -d'"' -f4)"
chmod +x f9-talk.AppImage

# 2. One-time setup: desktop integration, then keyboard access (needs sudo):
./f9-talk.AppImage install --user         # apps menu, autostart, icon
sudo ./f9-talk.AppImage install --system  # udev rule + adds you to the `input` group

# 3. Log out and back in once, so the `input` group membership applies.

# 4. Start it from the apps menu (or ./f9-talk.AppImage). It autostarts
#    at every login from now on. The first time, Settings opens: add a key.
```

Then **hold F9, speak, release**.

`./f9-talk.AppImage uninstall [--user|--system]` reverses step 2; your keys and settings are kept.

### Build from source

```bash
git clone https://github.com/moamen1358/f9-talk.git
cd f9-talk
# One-time: Rust toolchain + Linux build deps (see docs/architecture.md).
./run.sh --build
```

## Use

Hold **F9**, speak, release: the text is typed at your cursor.

A small **red dot** sits at the bottom-center of the screen while F9 Talk runs.
While you hold F9 it turns into a red **voice wave** that follows your voice, then settles back into the dot.

- **Settings:** right-click the dot, or choose *F9 Talk > Settings* in the apps menu (launching F9 Talk again while it runs also opens Settings).
- **Quit:** hover the dot (it turns into a red **x**) and left-click it. Start it again from the apps menu, or log in again.
  On X11 (no clickable dot), quit with `pkill -f f9-talk`.

## Settings

The Settings window covers everything:

| Setting | What it does |
|---|---|
| Service | AssemblyAI Universal-3.6 Pro (default) or Deepgram Nova-3. |
| API keys | One per service, masked, with **Show** and **Test key** (one free request that answers "works" or the service's exact error). |
| Language | English (default), Spanish, French, German, Italian or Portuguese. |
| Key terms | Names and jargon to always spell right, one per line, up to 100. |
| Connection | How long the AssemblyAI connection stays open after a dictation (60 s; 0 keeps it always open). |

**Save** applies the changes at once; there is nothing to restart.

**Where things are kept:**

- Keys saved in Settings go into your desktop keyring (GNOME Keyring, KWallet, or any Secret Service), not a plain file.
  Without a keyring they go into `~/.config/F9_talk/secrets.env`, readable only by you.
- `ASSEMBLYAI_API_KEY` and `DEEPGRAM_API_KEY` in the environment still work, and win over saved keys.
- The other settings are plain text in `~/.config/F9_talk/`: `config.toml` (service, language, timings) and `keyterms.txt` (one term per line).
  You can edit them by hand too; restart F9 Talk afterwards.
  For example, switching to Deepgram is one line in `config.toml`: `backend = "deepgram"`.

## Command line

With the AppImage, `f9-talk` below means `~/Applications/f9-talk.AppImage`.

| Command | Result |
|---|---|
| `f9-talk` | Run it: hold F9 to dictate |
| `f9-talk settings` | Open the Settings window |
| `f9-talk --indicator-margin 80` | Pixels the dot sits above the bottom edge (default 20) |
| `f9-talk -v` | Verbose logging |
| `f9-talk install [--user\|--system]` | Desktop integration (apps menu, autostart, udev rule, `input` group) |
| `f9-talk uninstall [--user\|--system]` | Reverse it. Keys and settings are kept. |

To make a flag permanent, edit `Exec=` in `~/.config/autostart/f9-talk.desktop`.

## Privacy

Audio leaves your computer only while F9 is held: it is streamed over TLS to the service you chose and never written to disk.
Keys are never logged.
Each dictation's text is logged to your own user journal (`journalctl --user -t f9-talk`), which stays on your computer.
See [SECURITY.md](SECURITY.md).

## Build, architecture, troubleshooting

See [docs/architecture.md](docs/architecture.md) for the workspace layout, how release and reconnects work, build-from-source instructions and the troubleshooting table.

## License

MIT.
