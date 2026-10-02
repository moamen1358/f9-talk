//! AssemblyAI Universal-3.6 Pro Realtime (streaming API v3) over
//! `tokio-tungstenite`.
//!
//! Wire protocol (`wss://streaming.assemblyai.com/v3/ws?<params>`):
//! - Auth: `Authorization: <key>` header (no `Bearer`/`Token` prefix).
//! - Send: raw s16le PCM as binary frames, each 50 ms to 1000 ms long.
//!   A shorter frame is an "input duration violation" and the server
//!   closes the socket with code 3007, so 25 ms mic frames are paired up
//!   and the last odd frame of a press is padded with silence.
//! - Send: `{"type":"ForceEndpoint"}` ends the current turn now; the
//!   server answers with the formatted final `Turn` in about 0.1-0.25 s
//!   (measured on this machine). With no speech pending it answers
//!   nothing at all.
//! - Send: `{"type":"Terminate"}` closes the session cleanly.
//! - Receive: `Begin`, `SpeechStarted`, `Turn { turn_order, end_of_turn,
//!   turn_is_formatted, transcript }` (partials have `end_of_turn=false`),
//!   `Termination`.
//!
//! Session lifecycle. AssemblyAI bills streaming by how long the socket is
//! OPEN, idle time included, so unlike the Deepgram backend this one does
//! not hold one socket forever:
//! - `start()` opens a session right away, so the first press is warm.
//! - After a press the session stays open for `warm_secs` (default 60 s)
//!   so a run of dictations reuses it, then it is closed with `Terminate`.
//! - A press that finds no open session reconnects at once. Its audio is
//!   kept from the first frame and sent as soon as the socket opens (the
//!   server accepts audio before its `Begin` message), so the first word
//!   is never lost. Measured: socket open ~0.3 s, `Begin` ~0.9 s.
//! - If the socket drops mid-press, the press's audio is replayed into a
//!   fresh session, so the dictation still completes.
//! - `warm_secs = 0` keeps one session open for as long as the app runs
//!   (reconnecting when the server's 3-hour session limit ends it).
//!
//! Turns: the server would end a turn on its own after ~1.3 s of silence
//! and punctuate each turn separately, so a pause to think mid-sentence
//! split one dictation into fragments. The turn silence is set to the
//! 10 s maximum: one press is one turn, ended by `ForceEndpoint`.
//! English is pinned with `language_codes=["en"]`.
//!
//! On release ([`Stt::end_session`]) the backend sends `ForceEndpoint`
//! and waits for the final text of every turn of the press, never a
//! partial hypothesis. See [`decide`] for the exact rule.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde::Deserialize;
use tokio::sync::{mpsc, Notify};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{http::HeaderValue, Message};
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use tracing::{debug, info, trace, warn};
use url::Url;

use crate::{BackendEvent, SessionResult, Stt, SttError, STT_SAMPLE_RATE};

pub const DEFAULT_ENDPOINT: &str = "wss://streaming.assemblyai.com/v3/ws";
pub const DEFAULT_SPEECH_MODEL: &str = "universal-3-6-pro";

/// 50 ms of 16 kHz mono s16le: the smallest frame the server accepts.
const CHUNK_BYTES: usize = 1600;
/// The largest turn silence the server accepts.
pub const MAX_TURN_SILENCE_MS: u32 = 10_000;
/// The server allows at most 100 key terms per session.
const MAX_KEYTERMS: usize = 100;
/// A final that lands sooner than this after `ForceEndpoint` cannot be
/// the reply to it (measured replies: 110-260 ms); it is a turn the
/// server had already closed on its own.
const FORCED_FINAL_MIN: Duration = Duration::from_millis(80);
/// With no turn open, how long to wait after `ForceEndpoint` for speech
/// the server had not reported yet (about 3x the slowest measured reply).
const QUIET_AFTER_FORCE: Duration = Duration::from_millis(700);
/// After `ForceEndpoint`, how long a `SpeechStarted` with no words at all
/// may hold the press open.
const SPEECH_ONLY_GRACE: Duration = Duration::from_millis(2000);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
/// Only sent in always-open mode, where the socket idles for hours.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);
const RECONNECT_INITIAL: Duration = Duration::from_millis(500);
const RECONNECT_CAP: Duration = Duration::from_secs(30);
const COMMAND_CAPACITY: usize = 1024;

#[derive(Debug, Clone)]
pub struct Config {
    /// Streaming endpoint (overridden by tests with a local mock).
    pub endpoint: String,
    pub speech_model: String,
    /// Language hint (`language_codes`), e.g. `en`.
    pub language: String,
    /// Names and jargon to boost (`keyterms_prompt`), at most 100.
    pub keyterms: Vec<String>,
    /// Seconds the session stays open after a press. 0 = always open.
    pub warm_secs: u64,
    /// Silence (ms) before the server may end a turn on its own
    /// (`min_turn_silence` and `max_turn_silence`). One F9 press is one
    /// utterance that `ForceEndpoint` ends, so this is set to the
    /// maximum: with the server default (about 1.3 s) a thinking pause
    /// mid-sentence closed the turn and each piece was punctuated on its
    /// own ("The tokenizing model. Works correctly, ...").
    pub turn_silence_ms: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            endpoint: DEFAULT_ENDPOINT.into(),
            speech_model: DEFAULT_SPEECH_MODEL.into(),
            language: "en".into(),
            keyterms: vec![],
            warm_secs: 60,
            turn_silence_ms: MAX_TURN_SILENCE_MS,
        }
    }
}

impl Config {
    fn warm(&self) -> Option<Duration> {
        (self.warm_secs > 0).then(|| Duration::from_secs(self.warm_secs))
    }
}

