//! STT backend trait + two streaming implementations:
//! [`assemblyai::AssemblyAi`] (Universal-3.6 Pro Realtime, the default)
//! and [`deepgram::Deepgram`] (Nova-3, the fallback).
//!
//! Both implement the async [`Stt`] trait with a press/release-driven
//! session model:
//!
//! ```text
//!   start() ───► connection task + first session open
//!   begin_session()
//!     send_audio(frame) × N            (40 frames/sec × press duration)
//!   end_session(timeout)  ─► Final transcript (waits for the server's
//!                            reply to its finalize request; `timeout`
//!                            is only a safety net)
//!   stop()  (on app shutdown)
//! ```

#![forbid(unsafe_code)]

pub mod assemblyai;
pub mod deepgram;

#[cfg(test)]
mod test_support;

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
    /// Static label for log lines (e.g. `"assemblyai"`).
    fn name(&self) -> &'static str;

    /// Spawn the connection task and open the first session. Called
    /// once at app start.
    async fn start(&self, events: mpsc::Sender<BackendEvent>) -> Result<(), SttError>;

    /// Reset per-session state. Called on F9 press.
    async fn begin_session(&self);

    /// Forward a 25 ms PCM frame. Errors are absorbed (logged via
    /// `BackendEvent::Error`) to keep the audio thread fast.
    async fn send_audio(&self, pcm: &[u8]);

    /// Ask the server to finalize the press and wait for its final text
    /// (never a partial hypothesis). `timeout` is a safety net for a
    /// reply that never comes; then the best text so far is returned.
    /// Called on F9 release.
    async fn end_session(&self, timeout: Duration) -> SessionResult;

    /// Tear down the connection / unload the model. Called on app stop.
    async fn stop(&self);
}

/// Both cloud backends get 16 kHz mono s16le PCM: the audio crate
/// down-mixes and resamples the device's native stream to that.
pub const STT_SAMPLE_RATE: u32 = 16_000;
