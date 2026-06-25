# f9-talk logos

10 candidate app logos generated with **gpt-image-2** (via Codex's `$imagegen` tool) — hand-painted red brush marks, cut to transparent backgrounds.

- `logo-01.png` … `logo-10.png` — the transparent, despilled cutouts.
- **`logo-01.png` is the chosen app icon** (the bold brush "F9"). It's wired into `assets/f9-talk.png`, `assets/f9-talk.svg`, the README banner, and the embedded/installed icon.
- `source/` (gitignored) — the raw generations on a green chroma-key background.

Concepts: 1 bold F9 · 2 diagonal F9 · 3 single F · 4 mic · 5 mic+bars · 6 waveform · 7 F9 emblem · 8 speech bubble · 9 nine+tail · 10 voice burst.

To regenerate or add logos, see the image-gen notes in the project `CLAUDE.md` — Codex needs `--sandbox danger-full-access`, generate on flat green `#00FF00`, then despill with `remove_chroma_key.py`.
