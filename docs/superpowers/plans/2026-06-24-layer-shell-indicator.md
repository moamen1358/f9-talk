# Layer-Shell Voice Indicator — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the XWayland eframe indicator (which cosmic-comp mis-places top-center, gives a focus border, and squeezes into a 22px bar) with a Wayland-native `wlr-layer-shell` overlay surface that anchors bottom-center, takes no keyboard focus (no border), is click-through, and renders a clear voice-reactive pulse with real transparency.

**Architecture:** A dedicated thread owns a `smithay-client-toolkit` Wayland connection and a single `zwlr_layer_surface_v1` on the **overlay** layer, anchored **bottom** with a bottom margin, `keyboard-interactivity = none`, and an **empty input region** (click-through). It renders into a **wl_shm ARGB8888** buffer in software each frame (real per-pixel alpha, no GPU/EGL dependency), reading the existing shared `IndicatorState` (`rms`, `recording`). The eframe app is retained **only** for the keys dialog; its wave rendering is removed.

**Tech Stack:** Rust, `smithay-client-toolkit` (layer-shell + shm), `wayland-client`, existing `f9_talk_audio::RmsHandle`. Software rasterization (no wgpu/egui for the indicator).

## Global Constraints

- Platform: this surface is **Linux/Wayland only**. All new code lives behind `#[cfg(target_os = "linux")]` and is only spawned when `WAYLAND_DISPLAY` is set; otherwise the existing eframe indicator path remains (macOS/Windows/X11 unaffected).
- Do not regress the X11 path: under a real X11 session (`XDG_SESSION_TYPE=x11`, no `WAYLAND_DISPLAY`) keep using the eframe indicator.
- Reuse `IndicatorState` (`crates/ui/src/indicator.rs`) verbatim as the shared state contract; the audio callback already updates `rms`.
- The keys dialog (`crates/ui/src/keys_dialog.rs`) and tray (`crates/ui/src/tray.rs`) must keep working unchanged.
- Keep `cargo fmt`, `clippy` (default + no-default-features), and `cargo deny` green — these gate CI on `main`.

---

## File Structure

- **Create** `crates/ui/src/layer_indicator/mod.rs` — public entry: `spawn(state: Arc<IndicatorState>) -> std::thread::JoinHandle<()>`. Owns the Wayland event loop.
- **Create** `crates/ui/src/layer_indicator/surface.rs` — layer-shell surface lifecycle: registry binding, `LayerSurface` creation (overlay/bottom-anchor/margin/no-keyboard/empty-input-region), configure handling, show/hide via attach-buffer / null-buffer.
- **Create** `crates/ui/src/layer_indicator/render.rs` — pure software rasterizer: `fn paint_pulse(buf: &mut [u32], w: u32, h: u32, level: f32, anim_t: f32)` plus the testable geometry/color helpers. **No Wayland types here** so it unit-tests cleanly.
- **Create** `crates/ui/src/layer_indicator/level.rs` — `LevelSmoother` (asymmetric EMA), extracted from `IndicatorApp::update_smoothed_level` so both paths share one tested implementation.
- **Modify** `crates/ui/src/lib.rs` — export `layer_indicator`; add `pub use`.
- **Modify** `crates/ui/Cargo.toml` — add `smithay-client-toolkit`, `wayland-client` (Linux-only `[target.'cfg(...)'.dependencies]`).
- **Modify** `crates/ui/src/indicator.rs` — remove wave painting from `IndicatorApp::update` (keep keys-dialog + visibility plumbing); extract EMA into `level.rs`.
- **Modify** `crates/app/src/main.rs` — when on Wayland, `layer_indicator::spawn(state)` on its own thread and tell the eframe `IndicatorApp` to stay hidden (wave handled by the layer surface).
- **Create** `crates/ui/examples/layer_demo.rs` — standalone visual harness: opens the surface and animates a synthetic level sweep, so the surface can be validated **without** the input-group / hotkey machinery.

---

### Task 1: Level smoother extracted and unit-tested

**Files:**
- Create: `crates/ui/src/layer_indicator/level.rs`
- Test: same file (`#[cfg(test)]`)
- Modify: `crates/ui/src/indicator.rs:98-107` (delegate to the new type)

**Interfaces:**
- Produces: `pub struct LevelSmoother { level: f32 }` with `pub fn new() -> Self`, `pub fn push(&mut self, raw: f32) -> f32` (returns the new smoothed level), `pub fn level(&self) -> f32`.

- [ ] **Step 1: Write the failing test**
```rust
#[test]
fn rises_fast_falls_slow() {
    let mut s = LevelSmoother::new();
    let up = s.push(1.0);                 // from 0.0 with α=0.45
    assert!((up - 0.45).abs() < 1e-6);
    let down = s.push(0.0);               // from 0.45 with α=0.15 fall
    assert!((down - 0.3825).abs() < 1e-6);
}
```
- [ ] **Step 2: Run it, expect FAIL** — `cargo test -p f9-talk-ui level::tests::rises_fast_falls_slow`
- [ ] **Step 3: Implement** the asymmetric EMA (rise `0.55*l+0.45*raw`, fall `0.85*l+0.15*raw`, `raw.max(0.0)`).
- [ ] **Step 4: Run it, expect PASS.**
- [ ] **Step 5:** Replace the body of `IndicatorApp::update_smoothed_level` to delegate to `LevelSmoother`; `cargo test -p f9-talk-ui`.
- [ ] **Step 6: Commit** `refactor(ui): extract tested LevelSmoother (shared by both indicators)`

---

### Task 2: Software pulse rasterizer (pure, unit-tested)

