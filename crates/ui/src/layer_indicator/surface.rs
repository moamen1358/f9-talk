//! `wlr-layer-shell` overlay surface lifecycle + software draw loop.
//!
//! Anchored bottom-center, overlay layer, `keyboard-interactivity = none`
//! and an empty input region (click-through). The compositor therefore
//! never focuses it — so there is no focus/accent border — and clicks
//! pass straight through to whatever is underneath.

use std::sync::Arc;
use std::time::Instant;

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, Region},
    delegate_compositor, delegate_layer, delegate_output, delegate_pointer, delegate_registry,
    delegate_seat, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        pointer::{PointerEvent, PointerEventKind, PointerHandler, BTN_LEFT},
        Capability, SeatHandler, SeatState,
    },
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use tracing::{info, warn};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
    Connection, QueueHandle,
};

use super::level::LevelSmoother;
use super::render::{paint_close, paint_dot, paint_wave, LEVEL_GAIN, WAVE_H, WAVE_W};
use crate::indicator::IndicatorState;

/// Default gap above the bottom screen edge, in px. Overridable per-run
/// via the `--indicator-margin` flag.
pub const DEFAULT_BOTTOM_MARGIN: i32 = 20;
/// Below this smoothed level after release the wave has settled, so we
/// switch from the decaying wave back to the idle "ready" dot.
const HIDE_BELOW: f32 = 0.02;

/// Small clickable hotspot centred over the dot. The rest of the surface
/// stays click-through; clicking this hotspot quits the tool. Sized to
/// roughly match the dot's glow so the cursor only "catches" on the dot.
const HOTSPOT_W: i32 = 26;
const HOTSPOT_H: i32 = 26;
const HOTSPOT_X: i32 = (WAVE_W as i32 - HOTSPOT_W) / 2;
const HOTSPOT_Y: i32 = (WAVE_H as i32 - HOTSPOT_H) / 2;

/// Connect to Wayland and run the indicator event loop. `bottom_margin`
/// is the gap (px) above the bottom screen edge. Blocks until the
/// surface is closed; intended to own a dedicated thread.
pub fn run(state: Arc<IndicatorState>, bottom_margin: i32) -> anyhow::Result<()> {
    let conn = Connection::connect_to_env()
        .map_err(|e| anyhow::anyhow!("layer indicator: no Wayland connection: {e}"))?;
    let (globals, mut event_queue) = registry_queue_init(&conn)
        .map_err(|e| anyhow::anyhow!("layer indicator: registry init failed: {e}"))?;
    let qh = event_queue.handle();

    let compositor = CompositorState::bind(&globals, &qh)
        .map_err(|e| anyhow::anyhow!("wl_compositor unavailable: {e}"))?;
    let layer_shell = LayerShell::bind(&globals, &qh)
        .map_err(|e| anyhow::anyhow!("wlr-layer-shell unavailable: {e}"))?;
    let shm = Shm::bind(&globals, &qh).map_err(|e| anyhow::anyhow!("wl_shm unavailable: {e}"))?;

    let (layer, region) = build_layer(&compositor, &layer_shell, &qh, bottom_margin)?;

    let pool = SlotPool::new((WAVE_W * WAVE_H * 4) as usize, &shm)
        .map_err(|e| anyhow::anyhow!("shm pool alloc failed: {e}"))?;

    let mut app = LayerIndicator {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        seat_state: SeatState::new(&globals, &qh),
        pointer: None,
        hovered: false,
        shm,
        pool,
        layer,
        _region: region,
        compositor,
        layer_shell,
        bottom_margin,
        state,
        smoother: LevelSmoother::new(),
        anim_t0: Instant::now(),
        scratch: vec![0u32; (WAVE_W * WAVE_H) as usize],
        width: WAVE_W,
        height: WAVE_H,
        first_configure: true,
        last_recording: false,
        needs_rebuild: false,
        exit: false,
    };

    loop {
        event_queue.blocking_dispatch(&mut app)?;
        if app.exit {
            break;
        }
        // A new dictation just started: re-place the surface so the
        // compositor maps it onto the now-focused output.
        if app.needs_rebuild {
            app.rebuild(&qh);
        }
    }
    Ok(())
}