pub fn build_url(cfg: &Config) -> Result<Url, SttError> {
    let mut u = Url::parse(&cfg.endpoint).map_err(|e| SttError::Internal(e.to_string()))?;
    {
        let mut q = u.query_pairs_mut();
        q.append_pair("speech_model", &cfg.speech_model);
        q.append_pair("sample_rate", &STT_SAMPLE_RATE.to_string());
        q.append_pair("encoding", "pcm_s16le");
        q.append_pair("format_turns", "true");
        let silence = cfg
            .turn_silence_ms
            .clamp(50, MAX_TURN_SILENCE_MS)
            .to_string();
        q.append_pair("min_turn_silence", &silence);
        q.append_pair("max_turn_silence", &silence);
        if !cfg.language.is_empty() {
            let codes = serde_json::to_string(&[&cfg.language]).unwrap_or_default();
            q.append_pair("language_codes", &codes);
        }
        let terms: Vec<&str> = cfg
            .keyterms
            .iter()
            .map(|t| t.trim())
            .filter(|t| !t.is_empty())
            .take(MAX_KEYTERMS)
            .collect();
        if !terms.is_empty() {
            let json = serde_json::to_string(&terms).unwrap_or_default();
            q.append_pair("keyterms_prompt", &json);
        }
        if let Some(warm) = cfg.warm() {
            // Server-side safety net for billing: if this process ever
            // fails to close an idle session, the server closes it.
            let secs = (warm.as_secs() + 30).clamp(5, 3600);
            q.append_pair("inactivity_timeout", &secs.to_string());
        }
    }
    Ok(u)
}

// ── Per-press transcript state ─────────────────────────────────────

/// What the server has said about the current press. Shared between the
/// connection task (writer) and `end_session` (reader).
#[derive(Debug, Default)]
struct PressState {
    /// Formatted final text per turn of this press.
    finals: BTreeMap<u32, String>,
    /// Latest partial hypothesis of a turn that has no final yet.
    partial: Option<(u32, String)>,
    /// The server heard speech that has no final yet.
    speech_open: bool,
    /// Turns below this belong to an earlier press on the same socket.
    min_turn: u32,
    /// Highest turn order seen on the current socket.
    max_turn_seen: Option<u32>,
    /// When `ForceEndpoint` for this press went out.
    force_sent_at: Option<Instant>,
    /// A final landed after `ForceEndpoint`, late enough to be its reply.
    final_after_force: bool,
}

impl PressState {
    /// Called on F9 press: forget the previous press, but remember the
    /// turn numbering of the socket so its stragglers are ignored.
    fn begin(&mut self) {
        let min_turn = self.max_turn_seen.map_or(0, |t| t + 1);
        let max_turn_seen = self.max_turn_seen;
        *self = PressState {
            min_turn,
            max_turn_seen,
            ..PressState::default()
        };
    }

    /// Called when this press's audio is replayed into a fresh socket:
    /// that socket numbers turns from 0 and will resend every final.
    fn reset_for_new_socket(&mut self) {
        *self = PressState::default();
    }

    /// Apply one server message. Returns true when a waiter should
    /// re-check [`decide`].
    fn apply(&mut self, msg: &ServerMsg, now: Instant) -> bool {
        match msg {
            ServerMsg::SpeechStarted => {
                self.speech_open = true;
                true
            }
            ServerMsg::Turn(turn) => {
                self.max_turn_seen =
                    Some(self.max_turn_seen.map_or(turn.order, |m| m.max(turn.order)));
                if turn.order < self.min_turn {
                    trace!("aai: ignoring turn {} from an earlier press", turn.order);
                    return false;
                }
                let text = turn.transcript.trim();
                // With `format_turns`, an unformatted end-of-turn is followed
                // by the formatted one; keep the turn open until it lands.
                if turn.end_of_turn && turn.formatted {
                    if !text.is_empty() {
                        self.finals.insert(turn.order, text.to_string());
                    }
                    if self.partial.as_ref().is_some_and(|(o, _)| *o <= turn.order) {
                        self.partial = None;
                    }
                    self.speech_open = false;
                    if let Some(fe) = self.force_sent_at {
                        if now >= fe + FORCED_FINAL_MIN {
                            self.final_after_force = true;
                        }
                    }
                } else if !text.is_empty() {
                    self.partial = Some((turn.order, text.to_string()));
                }
                true
            }
            _ => false,
        }
    }

    fn turn_open(&self) -> bool {
        self.speech_open || self.partial.is_some()
    }

    fn final_text(&self) -> String {
        join(self.finals.values().map(String::as_str))
    }

    /// Finals plus the open turn's latest hypothesis: what gets typed if
    /// the final never comes.
    fn best_text(&self) -> String {
        let partial = self.partial.as_ref().map(|(_, t)| t.as_str());
        join(self.finals.values().map(String::as_str).chain(partial))
    }
}

fn join<'a>(parts: impl Iterator<Item = &'a str>) -> String {
    parts
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Debug, PartialEq)]
enum Decision {
    /// Every turn of the press has its final text.
    Done,
    /// Re-check at this instant, or sooner if a message lands.
    WaitUntil(Instant),
    /// The hard deadline passed with a turn still open.
    TimedOut,
}

/// The release rule, pure so it can be tested without a network:
/// - Until `ForceEndpoint` has gone out (the socket may still be
///   connecting) keep waiting.
/// - While a turn is open (speech heard, or a partial without a final)
///   wait for its final, up to the hard deadline.
/// - With no turn open, finish as soon as the reply to `ForceEndpoint`
///   lands; if none comes, the press ended in silence, so finish
///   `QUIET_AFTER_FORCE` after sending it.
/// - Speech heard but no word back at all: finish `SPEECH_ONLY_GRACE`
///   after `ForceEndpoint` (there is no text to lose).
fn decide(st: &PressState, now: Instant, deadline: Instant) -> Decision {
    if let Some(fe) = st.force_sent_at {
        if !st.turn_open() {
            let quiet_end = fe + QUIET_AFTER_FORCE;
            if st.final_after_force || now >= quiet_end || now >= deadline {
                return Decision::Done;
            }
            return Decision::WaitUntil(quiet_end.min(deadline));
        }
        // Speech was detected but no word ever came back, not even a
        // partial: the server answers a real turn within ~0.25 s, so
        // past this grace it was a noise, and nothing is lost by ending.
        let grace_end = fe + SPEECH_ONLY_GRACE;
        if st.partial.is_none() && st.speech_open {
            if now >= grace_end || now >= deadline {
                return Decision::Done;
            }
            return Decision::WaitUntil(grace_end.min(deadline));
        }
    }
    if now >= deadline {
        Decision::TimedOut
    } else {
        Decision::WaitUntil(deadline)
    }
}

