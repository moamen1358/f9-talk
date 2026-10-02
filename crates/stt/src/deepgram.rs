//! Deepgram Nova-3 streaming over `tokio-tungstenite`.
//!
//! Mirrors the auto-reconnect logic shipped in v0.3.1
//! (`f9_talk/stt/deepgram.py:_reconnect_loop`):
//! - Persistent WS, kept alive across many F9 presses.
//! - Auto-reconnect on close + on three consecutive send failures.
//! - `stop()` sets a shutdown flag so clean teardown won't trigger reconnect spam.
//!
//! Wire protocol (Listen WebSocket API):
//! - URL: `wss://api.deepgram.com/v1/listen?<params>`
//! - Auth: `Authorization: Token <key>` header
//! - Send: raw int16 PCM bytes as binary WS frames
//! - Send: `{"type":"Finalize"}` text frame to force the server to emit its final transcript;
//!   the reply to it carries `"from_finalize": true` (sent even when it is empty)
//! - Send: `{"type":"KeepAlive"}` text frame periodically (every 8 s)
//! - Receive: JSON `{ "type": "Results", "is_final": true|false,
//!                    "from_finalize": true|false,
//!                    "channel": { "alternatives": [{ "transcript": "..." }] } }`
//! - Nova-3 boosts words with `keyterm=`; the older `keywords=` param is
//!   rejected with HTTP 400 on Nova-3, so it is never sent.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde::Deserialize;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{http::HeaderValue, Message};
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use tracing::{debug, info, trace, warn};
use url::Url;

use crate::{BackendEvent, SessionResult, Stt, SttError, STT_SAMPLE_RATE};

pub const DEFAULT_ENDPOINT: &str = "wss://api.deepgram.com/v1/listen";
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(8);
const RECONNECT_INITIAL: Duration = Duration::from_secs(1);
const RECONNECT_CAP: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct Config {
    /// Listen WebSocket endpoint (overridden by tests with a local mock).
    pub endpoint: String,
    pub model: String,
    pub language: String,
    /// Names and jargon to boost (`keyterm` query param).
    pub keyterms: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            endpoint: DEFAULT_ENDPOINT.into(),
            model: "nova-3".into(),
            language: "en".into(),
            keyterms: vec![],
        }
    }
}

pub struct Deepgram {
    api_key: String,
    cfg: Config,
    state: Arc<SharedState>,
    cmd_tx: Mutex<Option<mpsc::Sender<Cmd>>>,
}

struct SharedState {
    recording: Mutex<bool>,
    session_finals: Mutex<Vec<String>>,
    /// One-shot signal published by `handle_text` when the reply to this
    /// press's Finalize (`from_finalize: true`) arrives. Per-session:
    /// replaced at the start of every end_session() call so a late reply
    /// from a previous press can never wake the next press's await early.
    final_signal: Mutex<Option<oneshot::Sender<()>>>,
    shutting_down: std::sync::atomic::AtomicBool,
    /// True once the current attempt's `connect_async` has succeeded.
    /// The reconnect loop reads this to decide whether to apply
    /// exponential backoff: a session that was healthy and then dropped
    /// (network blip, server restart, idle timeout) reconnects at
    /// `RECONNECT_INITIAL` instead of doubling forever.
    had_successful_connect: std::sync::atomic::AtomicBool,
}

enum Cmd {
    Audio(Vec<u8>),
    Finalize,
    Stop,
}

impl Deepgram {
    pub fn new(api_key: impl Into<String>, cfg: Config) -> Self {
        Self {
            api_key: api_key.into(),
            cfg,
            state: Arc::new(SharedState {
                recording: Mutex::new(false),
                session_finals: Mutex::new(Vec::new()),
                final_signal: Mutex::new(None),
                shutting_down: std::sync::atomic::AtomicBool::new(false),
                had_successful_connect: std::sync::atomic::AtomicBool::new(false),
            }),
            cmd_tx: Mutex::new(None),
        }
    }

