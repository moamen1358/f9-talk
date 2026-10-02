//! A scripted local WebSocket server that stands in for a cloud STT API in
//! unit tests. Each incoming client message is passed to a handler that
//! returns the replies to send, each after its own delay, so a test can
//! reproduce real server timing (a final that lands 500 ms after the
//! finalize request, a connection that drops mid-press, and so on).

// The tungstenite handshake callback signature returns a large Err type.
#![allow(clippy::result_large_err)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::Message;

/// What the client sent.
#[derive(Debug, Clone)]
pub(crate) enum MockIn {
    /// A binary audio frame of this many bytes.
    Audio(usize),
    /// A text (JSON control) frame.
    Text(String),
}

/// One reply: send `Text(json)` or close the socket, after `delay`.
#[derive(Debug, Clone)]
pub(crate) enum MockOut {
    Text(String),
    Close,
}

pub(crate) struct Reply {
    pub delay: Duration,
    pub out: MockOut,
}

pub(crate) fn text_after(ms: u64, json: impl Into<String>) -> Reply {
    Reply {
        delay: Duration::from_millis(ms),
        out: MockOut::Text(json.into()),
    }
}

pub(crate) fn close_after(ms: u64) -> Reply {
    Reply {
        delay: Duration::from_millis(ms),
        out: MockOut::Close,
    }
}

/// Context handed to the handler for each client message.
pub(crate) struct Ctx {
    /// 0 for the first connection, 1 for the first reconnect, ...
    pub connection: usize,
    /// Audio bytes received on this connection so far (including this message).
    pub audio_bytes: usize,
}

type Handler = dyn Fn(&Ctx, &MockIn) -> Vec<Reply> + Send + Sync;
type OnConnect = dyn Fn(usize) -> Vec<Reply> + Send + Sync;

pub(crate) struct MockServer {
    pub url: String,
    /// Request URI and Authorization header of every connection.
    pub requests: Arc<Mutex<Vec<(String, String)>>>,
    /// Every client message, in order, tagged with its connection index.
    pub received: Arc<Mutex<Vec<(usize, MockIn)>>>,
    pub connections: Arc<AtomicUsize>,
}

impl MockServer {
    pub fn connection_count(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    /// Text messages the client sent, in order.
    pub fn texts(&self) -> Vec<String> {
        self.received
            .lock()
            .iter()
            .filter_map(|(_, m)| match m {
                MockIn::Text(t) => Some(t.clone()),
                MockIn::Audio(_) => None,
            })
            .collect()
    }

    /// Sizes of the audio frames the client sent, in order.
    pub fn audio_frames(&self) -> Vec<usize> {
        self.received
            .lock()
            .iter()
            .filter_map(|(_, m)| match m {
                MockIn::Audio(n) => Some(*n),
                MockIn::Text(_) => None,
            })
            .collect()
    }
}

pub(crate) async fn spawn_mock(
    on_connect: impl Fn(usize) -> Vec<Reply> + Send + Sync + 'static,
    handler: impl Fn(&Ctx, &MockIn) -> Vec<Reply> + Send + Sync + 'static,
) -> MockServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let received = Arc::new(Mutex::new(Vec::new()));
    let connections = Arc::new(AtomicUsize::new(0));
    let handler: Arc<Handler> = Arc::new(handler);
    let on_connect: Arc<OnConnect> = Arc::new(on_connect);

    let (req_c, rec_c, conn_c) = (requests.clone(), received.clone(), connections.clone());
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let index = conn_c.fetch_add(1, Ordering::SeqCst);
            let req_c = req_c.clone();
            let callback = move |req: &Request, resp: Response| {
                let auth = req
                    .headers()
                    .get("Authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_string();
                req_c.lock().push((req.uri().to_string(), auth));
                Ok(resp)
            };
            let Ok(ws) = tokio_tungstenite::accept_hdr_async(tcp, callback).await else {
                continue;
            };
            tokio::spawn(serve(
                ws,
                index,
                handler.clone(),
                on_connect.clone(),
                rec_c.clone(),
            ));
        }
    });

    MockServer {
        url: format!("ws://{addr}/"),
        requests,
        received,
        connections,
    }
}

async fn serve(
    ws: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    index: usize,
    handler: Arc<Handler>,
    on_connect: Arc<OnConnect>,
    received: Arc<Mutex<Vec<(usize, MockIn)>>>,
) {
    let (mut sink, mut source) = ws.split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<MockOut>();
    let schedule = |replies: Vec<Reply>| {
        for r in replies {
            let tx = out_tx.clone();
            tokio::spawn(async move {
                tokio::time::sleep(r.delay).await;
                let _ = tx.send(r.out);
            });
        }
    };
    schedule(on_connect(index));
    let mut audio_bytes = 0usize;
    loop {
        tokio::select! {
            out = out_rx.recv() => match out {
                Some(MockOut::Text(t)) => {
                    if sink.send(Message::Text(t.into())).await.is_err() {
                        return;
                    }
                }
                Some(MockOut::Close) | None => {
                    let _ = sink.close().await;
                    return;
                }
            },
            msg = source.next() => {
                let incoming = match msg {
                    Some(Ok(Message::Binary(b))) => {
                        audio_bytes += b.len();
                        MockIn::Audio(b.len())
                    }
                    Some(Ok(Message::Text(t))) => MockIn::Text(t.to_string()),
                    Some(Ok(_)) => continue,
                    _ => return,
                };
                received.lock().push((index, incoming.clone()));
                let ctx = Ctx { connection: index, audio_bytes };
                schedule(handler(&ctx, &incoming));
            }
        }
    }
}