// ── Server messages ────────────────────────────────────────────────

#[derive(Debug, PartialEq)]
struct Turn {
    order: u32,
    end_of_turn: bool,
    formatted: bool,
    transcript: String,
}

#[derive(Debug, PartialEq)]
enum ServerMsg {
    Begin { id: String, model: Option<String> },
    SpeechStarted,
    Turn(Turn),
    Termination,
    Error(String),
    Other,
}

#[derive(Deserialize)]
struct RawMsg {
    #[serde(rename = "type")]
    msg_type: Option<String>,
    id: Option<String>,
    configuration: Option<RawConfig>,
    turn_order: Option<u32>,
    end_of_turn: Option<bool>,
    turn_is_formatted: Option<bool>,
    transcript: Option<String>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct RawConfig {
    model: Option<String>,
}

fn parse_message(text: &str) -> ServerMsg {
    let Ok(raw) = serde_json::from_str::<RawMsg>(text) else {
        return ServerMsg::Other;
    };
    if let Some(err) = raw.error {
        return ServerMsg::Error(err);
    }
    match raw.msg_type.as_deref() {
        Some("Begin") => ServerMsg::Begin {
            id: raw.id.unwrap_or_default(),
            model: raw.configuration.and_then(|c| c.model),
        },
        Some("SpeechStarted") => ServerMsg::SpeechStarted,
        Some("Turn") => match raw.turn_order {
            Some(order) => ServerMsg::Turn(Turn {
                order,
                end_of_turn: raw.end_of_turn.unwrap_or(false),
                formatted: raw.turn_is_formatted.unwrap_or(false),
                transcript: raw.transcript.unwrap_or_default(),
            }),
            None => ServerMsg::Other,
        },
        Some("Termination") => ServerMsg::Termination,
        _ => ServerMsg::Other,
    }
}

// ── Backend ────────────────────────────────────────────────────────

struct Shared {
    press: Mutex<PressState>,
    changed: Notify,
}

enum Cmd {
    /// F9 pressed: start a new press (connects if needed).
    Begin,
    Audio(Vec<u8>),
    /// F9 released: flush audio and send `ForceEndpoint`.
    Finalize,
    /// `end_session` returned: the press is over.
    End,
    Stop,
}

pub struct AssemblyAi {
    api_key: String,
    cfg: Config,
    shared: Arc<Shared>,
    cmd_tx: Mutex<Option<mpsc::Sender<Cmd>>>,
    recording: Mutex<bool>,
}

impl AssemblyAi {
    pub fn new(api_key: impl Into<String>, cfg: Config) -> Self {
        Self {
            api_key: api_key.into(),
            cfg,
            shared: Arc::new(Shared {
                press: Mutex::new(PressState::default()),
                changed: Notify::new(),
            }),
            cmd_tx: Mutex::new(None),
            recording: Mutex::new(false),
        }
    }

    fn send(&self, cmd: Cmd) {
        if let Some(tx) = self.cmd_tx.lock().clone() {
            // Try-send so a stalled socket never blocks the audio path.
            if tx.try_send(cmd).is_err() {
                warn!("assemblyai: command queue full; dropped a command");
            }
        }
    }
}

#[async_trait]
impl Stt for AssemblyAi {
    fn name(&self) -> &'static str {
        "assemblyai"
    }

    async fn start(&self, events: mpsc::Sender<BackendEvent>) -> Result<(), SttError> {
        if self.api_key.is_empty() {
            return Err(SttError::MissingKey("ASSEMBLYAI_API_KEY"));
        }
        let url = build_url(&self.cfg)?;
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>(COMMAND_CAPACITY);
        *self.cmd_tx.lock() = Some(cmd_tx);
        let conn = Connection {
            api_key: self.api_key.clone(),
            url,
            model: self.cfg.speech_model.clone(),
            warm: self.cfg.warm(),
            shared: self.shared.clone(),
            events,
            press: PressAudio::default(),
        };
        tokio::spawn(conn.run(cmd_rx));
        Ok(())
    }

    async fn begin_session(&self) {
        self.shared.press.lock().begin();
        *self.recording.lock() = true;
        self.send(Cmd::Begin);
    }

    async fn send_audio(&self, pcm: &[u8]) {
        if !*self.recording.lock() {
            return;
        }
        self.send(Cmd::Audio(pcm.to_vec()));
    }

    async fn end_session(&self, timeout: Duration) -> SessionResult {
        let started = Instant::now();
        let deadline = started + timeout;
        *self.recording.lock() = false;
        self.send(Cmd::Finalize);

        let transcript = loop {
            // Register interest before reading the state so a message
            // that lands in between still wakes us.
            let changed = self.shared.changed.notified();
            let decision = decide(&self.shared.press.lock(), Instant::now(), deadline);
            match decision {
                Decision::Done => break self.shared.press.lock().final_text(),
                Decision::TimedOut => {
                    let st = self.shared.press.lock();
                    let best = st.best_text();
                    warn!(
                        "assemblyai: final text did not arrive within {timeout:?} \
                         (ForceEndpoint sent: {}); typing the best text so far: {best:?}",
                        st.force_sent_at.is_some()
                    );
                    break best;
                }
                Decision::WaitUntil(at) => {
                    let _ = tokio::time::timeout_at(at.into(), changed).await;
                }
            }
        };
        self.send(Cmd::End);
        SessionResult {
            transcript,
            finalize_latency: started.elapsed(),
        }
    }

    async fn stop(&self) {
        *self.recording.lock() = false;
        self.send(Cmd::Stop);
    }
}

// ── Connection task ────────────────────────────────────────────────

/// The current press's audio, kept whole so it can be (re)sent to a
/// socket that opens after the press started or replaces one that died.
#[derive(Debug, Default)]
struct PressAudio {
    active: bool,
    finalize_requested: bool,
    bytes: Vec<u8>,
    /// Bytes of `bytes` already sent on the current socket.
    sent: usize,
}