    fn build_url(&self) -> Result<Url, SttError> {
        let mut u =
            Url::parse(&self.cfg.endpoint).map_err(|e| SttError::Internal(e.to_string()))?;
        {
            let mut q = u.query_pairs_mut();
            q.append_pair("model", &self.cfg.model);
            q.append_pair("language", &self.cfg.language);
            q.append_pair("encoding", "linear16");
            q.append_pair("sample_rate", &STT_SAMPLE_RATE.to_string());
            q.append_pair("channels", "1");
            q.append_pair("interim_results", "false");
            q.append_pair("smart_format", "true");
            q.append_pair("punctuate", "true");
            // Measured 2026-10-02: a 1.6 s pause splits the sentence at any
            // endpointing value (25, 100, 300, false), and 100 or 300 with
            // keyterms make Nova-3 Title-Case a whole segment ("To Open A
            // Pull Request"), so 25 stays.
            q.append_pair("endpointing", "25");
            q.append_pair("no_delay", "true");
            for term in &self.cfg.keyterms {
                q.append_pair("keyterm", term);
            }
        }
        Ok(u)
    }
}

#[async_trait]
impl Stt for Deepgram {
    fn name(&self) -> &'static str {
        "deepgram"
    }

    async fn start(&self, events: mpsc::Sender<BackendEvent>) -> Result<(), SttError> {
        if self.api_key.is_empty() {
            return Err(SttError::MissingKey("DEEPGRAM_API_KEY"));
        }
        let url = self.build_url()?;
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>(256);
        *self.cmd_tx.lock() = Some(cmd_tx);

        let api_key = self.api_key.clone();
        let state = self.state.clone();
        state
            .shutting_down
            .store(false, std::sync::atomic::Ordering::Relaxed);

        tokio::spawn(reconnect_loop(api_key, url, cmd_rx, events, state));
        Ok(())
    }

    async fn begin_session(&self) {
        *self.state.recording.lock() = true;
        self.state.session_finals.lock().clear();
        // Drop any sender from a previous end_session — late finals
        // from the prior press will find None and silently drop.
        *self.state.final_signal.lock() = None;
    }

    async fn send_audio(&self, pcm: &[u8]) {
        if !*self.state.recording.lock() {
            return;
        }
        let Some(tx) = self.cmd_tx.lock().clone() else {
            return;
        };
        // Try-send so a stalled WS doesn't block the audio thread.
        let _ = tx.try_send(Cmd::Audio(pcm.to_vec()));
    }

    async fn end_session(&self, timeout: Duration) -> SessionResult {
        let started = Instant::now();
        // Install a fresh oneshot signal BEFORE flipping recording=false.
        // Otherwise a final that lands between recording=false and the
        // signal install would be lost (no waker).
        let (signal_tx, signal_rx) = oneshot::channel();
        *self.state.final_signal.lock() = Some(signal_tx);
        *self.state.recording.lock() = false;

        if let Some(tx) = self.cmd_tx.lock().clone() {
            let _ = tx.try_send(Cmd::Finalize);
        }

        // Wait for the reply to Finalize: Deepgram sends it after every
        // segment final of this press's audio, so once it lands the
        // transcript is complete. Segment finals that land first (he
        // paused mid-sentence) do not end the wait. `timeout` is only a
        // safety net for a lost reply; then we return what we have.
        if tokio::time::timeout(timeout, signal_rx).await.is_err() {
            warn!(
                "deepgram: no Finalize reply within {timeout:?}; typing the finals received so far"
            );
        }
        // Drop the slot so a late final from THIS session doesn't
        // wake the next press.
        *self.state.final_signal.lock() = None;

        let transcript = self
            .state
            .session_finals
            .lock()
            .join(" ")
            .trim()
            .to_string();
        SessionResult {
            transcript,
            finalize_latency: started.elapsed(),
        }
    }

    async fn stop(&self) {
        self.state
            .shutting_down
            .store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(tx) = self.cmd_tx.lock().clone() {
            let _ = tx.try_send(Cmd::Stop);
        }
    }
}