/// Create a fresh bottom-anchored, click-through, focusless layer surface.
/// With `output = None` the compositor maps it onto the active output, so
/// rebuilding at dictation time follows the user's focused monitor.
fn build_layer(
    compositor: &CompositorState,
    layer_shell: &LayerShell,
    qh: &QueueHandle<LayerIndicator>,
    bottom_margin: i32,
) -> anyhow::Result<(LayerSurface, Region)> {
    let surface = compositor.create_surface(qh);
    let layer = layer_shell.create_layer_surface(
        qh,
        surface,
        Layer::Overlay,
        Some("f9-talk-indicator"),
        None,
    );
    layer.set_anchor(Anchor::BOTTOM);
    layer.set_size(WAVE_W, WAVE_H);
    layer.set_margin(0, 0, bottom_margin.max(0), 0);
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    // Click-through everywhere except a small hotspot over the dot, so the
    // dot doubles as a "quit" button while the rest passes clicks through.
    let region = Region::new(compositor)
        .map_err(|e| anyhow::anyhow!("could not create input region: {e}"))?;
    region.add(HOTSPOT_X, HOTSPOT_Y, HOTSPOT_W, HOTSPOT_H);
    layer.set_input_region(Some(region.wl_region()));
    layer.commit();
    Ok((layer, region))
}

struct LayerIndicator {
    registry_state: RegistryState,
    output_state: OutputState,
    seat_state: SeatState,
    // Pointer for the clickable dot; created when the seat advertises one.
    pointer: Option<wl_pointer::WlPointer>,
    // True while the cursor is over the dot's hotspot — draws the "×".
    hovered: bool,
    shm: Shm,
    pool: SlotPool,
    layer: LayerSurface,
    // Kept alive so the click-through input region stays valid.
    _region: Region,
    // Kept so the surface can be rebuilt on the focused output per press.
    compositor: CompositorState,
    layer_shell: LayerShell,
    bottom_margin: i32,
    state: Arc<IndicatorState>,
    smoother: LevelSmoother,
    anim_t0: Instant,
    scratch: Vec<u32>,
    width: u32,
    height: u32,
    first_configure: bool,
    last_recording: bool,
    needs_rebuild: bool,
    exit: bool,
}

impl LayerIndicator {
    /// Tear down the current surface and build a fresh one. The
    /// compositor re-maps the new (`output = None`) surface onto the
    /// active output, which is the one holding keyboard focus — i.e. the
    /// monitor the user just pressed F9 on.
    fn rebuild(&mut self, qh: &QueueHandle<Self>) {
        self.needs_rebuild = false;
        match build_layer(&self.compositor, &self.layer_shell, qh, self.bottom_margin) {
            Ok((layer, region)) => {
                self.layer = layer; // old surface dropped → destroyed
                self._region = region;
                self.first_configure = true;
                self.width = WAVE_W;
                self.height = WAVE_H;
                // The old surface is gone, so its hover is meaningless; the
                // new surface re-enters if the cursor is actually over it.
                self.hovered = false;
            }
            Err(e) => warn!("layer indicator: surface rebuild failed: {e}"),
        }
    }