**Files:**
- Create: `crates/ui/src/layer_indicator/render.rs`
- Test: same file

**Interfaces:**
- Produces: `pub const PULSE_W: u32`, `pub const PULSE_H: u32`; `pub fn paint_pulse(buf: &mut [u32], w: u32, h: u32, level: f32, anim_t: f32)` writing premultiplied ARGB8888 (`0xAARRGGBB`); helper `fn bar_height(i: usize, n: usize, level: f32, anim_t: f32) -> f32` returns 0..=1.

- [ ] **Step 1: Write failing tests** — buffer fully cleared to `0x00000000` when `level==0` and `anim_t==0` produces only transparent or centred-baseline pixels (no pixel has alpha>0 outside the centre row); `bar_height` is monotonic in `level` for a fixed `i`,`anim_t`; output alpha is 0 in the corner pixel `buf[0]`.
- [ ] **Step 2: Run, expect FAIL.**
- [ ] **Step 3: Implement** a symmetric row of `N` rounded vertical bars (a graphic-equaliser pulse): each bar's height = `baseline + level*envelope(i)*wiggle(anim_t,i)`, colour red→bright with alpha falloff at the bar edges; background stays `0x00000000` (transparent). Premultiply alpha.
- [ ] **Step 4: Run, expect PASS.**
- [ ] **Step 5: Commit** `feat(ui): software ARGB pulse rasterizer`

---

### Task 3: Layer-shell surface that shows a static frame

**Files:**
- Create: `crates/ui/src/layer_indicator/surface.rs`, `crates/ui/src/layer_indicator/mod.rs`
- Create: `crates/ui/examples/layer_demo.rs`
- Modify: `crates/ui/Cargo.toml`, `crates/ui/src/lib.rs`

**Interfaces:**
- Produces: `pub fn spawn(state: std::sync::Arc<crate::IndicatorState>) -> std::thread::JoinHandle<()>`; internal `struct LayerIndicator` implementing the sctk `LayerShellHandler`/`ShmHandler` traits.

- [ ] **Step 1:** Add deps; create the surface: `Layer::Overlay`, `anchor = BOTTOM`, `set_margin(0,0,80,0)`, `set_size(PULSE_W, PULSE_H)`, `set_keyboard_interactivity(None)`, and set an **empty `wl_region` as input region** (click-through). Commit the surface; on first `configure`, draw one `paint_pulse(level=0.4)` frame into a wl_shm buffer and attach.
- [ ] **Step 2: Visual verify (manual)** — `cargo run -p f9-talk-ui --example layer_demo`. **Gate:** user confirms a pulse appears **bottom-center**, **no window border**, **no focus stolen** (typing in another app still works), click passes through.
- [ ] **Step 3: Commit** `feat(ui): wlr-layer-shell overlay surface (bottom-center, click-through)`

---

### Task 4: Animate from live state; show only while recording

**Files:**
- Modify: `crates/ui/src/layer_indicator/mod.rs`, `surface.rs`
- Modify: `crates/ui/examples/layer_demo.rs` (sweep synthetic level)

**Interfaces:**
- Consumes: `IndicatorState.recording` (`Arc<Mutex<bool>>`), `IndicatorState.rms` (`RmsHandle`), `LevelSmoother`.

- [ ] **Step 1:** Event loop: when `*recording` is true, request frames at ~60fps (via `wl_surface.frame` callbacks), each frame `push` the live `rms` through `LevelSmoother` and repaint. When false, attach a null buffer (hide) and idle on a short timer.
- [ ] **Step 2: Visual verify (manual)** in the real app: rebuild, `./run.sh`, hold F9 and speak — **Gate:** the pulse visibly tracks the voice (big on loud, small on quiet) and disappears on release.
- [ ] **Step 3: Commit** `feat(ui): drive layer pulse from live mic level`

---

### Task 5: Wire into the app; retire the eframe wave

**Files:**
- Modify: `crates/app/src/main.rs:136-185` (spawn), `crates/ui/src/indicator.rs:142-209` (drop wave paint)

- [ ] **Step 1:** In `main.rs`, compute `on_wayland`; if true, `let _ind = f9_talk_ui::layer_indicator::spawn(indicator_state.clone());` and make the eframe `IndicatorApp` never set itself visible (keys dialog still works). If not Wayland, keep the eframe wave.
- [ ] **Step 2:** Remove the `paint_wave`/`build_wave_path` call from `IndicatorApp::update` (leave the functions or delete if now unused — clippy must stay clean).
- [ ] **Step 3: Visual verify (manual):** rebuild + `./run.sh`; confirm exactly one indicator (the layer pulse), bottom-center, no border; keys dialog from the tray still opens.
- [ ] **Step 4:** `cargo fmt --all`, `cargo clippy --all-targets`, `cargo clippy --no-default-features`; all green.
- [ ] **Step 5: Commit** `feat(ui): use layer-shell indicator on Wayland; retire XWayland wave`

---

## Self-Review Notes

- **Spec coverage:** position (Task 3 anchor+margin), border/focus (Task 3 keyboard-interactivity none + overlay), voice pulse (Tasks 2+4), don't-regress-X11 (Task 5 `on_wayland` gate). Accuracy is a **separate** change (already done via the `wtype` typer path) — not in this plan.
- **Open visual decisions deferred to the screenshot:** exact bar count, palette (keep red?), size, bottom margin. These are constants in `render.rs`/`surface.rs`, cheap to tune in Task 3/4.
- **Risk:** sctk frame-callback cadence + show/hide correctness is the main integration risk; the `layer_demo` example de-risks it before app integration.