async fn reconnect_loop(
    api_key: String,
    url: Url,
    mut cmd_rx: mpsc::Receiver<Cmd>,
    events: mpsc::Sender<BackendEvent>,
    state: Arc<SharedState>,
) {
    let mut backoff = RECONNECT_INITIAL;
    let mut have_been_connected_once = false;
    loop {
        if state
            .shutting_down
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        let outcome = run_connection(&api_key, &url, &mut cmd_rx, &state).await;
        // If the previous attempt got far enough to actually open the
        // socket, treat the next reconnect as fresh — backoff is for
        // genuine connect failures, not for an in-flight session that
        // dropped after working.
        let was_healthy = state
            .had_successful_connect
            .swap(false, std::sync::atomic::Ordering::Relaxed);
        match outcome {
            ConnectionEnd::Stop => return,
            ConnectionEnd::Closed => {
                if state
                    .shutting_down
                    .load(std::sync::atomic::Ordering::Relaxed)
                {
                    return;
                }
                let _ = events
                    .send(BackendEvent::SocketLost(
                        "deepgram socket closed; reconnecting".into(),
                    ))
                    .await;
            }
            ConnectionEnd::Error(e) => {
                let _ = events.send(BackendEvent::Error(e)).await;
            }
        }

        if was_healthy {
            backoff = RECONNECT_INITIAL;
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(RECONNECT_CAP);
        if state
            .shutting_down
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }

        // Reconnected indicator on next successful run.
        if have_been_connected_once {
            let _ = events.send(BackendEvent::SocketBack).await;
        }
        have_been_connected_once = true;
    }
}

#[derive(Debug)]
enum ConnectionEnd {
    Stop,
    Closed,
    Error(String),
}

async fn run_connection(
    api_key: &str,
    url: &Url,
    cmd_rx: &mut mpsc::Receiver<Cmd>,
    state: &Arc<SharedState>,
) -> ConnectionEnd {
    let mut req = match url.as_str().into_client_request() {
        Ok(r) => r,
        Err(e) => return ConnectionEnd::Error(format!("bad request: {e}")),
    };
    let auth = match HeaderValue::from_str(&format!("Token {api_key}")) {
        Ok(v) => v,
        Err(e) => return ConnectionEnd::Error(format!("bad auth header: {e}")),
    };
    req.headers_mut().insert("Authorization", auth);

    let stream = match connect_async(req).await {
        Ok((s, _resp)) => s,
        Err(e) => return ConnectionEnd::Error(format!("connect: {e}")),
    };
    info!("Deepgram socket open (model query in URL)");
    state
        .had_successful_connect
        .store(true, std::sync::atomic::Ordering::Relaxed);

    let outcome = pump_messages(stream, cmd_rx, state).await;
    info!("Deepgram socket closed: {outcome:?}");
    outcome
}

