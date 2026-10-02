# Security Policy

## Supported Versions

| Version | Supported |
|---|---|
| 0.1.x | Yes |

## Scope

f9-talk handles sensitive resources:

- **Microphone audio** — captured locally via PulseAudio/PipeWire and streamed over TLS to the chosen speech-to-text service (AssemblyAI by default, or Deepgram). Audio is never written to disk.
- **API keys** — stored in `~/.config/F9_talk/secrets.env` (mode `600`). Never logged or transmitted beyond the intended service.
- **Keystroke injection** — text is injected via `xdotool` after transcription. No keystrokes are intercepted or logged beyond the hotkey trigger.

## Reporting a Vulnerability

If you discover a security vulnerability, **do not open a public issue**.

Email: **moamen.ghareeb.11@gmail.com**

Include:
- A description of the vulnerability and its potential impact
- Steps to reproduce
- Any suggested mitigation

You will receive an acknowledgement within 48 hours. Fixes are prioritised and released as patch versions.

## Out of Scope

- Vulnerabilities in third-party services (AssemblyAI, Deepgram, PulseAudio)
- Issues requiring physical access to the machine
- Social engineering