impl PressAudio {
    /// Whole 50 ms chunks not yet sent; with `flush`, also the tail,
    /// padded with silence to 50 ms.
    fn take_chunks(&mut self, flush: bool) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        while self.bytes.len() - self.sent >= CHUNK_BYTES {
            out.push(self.bytes[self.sent..self.sent + CHUNK_BYTES].to_vec());
            self.sent += CHUNK_BYTES;
        }
        if flush && self.sent < self.bytes.len() {
            let mut tail = self.bytes[self.sent..].to_vec();
            tail.resize(CHUNK_BYTES, 0);
            self.sent = self.bytes.len();
            out.push(tail);
        }
        out
    }
}

type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

#[derive(Debug)]
enum ConnectionEnd {
    Stop,
    /// Closed on purpose after `warm` of idle time.
    Idle,
    Closed(String),
    Error(String),
}

struct Connection {
    api_key: String,
    url: Url,
    model: String,
    warm: Option<Duration>,
    shared: Arc<Shared>,
    events: mpsc::Sender<BackendEvent>,
    press: PressAudio,
}

impl Connection {
    async fn run(mut self, mut cmd_rx: mpsc::Receiver<Cmd>) {
        // Open a session at start-up so the first press is warm.
        let mut want_socket = true;
        let mut backoff = RECONNECT_INITIAL;
        let mut lost = false;
        loop {
            if !want_socket {
                // No socket: sleep until a press needs one.
                match cmd_rx.recv().await {
                    None | Some(Cmd::Stop) => return,
                    Some(cmd) => {
                        want_socket = matches!(cmd, Cmd::Begin | Cmd::Audio(_) | Cmd::Finalize);
                        self.apply_offline(cmd);
                        continue;
                    }
                }
            }

            let ws = match self.connect().await {
                Ok(ws) => ws,
                Err(e) => {
                    warn!("assemblyai: connect failed: {e}");
                    let _ = self
                        .events
                        .try_send(BackendEvent::Error(format!("assemblyai connect: {e}")));
                    if !self.press.active && self.warm.is_some() {
                        // Nobody is waiting; connect again on the next press.
                        want_socket = false;
                        continue;
                    }
                    // A press is waiting (its audio is kept), or the
                    // socket is meant to be always open: retry. Keep
                    // tracking commands meanwhile, so a press that ends
                    // (or a stop) is noticed before the next attempt.
                    let retry_at = tokio::time::Instant::now() + backoff;
                    backoff = (backoff * 2).min(RECONNECT_CAP);
                    loop {
                        match tokio::time::timeout_at(retry_at, cmd_rx.recv()).await {
                            Err(_) => break,
                            Ok(None) | Ok(Some(Cmd::Stop)) => return,
                            Ok(Some(cmd)) => self.apply_offline(cmd),
                        }
                    }
                    want_socket = self.press.active || self.warm.is_none();
                    continue;
                }
            };
            backoff = RECONNECT_INITIAL;
            if lost {
                let _ = self.events.try_send(BackendEvent::SocketBack);
                lost = false;
            }

            let end = self.pump(ws, &mut cmd_rx).await;
            match end {
                ConnectionEnd::Stop => return,
                ConnectionEnd::Idle => {
                    info!(
                        "assemblyai: closed the idle session (billing stops until the next press)"
                    );
                    want_socket = false;
                }
                ConnectionEnd::Closed(why) | ConnectionEnd::Error(why) => {
                    warn!("assemblyai: session ended: {why}");
                    // Reconnect now if a press is in flight (its audio is
                    // replayed) or the socket is meant to stay open.
                    want_socket = self.press.active || self.warm.is_none();
                    if want_socket {
                        lost = true;
                        let _ = self.events.try_send(BackendEvent::SocketLost(format!(
                            "assemblyai session ended ({why}); reconnecting"
                        )));
                    }
                }
            }
        }
    }

