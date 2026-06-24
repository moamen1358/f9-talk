//! STT backend trait + Deepgram Nova-3 streaming impl.
//!
//! [`deepgram::Deepgram`] implements the async [`Stt`] trait with a
//! press/release-driven session model:
//!
//! ```text
//!   start() ───► persistent WS open
//!   begin_session()
//!     send_audio(frame) × N            (40 frames/sec × press duration)
//!   end_session(timeout)  ─► Final transcript
//!   stop()  (on app shutdown)
//! ```

#![forbid(unsafe_code)]

pub mod deepgram;

use std::time::Duration;

use async_trait::async_trait;
use thiserror::Error;
use tokio::sync::mpsc;

/// Events the active backend pushes to the app independently of any
/// individual press (connection loss, recovery, async errors). Per-session
/// finals are returned synchronously by [`Stt::end_session`].
#[derive(Debug, Clone)]
pub enum BackendEvent {
    /// Connection (or local-model load) lost. UI may show error tint.
    SocketLost(String),
    /// Connection re-established after a previous loss.
    SocketBack,
    /// Async error inside the backend that doesn't tear down the
    /// connection (e.g. one bad message, send failure).
    Error(String),
}

/// What [`Stt::end_session`] returns: the final transcript (may be empty)
/// plus the latency from end-of-press to receiving all finals.
#[derive(Debug, Clone)]
pub struct SessionResult {
    pub transcript: String,
    pub finalize_latency: Duration,
}

#[derive(Debug, Error)]
pub enum SttError {
    #[error("API key not configured for {0}")]
    MissingKey(&'static str),
    #[error("network error: {0}")]
    Network(String),
    #[error("backend protocol error: {0}")]
    Protocol(String),
    #[error("backend not started")]
    NotStarted,
    #[error("internal: {0}")]
    Internal(String),
}

/// The unified backend interface. Methods MUST be cheap; reconnect /
/// keepalive logic belongs inside the implementation.
#[async_trait]
pub trait Stt: Send + Sync {
    /// Static label for log lines (e.g. `"deepgram"`).
    fn name(&self) -> &'static str;

    /// Open the persistent WebSocket / load the model. Called once at
    /// app start.
    async fn start(&self, events: mpsc::Sender<BackendEvent>) -> Result<(), SttError>;

    /// Reset per-session state. Called on F9 press.
    async fn begin_session(&self);

    /// Forward a 25 ms PCM frame. Errors are absorbed (logged via
    /// `BackendEvent::Error`) to keep the audio thread fast.
    async fn send_audio(&self, pcm: &[u8]);

    /// Wait up to `timeout` for any straggler finals, then return the
    /// concatenated transcript. Called on F9 release.
    async fn end_session(&self, timeout: Duration) -> SessionResult;

    /// Tear down the connection / unload the model. Called on app stop.
    async fn stop(&self);
}

/// The cloud backend requires 16 kHz mono s16le PCM. The cpal streamer
/// gives us the device's native sample rate (often 44.1 / 48 kHz). We
/// declare 16 kHz to Deepgram via the `sample_rate` query param and let
/// the server resample. M2 will do client-side resampling once we add a
/// deterministic resampler crate.
pub const STT_SAMPLE_RATE: u32 = 16_000;