    fn draw(&mut self, qh: &QueueHandle<Self>) {
        let recording = *self.state.recording.lock();
        // Rising edge of a dictation → re-place onto the focused monitor.
        // Skip this frame; the loop rebuilds and the new surface redraws.
        if recording && !self.last_recording {
            self.last_recording = recording;
            self.needs_rebuild = true;
            return;
        }
        self.last_recording = recording;

        let raw = if recording {
            *self.state.rms.lock()
        } else {
            0.0
        };
        // Smooth the raw mic RMS, then lift it into a 0..1 display range
        // (speech RMS is ~0.05–0.1). Mirrors the old wave's amplification.
        let level = (self.smoother.push(raw) * LEVEL_GAIN).clamp(0.0, 1.0);
        let anim_t = self.anim_t0.elapsed().as_secs_f32();

        // Wave while held (plus a short decay tail after release); once it
        // settles, the idle "ready" dot — or, while the cursor hovers it,
        // the "×" close affordance — so the user sees the tool is alive and
        // knows a click on the dot quits it.
        if recording || self.smoother.level() > HIDE_BELOW {
            paint_wave(&mut self.scratch, self.width, self.height, level, anim_t);
        } else if self.hovered {
            paint_close(&mut self.scratch, self.width, self.height);
        } else {
            paint_dot(&mut self.scratch, self.width, self.height, anim_t);
        }

        let stride = self.width as i32 * 4;
        let (buffer, canvas) = match self.pool.create_buffer(
            self.width as i32,
            self.height as i32,
            stride,
            wl_shm::Format::Argb8888,
        ) {
            Ok(pair) => pair,
            Err(e) => {
                warn!("layer indicator: buffer alloc failed: {e}");
                return;
            }
        };
        for (px, chunk) in self.scratch.iter().zip(canvas.chunks_exact_mut(4)) {
            chunk.copy_from_slice(&px.to_le_bytes());
        }

        let wl_surface = self.layer.wl_surface().clone();
        wl_surface.damage_buffer(0, 0, self.width as i32, self.height as i32);
        // Keep the animation ticking by always requesting the next frame.
        wl_surface.frame(qh, wl_surface.clone());
        if let Err(e) = buffer.attach_to(&wl_surface) {
            warn!("layer indicator: attach failed: {e}");
            return;
        }
        self.layer.commit();
    }
}

impl CompositorHandler for LayerIndicator {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {
        self.draw(qh);
    }
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl LayerShellHandler for LayerIndicator {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        let (w, h) = configure.new_size;
        self.width = if w == 0 { WAVE_W } else { w };
        self.height = if h == 0 { WAVE_H } else { h };
        let need = (self.width * self.height) as usize;
        if self.scratch.len() != need {
            self.scratch.resize(need, 0);
        }
        if self.first_configure {
            self.first_configure = false;
            self.draw(qh);
        }
    }
}

impl OutputHandler for LayerIndicator {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl SeatHandler for LayerIndicator {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer && self.pointer.is_none() {
            match self.seat_state.get_pointer(qh, &seat) {
                Ok(pointer) => self.pointer = Some(pointer),
                Err(e) => warn!("layer indicator: could not get pointer: {e}"),
            }
        }
    }
    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer {
            if let Some(pointer) = self.pointer.take() {
                pointer.release();
            }
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl PointerHandler for LayerIndicator {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            // Only our surface has an input region, but guard anyway.
            if &event.surface != self.layer.wl_surface() {
                continue;
            }
            match event.kind {
                PointerEventKind::Enter { .. } => self.hovered = true,
                PointerEventKind::Leave { .. } => self.hovered = false,
                PointerEventKind::Press { button, .. } if button == BTN_LEFT => {
                    // Clicking the dot is the tool's "quit" gesture. Exit the
                    // whole process; the instance lock releases on exit and
                    // the user relaunches from the apps menu.
                    info!("f9-talk: quitting (indicator dot clicked)");
                    std::process::exit(0);
                }
                _ => {}
            }
        }
    }
}

impl ShmHandler for LayerIndicator {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for LayerIndicator {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

delegate_compositor!(LayerIndicator);
delegate_output!(LayerIndicator);
delegate_shm!(LayerIndicator);
delegate_layer!(LayerIndicator);
delegate_seat!(LayerIndicator);
delegate_pointer!(LayerIndicator);
delegate_registry!(LayerIndicator);