    /// Track a command that arrived while no socket is open.
    fn apply_offline(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Begin => {
                self.press = PressAudio {
                    active: true,
                    ..PressAudio::default()
                };
            }
            Cmd::Audio(bytes) => {
                if self.press.active {
                    self.press.bytes.extend_from_slice(&bytes);
                }
            }
            Cmd::Finalize => self.press.finalize_requested = true,
            Cmd::End => self.press = PressAudio::default(),
            Cmd::Stop => {}
        }
    }

    async fn connect(&self) -> Result<Ws, String> {
        let mut req = self
            .url
            .as_str()
            .into_client_request()
            .map_err(|e| format!("bad request: {e}"))?;
        let auth =
            HeaderValue::from_str(&self.api_key).map_err(|e| format!("bad auth header: {e}"))?;
        req.headers_mut().insert("Authorization", auth);
        let t0 = Instant::now();
        match tokio::time::timeout(CONNECT_TIMEOUT, connect_async(req)).await {
            Ok(Ok((ws, _resp))) => {
                info!(
                    "assemblyai: socket open in {:.0?} (model {})",
                    t0.elapsed(),
                    self.model
                );
                Ok(ws)
            }
            Ok(Err(e)) => Err(e.to_string()),
            Err(_) => Err(format!("timed out after {CONNECT_TIMEOUT:?}")),
        }
    }

    async fn pump(&mut self, ws: Ws, cmd_rx: &mut mpsc::Receiver<Cmd>) -> ConnectionEnd {
        let (mut sink, mut source) = ws.split();
        let opened = Instant::now();

        // A press in flight: its earlier audio went to no socket or to
        // one that died, so send it all (again) and re-number its turns.
        self.press.sent = 0;
        if self.press.active {
            self.shared.press.lock().reset_for_new_socket();
            let flush = self.press.finalize_requested;
            let chunks = self.press.take_chunks(flush);
            if !chunks.is_empty() {
                debug!("assemblyai: sending {} buffered chunks", chunks.len());
            }
            for c in chunks {
                if let Err(e) = sink.send(Message::Binary(c.into())).await {
                    return ConnectionEnd::Error(format!("audio send: {e}"));
                }
            }
            if flush {
                if let Err(e) = send_force_endpoint(&mut sink, &self.shared).await {
                    return ConnectionEnd::Error(e);
                }
            }
        } else {
            self.shared.press.lock().reset_for_new_socket();
        }

        let mut idle_since = (!self.press.active).then_some(opened);
        let mut keepalive = tokio::time::interval(KEEPALIVE_INTERVAL);
        keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let _ = keepalive.tick().await;

        loop {
            let idle_deadline = match (self.warm, idle_since) {
                (Some(warm), Some(since)) => Some(since + warm),
                _ => None,
            };
            tokio::select! {
                biased;
                cmd = cmd_rx.recv() => match cmd {
                    Some(Cmd::Begin) => {
                        self.press = PressAudio { active: true, ..PressAudio::default() };
                        idle_since = None;
                    }
                    Some(Cmd::Audio(bytes)) => {
                        if !self.press.active {
                            continue;
                        }
                        self.press.bytes.extend_from_slice(&bytes);
                        for c in self.press.take_chunks(false) {
                            if let Err(e) = sink.send(Message::Binary(c.into())).await {
                                return ConnectionEnd::Error(format!("audio send: {e}"));
                            }
                        }
                    }
                    Some(Cmd::Finalize) => {
                        self.press.finalize_requested = true;
                        for c in self.press.take_chunks(true) {
                            if let Err(e) = sink.send(Message::Binary(c.into())).await {
                                return ConnectionEnd::Error(format!("audio send: {e}"));
                            }
                        }
                        if let Err(e) = send_force_endpoint(&mut sink, &self.shared).await {
                            return ConnectionEnd::Error(e);
                        }
                    }
                    Some(Cmd::End) => {
                        self.press = PressAudio::default();
                        idle_since = Some(Instant::now());
                    }
                    Some(Cmd::Stop) | None => {
                        terminate(&mut sink, &mut source).await;
                        return ConnectionEnd::Stop;
                    }
                },
                msg = source.next() => match msg {
                    Some(Ok(Message::Text(t))) => {
                        if let Some(end) = self.handle_text(&t) {
                            return end;
                        }
                    }
                    Some(Ok(Message::Close(frame))) => {
                        let why = frame
                            .map(|f| format!("closed by server: {} {}", u16::from(f.code), f.reason))
                            .unwrap_or_else(|| "closed by server".into());
                        return ConnectionEnd::Closed(why);
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return ConnectionEnd::Error(format!("ws read: {e}")),
                    None => return ConnectionEnd::Closed("socket closed".into()),
                },
                _ = keepalive.tick(), if self.warm.is_none() => {
                    if let Err(e) = sink.send(Message::Text(r#"{"type":"KeepAlive"}"#.into())).await {
                        return ConnectionEnd::Error(format!("keepalive send: {e}"));
                    }
                }
                _ = sleep_until_opt(idle_deadline) => {
                    terminate(&mut sink, &mut source).await;
                    return ConnectionEnd::Idle;
                }
            }
        }
    }

    /// Returns `Some` when the message ends the session.
    fn handle_text(&self, text: &str) -> Option<ConnectionEnd> {
        let msg = parse_message(text);
        match &msg {
            ServerMsg::Begin { id, model } => {
                info!(
                    "assemblyai: session {id} began (model {})",
                    model.as_deref().unwrap_or("?")
                );
                if model.as_deref().is_some_and(|m| m != self.model) {
                    warn!(
                        "assemblyai: asked for model {} but the server runs {:?}",
                        self.model, model
                    );
                }
            }
            ServerMsg::Turn(t) => {
                if t.end_of_turn && t.formatted {
                    debug!("aai final #{}: {:?}", t.order, t.transcript);
                } else {
                    trace!("aai partial #{}: {:?}", t.order, t.transcript);
                }
            }
            ServerMsg::Termination => {
                return Some(ConnectionEnd::Closed("server sent Termination".into()))
            }
            ServerMsg::Error(e) => {
                warn!("assemblyai: server error: {e}");
                let _ = self
                    .events
                    .try_send(BackendEvent::Error(format!("assemblyai: {e}")));
            }
            ServerMsg::SpeechStarted | ServerMsg::Other => trace!("aai: {text}"),
        }
        let wake = self.shared.press.lock().apply(&msg, Instant::now());
        if wake {
            self.shared.changed.notify_one();
        }
        None
    }
}

async fn send_force_endpoint(
    sink: &mut futures_util::stream::SplitSink<Ws, Message>,
    shared: &Shared,
) -> Result<(), String> {
    sink.send(Message::Text(r#"{"type":"ForceEndpoint"}"#.into()))
        .await
        .map_err(|e| format!("ForceEndpoint send: {e}"))?;
    shared.press.lock().force_sent_at = Some(Instant::now());
    shared.changed.notify_one();
    Ok(())
}

/// Ask the server to end the session and wait briefly for it to do so,
/// so billing stops now rather than when the TCP connection times out.
async fn terminate(
    sink: &mut futures_util::stream::SplitSink<Ws, Message>,
    source: &mut futures_util::stream::SplitStream<Ws>,
) {
    if sink
        .send(Message::Text(r#"{"type":"Terminate"}"#.into()))
        .await
        .is_err()
    {
        return;
    }
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(Ok(msg)) = source.next().await {
            if let Message::Text(t) = &msg {
                if parse_message(t) == ServerMsg::Termination {
                    break;
                }
            }
            if matches!(msg, Message::Close(_)) {
                break;
            }
        }
    })
    .await;
    let _ = sink.close().await;
}

async fn sleep_until_opt(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at.into()).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        close_after, spawn_mock, text_after, Ctx, MockIn, MockServer, Reply,
    };

    const BEGIN: &str = r#"{"type":"Begin","id":"s-1","expires_at":1,"configuration":{"model":"universal-3-6-pro"}}"#;

    fn turn(order: u32, end_of_turn: bool, transcript: &str) -> String {
        format!(
            r#"{{"type":"Turn","turn_order":{order},"turn_is_formatted":true,"end_of_turn":{end_of_turn},"transcript":"{transcript}","words":[]}}"#
        )
    }

    fn query(url: &Url) -> Vec<(String, String)> {
        url.query_pairs().into_owned().collect()
    }

    fn get<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
        pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    // ── URL and parameters ─────────────────────────────────────────

    #[test]
    fn url_carries_model_audio_format_language_and_keyterms() {
        let cfg = Config {
            keyterms: vec!["Kubernetes".into(), " PostgreSQL ".into(), "".into()],
            ..Config::default()
        };
        let url = build_url(&cfg).unwrap();
        assert_eq!(url.scheme(), "wss");
        assert_eq!(url.host_str(), Some("streaming.assemblyai.com"));
        assert_eq!(url.path(), "/v3/ws");
        let q = query(&url);
        assert_eq!(get(&q, "speech_model"), Some("universal-3-6-pro"));
        assert_eq!(get(&q, "sample_rate"), Some("16000"));
        assert_eq!(get(&q, "encoding"), Some("pcm_s16le"));
        assert_eq!(get(&q, "format_turns"), Some("true"));
        assert_eq!(get(&q, "language_codes"), Some(r#"["en"]"#));
        // A pause inside one press never ends the turn.
        assert_eq!(get(&q, "min_turn_silence"), Some("10000"));
        assert_eq!(get(&q, "max_turn_silence"), Some("10000"));
        // A JSON array, trimmed, blanks dropped.
        assert_eq!(
            get(&q, "keyterms_prompt"),
            Some(r#"["Kubernetes","PostgreSQL"]"#)
        );
        // Billing safety net: 60 s warm window + 30 s.
        assert_eq!(get(&q, "inactivity_timeout"), Some("90"));
    }

    #[test]
    fn url_omits_empty_keyterms_and_caps_at_100() {
        let q = query(&build_url(&Config::default()).unwrap());
        assert_eq!(get(&q, "keyterms_prompt"), None);

        let cfg = Config {
            keyterms: (0..150).map(|i| format!("term{i}")).collect(),
            ..Config::default()
        };
        let q = query(&build_url(&cfg).unwrap());
        let terms: Vec<String> = serde_json::from_str(get(&q, "keyterms_prompt").unwrap()).unwrap();
        assert_eq!(terms.len(), 100);
        assert_eq!(terms[99], "term99");
    }

    #[test]
    fn always_open_mode_sets_no_inactivity_timeout() {
        let cfg = Config {
            warm_secs: 0,
            ..Config::default()
        };
        let q = query(&build_url(&cfg).unwrap());
        assert_eq!(get(&q, "inactivity_timeout"), None);
    }

    // ── Message parsing ────────────────────────────────────────────

    #[test]
    fn parses_begin_turns_and_termination() {
        assert_eq!(
            parse_message(BEGIN),
            ServerMsg::Begin {
                id: "s-1".into(),
                model: Some("universal-3-6-pro".into())
            }
        );
        assert_eq!(
            parse_message(&turn(3, true, "Hello, Kubernetes.")),
            ServerMsg::Turn(Turn {
                order: 3,
                end_of_turn: true,
                formatted: true,
                transcript: "Hello, Kubernetes.".into()
            })
        );
        assert_eq!(
            parse_message(r#"{"type":"SpeechStarted","timestamp":1200,"confidence":0.9}"#),
            ServerMsg::SpeechStarted
        );
        assert_eq!(
            parse_message(
                r#"{"type":"Termination","audio_duration_seconds":1,"session_duration_seconds":2}"#
            ),
            ServerMsg::Termination
        );
    }

    #[test]
    fn parses_errors_and_ignores_junk() {
        assert_eq!(
            parse_message(r#"{"error":"Invalid API key"}"#),
            ServerMsg::Error("Invalid API key".into())
        );
        assert_eq!(parse_message("not json"), ServerMsg::Other);
        assert_eq!(parse_message(r#"{"type":"Turn"}"#), ServerMsg::Other);
        assert_eq!(parse_message(r#"{"type":"Heartbeat"}"#), ServerMsg::Other);
    }

    // ── Press state and the release rule ───────────────────────────

    fn apply(st: &mut PressState, json: &str, at: Instant) {
        st.apply(&parse_message(json), at);
    }

    #[test]
    fn finals_join_in_turn_order_and_partials_are_not_final() {
        let t = Instant::now();
        let mut st = PressState::default();
        apply(&mut st, r#"{"type":"SpeechStarted"}"#, t);
        apply(&mut st, &turn(0, false, "Open the"), t);
        assert!(st.turn_open());
        assert_eq!(st.final_text(), "");
        assert_eq!(st.best_text(), "Open the");
        apply(&mut st, &turn(0, true, "Open the Grafana dashboard."), t);
        apply(&mut st, &turn(1, false, "Then send"), t);
        assert_eq!(st.final_text(), "Open the Grafana dashboard.");
        apply(&mut st, &turn(1, true, "Then send it to Priya."), t);
        assert!(!st.turn_open());
        assert_eq!(
            st.final_text(),
            "Open the Grafana dashboard. Then send it to Priya."
        );
    }

    #[test]
    fn unformatted_end_of_turn_keeps_the_turn_open() {
        let t = Instant::now();
        let mut st = PressState::default();
        apply(
            &mut st,
            r#"{"type":"Turn","turn_order":0,"turn_is_formatted":false,"end_of_turn":true,"transcript":"hello world"}"#,
            t,
        );
        assert!(st.turn_open());
        assert_eq!(st.final_text(), "");
        apply(&mut st, &turn(0, true, "Hello world."), t);
        assert!(!st.turn_open());
        assert_eq!(st.final_text(), "Hello world.");
    }

    #[test]
    fn turns_of_an_earlier_press_are_ignored() {
        let t = Instant::now();
        let mut st = PressState::default();
        apply(&mut st, &turn(0, true, "First press."), t);
        apply(&mut st, &turn(1, false, "timed out partial"), t);
        st.begin();
        assert_eq!(st.min_turn, 2);
        // The late final of turn 1 belongs to the previous press.
        apply(&mut st, &turn(1, true, "Timed out partial."), t);
        apply(&mut st, &turn(2, true, "Second press."), t);
        assert_eq!(st.final_text(), "Second press.");
    }

    #[test]
    fn release_waits_while_a_turn_is_open_and_ends_on_its_final() {
        let t0 = Instant::now();
        let deadline = t0 + Duration::from_secs(4);
        let mut st = PressState::default();
        apply(&mut st, &turn(0, false, "Can you push"), t0);
        // ForceEndpoint not sent yet (still connecting): wait.
        assert_eq!(decide(&st, t0, deadline), Decision::WaitUntil(deadline));
        st.force_sent_at = Some(t0);
        // A turn is open: wait for its final, however long it takes,
        // up to the deadline. This is the case the old fixed 350 ms wait cut.
        let t500 = t0 + Duration::from_millis(500);
        assert_eq!(decide(&st, t500, deadline), Decision::WaitUntil(deadline));
        apply(&mut st, &turn(0, true, "Can you push the fix?"), t500);
        assert_eq!(decide(&st, t500, deadline), Decision::Done);
        assert_eq!(st.final_text(), "Can you push the fix?");
    }

    #[test]
    fn short_press_ends_on_the_forced_final_even_without_a_partial() {
        let t0 = Instant::now();
        let deadline = t0 + Duration::from_secs(4);
        let mut st = PressState {
            force_sent_at: Some(t0),
            ..PressState::default()
        };
        // Nothing heard yet: wait for the quiet window.
        assert_eq!(
            decide(&st, t0, deadline),
            Decision::WaitUntil(t0 + QUIET_AFTER_FORCE)
        );
        // The server's reply to ForceEndpoint: SpeechStarted + final.
        let t190 = t0 + Duration::from_millis(190);
        apply(&mut st, r#"{"type":"SpeechStarted"}"#, t190);
        apply(&mut st, &turn(0, true, "Yes, ship it."), t190);
        assert_eq!(decide(&st, t190, deadline), Decision::Done);
        assert_eq!(st.final_text(), "Yes, ship it.");
    }

    #[test]
    fn a_turn_closed_just_before_the_force_does_not_end_the_wait_early() {
        let t0 = Instant::now();
        let deadline = t0 + Duration::from_secs(4);
        let mut st = PressState {
            force_sent_at: Some(t0),
            ..PressState::default()
        };
        // A final already in flight when ForceEndpoint went out.
        apply(
            &mut st,
            &turn(0, true, "Open the dashboard."),
            t0 + Duration::from_millis(20),
        );
        assert!(!st.final_after_force);
        assert_eq!(
            decide(&st, t0 + Duration::from_millis(20), deadline),
            Decision::WaitUntil(t0 + QUIET_AFTER_FORCE)
        );
        // The forced reply for trailing speech then lands.
        let t200 = t0 + Duration::from_millis(200);
        apply(&mut st, r#"{"type":"SpeechStarted"}"#, t200);
        apply(&mut st, &turn(1, true, "Then email Priya."), t200);
        assert_eq!(decide(&st, t200, deadline), Decision::Done);
        assert_eq!(st.final_text(), "Open the dashboard. Then email Priya.");
    }

    #[test]
    fn silent_press_ends_after_the_quiet_window() {
        let t0 = Instant::now();
        let deadline = t0 + Duration::from_secs(4);
        let st = PressState {
            force_sent_at: Some(t0),
            ..PressState::default()
        };
        assert_eq!(
            decide(&st, t0 + QUIET_AFTER_FORCE, deadline),
            Decision::Done
        );
        assert_eq!(st.final_text(), "");
    }

    #[test]
    fn speech_without_words_ends_after_the_grace() {
        let t0 = Instant::now();
        let deadline = t0 + Duration::from_secs(4);
        let mut st = PressState {
            force_sent_at: Some(t0),
            ..PressState::default()
        };
        apply(&mut st, r#"{"type":"SpeechStarted"}"#, t0);
        assert_eq!(
            decide(&st, t0, deadline),
            Decision::WaitUntil(t0 + SPEECH_ONLY_GRACE)
        );
        assert_eq!(
            decide(&st, t0 + SPEECH_ONLY_GRACE, deadline),
            Decision::Done
        );
    }

    #[test]
    fn deadline_with_an_open_turn_times_out_with_the_best_text() {
        let t0 = Instant::now();
        let deadline = t0 + Duration::from_secs(4);
        let mut st = PressState {
            force_sent_at: Some(t0),
            ..PressState::default()
        };
        apply(&mut st, &turn(0, true, "First sentence."), t0);
        apply(&mut st, &turn(1, false, "and the second"), t0);
        assert_eq!(decide(&st, deadline, deadline), Decision::TimedOut);
        assert_eq!(st.best_text(), "First sentence. and the second");
    }

    #[test]
    fn audio_goes_out_in_50ms_chunks_and_the_tail_is_padded() {
        let mut press = PressAudio {
            active: true,
            ..PressAudio::default()
        };
        press.bytes.extend_from_slice(&[1u8; 800]);
        assert!(press.take_chunks(false).is_empty());
        press.bytes.extend_from_slice(&[2u8; 800 * 2]);
        let chunks = press.take_chunks(false);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].len(), CHUNK_BYTES);
        let tail = press.take_chunks(true);
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].len(), CHUNK_BYTES);
        assert!(tail[0][..800].iter().all(|b| *b == 2));
        assert!(tail[0][800..].iter().all(|b| *b == 0));
        assert!(press.take_chunks(true).is_empty());
    }

    // ── Against a scripted local server ────────────────────────────

    /// A server that answers ForceEndpoint like AssemblyAI does: the
    /// formatted final of the open turn after `reply_ms`.
    async fn server(reply_ms: u64, text: &'static str) -> MockServer {
        spawn_mock(
            |_| vec![text_after(5, BEGIN)],
            move |_ctx: &Ctx, msg: &MockIn| -> Vec<Reply> {
                match msg {
                    MockIn::Text(t) if t.contains("ForceEndpoint") => vec![
                        text_after(reply_ms.saturating_sub(5), r#"{"type":"SpeechStarted"}"#),
                        text_after(reply_ms, turn(0, true, text)),
                    ],
                    _ => vec![],
                }
            },
        )
        .await
    }

    fn backend(server: &MockServer, warm_secs: u64) -> AssemblyAi {
        AssemblyAi::new(
            "test-key",
            Config {
                endpoint: server.url.clone(),
                keyterms: vec!["PostgreSQL".into()],
                warm_secs,
                ..Config::default()
            },
        )
    }

    async fn wait_for(cond: impl Fn() -> bool) {
        let give_up = Instant::now() + Duration::from_secs(5);
        while !cond() {
            assert!(Instant::now() < give_up, "condition never became true");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    async fn press(aai: &AssemblyAi, frames: usize, timeout_ms: u64) -> SessionResult {
        aai.begin_session().await;
        for _ in 0..frames {
            aai.send_audio(&[7u8; 800]).await;
        }
        aai.end_session(Duration::from_millis(timeout_ms)).await
    }

    #[tokio::test]
    async fn waits_for_a_slow_final_that_the_old_350ms_timeout_cut() {
        let server = server(500, "Can you push the fix to GitHub tonight?").await;
        let aai = backend(&server, 60);
        let (tx, _rx) = mpsc::channel(8);
        aai.start(tx).await.unwrap();
        wait_for(|| server.connection_count() == 1).await;

        let result = press(&aai, 10, 4000).await;
        assert_eq!(result.transcript, "Can you push the fix to GitHub tonight?");
        assert!(result.finalize_latency >= Duration::from_millis(480));
        assert!(result.finalize_latency < Duration::from_millis(1500));

        // Auth header and parameters reached the server.
        let (uri, auth) = server.requests.lock()[0].clone();
        assert_eq!(auth, "test-key");
        assert!(uri.contains("speech_model=universal-3-6-pro"));
        assert!(uri.contains("keyterms_prompt="));
        // Every audio frame is 50 ms: 10 x 25 ms frames = 5 chunks.
        assert_eq!(server.audio_frames(), vec![CHUNK_BYTES; 5]);
        assert_eq!(
            server.texts(),
            vec![r#"{"type":"ForceEndpoint"}"#.to_string()]
        );
    }

    #[tokio::test]
    async fn first_word_survives_a_press_that_has_to_reconnect() {
        let server = server(150, "Hello PostgreSQL.").await;
        // Warm window of 1 s, so the start-up session closes on its own.
        let aai = backend(&server, 1);
        let (tx, _rx) = mpsc::channel(8);
        aai.start(tx).await.unwrap();
        wait_for(|| server.texts().iter().any(|t| t.contains("Terminate"))).await;

        // The press finds no session: its audio is kept from the first
        // frame and all of it reaches the new session.
        let result = press(&aai, 11, 4000).await;
        assert_eq!(server.connection_count(), 2);
        assert_eq!(result.transcript, "Hello PostgreSQL.");
        let second: usize = server
            .received
            .lock()
            .iter()
            .filter(|(c, m)| *c == 1 && matches!(m, MockIn::Audio(_)))
            .count();
        // 11 frames = 5 whole chunks + 1 padded tail.
        assert_eq!(second, 6);
    }

    #[tokio::test]
    async fn a_dropped_socket_mid_press_is_replayed_into_a_new_session() {
        let server = spawn_mock(
            |_| vec![text_after(5, BEGIN)],
            |ctx: &Ctx, msg: &MockIn| -> Vec<Reply> {
                match (ctx.connection, msg) {
                    // The first session dies after 4 chunks of the press.
                    (0, MockIn::Audio(_)) if ctx.audio_bytes == 4 * CHUNK_BYTES => {
                        vec![close_after(0)]
                    }
                    (1, MockIn::Text(t)) if t.contains("ForceEndpoint") => {
                        vec![text_after(150, turn(0, true, "Every word of the press."))]
                    }
                    _ => vec![],
                }
            },
        )
        .await;
        let aai = backend(&server, 60);
        let (tx, mut rx) = mpsc::channel(8);
        aai.start(tx).await.unwrap();
        wait_for(|| server.connection_count() == 1).await;

        aai.begin_session().await;
        for _ in 0..20 {
            aai.send_audio(&[7u8; 800]).await;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let result = aai.end_session(Duration::from_millis(4000)).await;
        assert_eq!(result.transcript, "Every word of the press.");
        // All 20 frames (10 chunks) went to the second session.
        let replayed = server
            .received
            .lock()
            .iter()
            .filter(|(c, m)| *c == 1 && matches!(m, MockIn::Audio(_)))
            .count();
        assert_eq!(replayed, 10);
        assert!(matches!(rx.try_recv(), Ok(BackendEvent::SocketLost(_))));
    }

    #[tokio::test]
    async fn silent_press_returns_empty_after_the_quiet_window() {
        // This server never answers ForceEndpoint, like the real one
        // when no speech is pending.
        let server = spawn_mock(|_| vec![text_after(5, BEGIN)], |_, _| vec![]).await;
        let aai = backend(&server, 60);
        let (tx, _rx) = mpsc::channel(8);
        aai.start(tx).await.unwrap();
        wait_for(|| server.connection_count() == 1).await;

        let result = press(&aai, 8, 4000).await;
        assert_eq!(result.transcript, "");
        assert!(result.finalize_latency >= QUIET_AFTER_FORCE);
        assert!(result.finalize_latency < QUIET_AFTER_FORCE + Duration::from_millis(400));
    }

    #[tokio::test]
    async fn stop_terminates_the_session() {
        let server = server(150, "x").await;
        let aai = backend(&server, 60);
        let (tx, _rx) = mpsc::channel(8);
        aai.start(tx).await.unwrap();
        wait_for(|| server.connection_count() == 1).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        aai.stop().await;
        wait_for(|| {
            server
                .texts()
                .iter()
                .any(|t| t == r#"{"type":"Terminate"}"#)
        })
        .await;
    }
}