async fn pump_messages(
    stream: WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    cmd_rx: &mut mpsc::Receiver<Cmd>,
    state: &Arc<SharedState>,
) -> ConnectionEnd {
    let (mut sink, mut source) = stream.split();
    let mut keepalive_tick = tokio::time::interval(KEEPALIVE_INTERVAL);
    keepalive_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let _ = keepalive_tick.tick().await; // skip the immediate fire

    loop {
        tokio::select! {
            biased;
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(Cmd::Audio(bytes)) => {
                        if let Err(e) = sink.send(Message::Binary(bytes.into())).await {
                            return ConnectionEnd::Error(format!("audio send: {e}"));
                        }
                    }
                    Some(Cmd::Finalize) => {
                        if let Err(e) = sink.send(Message::Text(r#"{"type":"Finalize"}"#.into())).await {
                            warn!("Finalize send failed: {e}");
                        }
                    }
                    Some(Cmd::Stop) => {
                        let _ = sink.send(Message::Text(r#"{"type":"CloseStream"}"#.into())).await;
                        return ConnectionEnd::Stop;
                    }
                    None => return ConnectionEnd::Stop,
                }
            }
            msg = source.next() => {
                match msg {
                    Some(Ok(Message::Text(t))) => handle_text(&t, state),
                    Some(Ok(Message::Binary(_))) => {} // ignored
                    Some(Ok(Message::Ping(p))) => { let _ = sink.send(Message::Pong(p)).await; }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) | Some(Ok(Message::Frame(_))) | None => {
                        return ConnectionEnd::Closed;
                    }
                    Some(Err(e)) => return ConnectionEnd::Error(format!("ws read: {e}")),
                }
            }
            _ = keepalive_tick.tick() => {
                if let Err(e) = sink.send(Message::Text(r#"{"type":"KeepAlive"}"#.into())).await {
                    return ConnectionEnd::Error(format!("keepalive send: {e}"));
                }
            }
        }
    }
}

#[derive(Deserialize)]
struct DgMessage {
    #[serde(rename = "type")]
    msg_type: Option<String>,
    is_final: Option<bool>,
    from_finalize: Option<bool>,
    channel: Option<DgChannel>,
}

#[derive(Deserialize)]
struct DgChannel {
    alternatives: Vec<DgAlternative>,
}

#[derive(Deserialize)]
struct DgAlternative {
    transcript: String,
}

/// One final `Results` frame: its transcript (`None` when empty) and
/// whether it is the reply to our Finalize.
#[derive(Debug, PartialEq)]
struct DgFinal {
    transcript: Option<String>,
    from_finalize: bool,
}

/// Parse one Deepgram text frame. Returns `None` for non-`Results`
/// messages, partials (`is_final=false`) and unparseable JSON. A final
/// with an empty transcript still comes back (as `transcript: None`)
/// because the Finalize reply for a silent press is exactly that. Pure:
/// used both by the live message handler and unit tests.
fn parse_final(text: &str) -> Option<DgFinal> {
    let parsed: DgMessage = serde_json::from_str(text).ok()?;
    if parsed.msg_type.as_deref() != Some("Results") {
        return None;
    }
    if !parsed.is_final.unwrap_or(false) {
        return None;
    }
    let transcript = parsed
        .channel
        .and_then(|c| c.alternatives.into_iter().next())
        .map(|alt| alt.transcript.trim().to_string())
        .filter(|t| !t.is_empty());
    Some(DgFinal {
        transcript,
        from_finalize: parsed.from_finalize.unwrap_or(false),
    })
}

fn handle_text(text: &str, state: &Arc<SharedState>) {
    let Some(fin) = parse_final(text) else {
        // Non-JSON, partial, or otherwise uninteresting: log at trace
        // so debug logs don't get spammed by every interim Result.
        trace!("dg: skipped payload: {text:?}");
        return;
    };

    if let Some(transcript) = fin.transcript {
        debug!(
            "dg final: {transcript:?} (from_finalize={})",
            fin.from_finalize
        );
        state.session_finals.lock().push(transcript);
    }

    if fin.from_finalize && !*state.recording.lock() {
        // The reply to this press's Finalize: every final for its audio
        // is in. Wake the waiter via the per-session oneshot, if it's
        // still installed. Replies after end_session has already
        // returned find None and silently drop.
        if let Some(tx) = state.final_signal.lock().take() {
            let _ = tx.send(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{spawn_mock, text_after, MockIn};

    fn final_json(transcript: &str, from_finalize: bool) -> String {
        format!(
            r#"{{"type":"Results","is_final":true,"from_finalize":{from_finalize},"channel":{{"alternatives":[{{"transcript":"{transcript}"}}]}}}}"#
        )
    }

    /// The v0.7.1 cut-off, as a test. Deepgram (endpointing=25) closes a
    /// segment the moment he pauses, so after release a normal segment
    /// final often lands first and the `from_finalize` final with the
    /// tail of the sentence lands later, here 500 ms after Finalize. The
    /// old end_session returned 30 ms after the FIRST final (or at the
    /// 350 ms timeout), so the tail was never typed.
    #[tokio::test]
    async fn release_waits_for_the_finalize_result_not_the_first_final() {
        let server = spawn_mock(
            |_| vec![],
            |_, msg| match msg {
                MockIn::Text(t) if t.contains("Finalize") => vec![
                    text_after(40, final_json("Can you push the fix", false)),
                    text_after(500, final_json("to GitHub tonight?", true)),
                ],
                _ => vec![],
            },
        )
        .await;
        let dg = Deepgram::new(
            "test-key",
            Config {
                endpoint: server.url.clone(),
                ..Config::default()
            },
        );
        let (tx, _rx) = mpsc::channel(8);
        dg.start(tx).await.unwrap();
        while server.connection_count() == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;

        dg.begin_session().await;
        for _ in 0..10 {
            dg.send_audio(&[0u8; 800]).await;
        }
        let result = dg.end_session(Duration::from_millis(3000)).await;
        assert_eq!(result.transcript, "Can you push the fix to GitHub tonight?");
        assert!(result.finalize_latency >= Duration::from_millis(480));
    }

    fn text_of(payload: &str) -> Option<String> {
        parse_final(payload).and_then(|f| f.transcript)
    }

    fn results(transcript: &str, is_final: bool) -> String {
        format!(
            r#"{{"type":"Results","is_final":{is_final},"channel":{{"alternatives":[{{"transcript":"{transcript}"}}]}}}}"#
        )
    }

    #[test]
    fn final_results_message_yields_transcript() {
        let payload = results("Hello world.", true);
        assert_eq!(text_of(&payload).as_deref(), Some("Hello world."));
    }

    #[test]
    fn partial_results_returns_none() {
        let payload = results("Hello", false);
        assert!(parse_final(&payload).is_none());
    }

    #[test]
    fn non_results_message_returns_none() {
        let metadata = r#"{"type":"Metadata","request_id":"abc"}"#;
        let speech_started = r#"{"type":"SpeechStarted","timestamp":0.5}"#;
        assert!(parse_final(metadata).is_none());
        assert!(parse_final(speech_started).is_none());
    }

    #[test]
    fn empty_or_whitespace_transcript_has_no_text() {
        assert!(text_of(&results("", true)).is_none());
        assert!(text_of(&results("   ", true)).is_none());
        assert!(text_of(&results("\t\n", true)).is_none());
    }

    #[test]
    fn missing_channel_has_no_text() {
        let payload = r#"{"type":"Results","is_final":true}"#;
        assert!(text_of(payload).is_none());
    }

    #[test]
    fn empty_alternatives_has_no_text() {
        let payload = r#"{"type":"Results","is_final":true,"channel":{"alternatives":[]}}"#;
        assert!(text_of(payload).is_none());
    }

    #[test]
    fn finalize_reply_is_flagged_even_when_empty() {
        assert_eq!(
            parse_final(&final_json("", true)),
            Some(DgFinal {
                transcript: None,
                from_finalize: true
            })
        );
        assert_eq!(
            parse_final(&final_json("Tail.", true)),
            Some(DgFinal {
                transcript: Some("Tail.".into()),
                from_finalize: true
            })
        );
        assert!(!parse_final(&results("Seg.", true)).unwrap().from_finalize);
    }

    #[test]
    fn url_uses_keyterm_never_keywords() {
        let dg = Deepgram::new(
            "k",
            Config {
                keyterms: vec!["Kubernetes".into(), "PostgreSQL".into()],
                ..Config::default()
            },
        );
        let url = dg.build_url().unwrap();
        assert_eq!(url.host_str(), Some("api.deepgram.com"));
        let pairs: Vec<(String, String)> = url.query_pairs().into_owned().collect();
        let terms: Vec<&str> = pairs
            .iter()
            .filter(|(k, _)| k == "keyterm")
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(terms, ["Kubernetes", "PostgreSQL"]);
        assert!(pairs.iter().all(|(k, _)| k != "keywords"));
        assert!(pairs.contains(&("model".into(), "nova-3".into())));
        assert!(pairs.contains(&("sample_rate".into(), "16000".into())));
    }

    #[test]
    fn malformed_json_returns_none_quietly() {
        assert!(parse_final("not json").is_none());
        assert!(parse_final("{partial").is_none());
        assert!(parse_final("").is_none());
    }

    #[test]
    fn first_alternative_wins_when_multiple() {
        let payload = r#"{"type":"Results","is_final":true,"channel":{"alternatives":[{"transcript":"first"},{"transcript":"second"}]}}"#;
        assert_eq!(text_of(payload).as_deref(), Some("first"));
    }

    #[test]
    fn transcript_is_trimmed() {
        let payload = results("  spaced out  ", true);
        assert_eq!(text_of(&payload).as_deref(), Some("spaced out"));
    }
}
