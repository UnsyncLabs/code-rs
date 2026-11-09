// Copyright (c) code-rs contributors. MIT license (see LICENSE.txt).
//
//! WebSocket + Noise session driving the link protocol, plus the reply
//! correlation logic (`muse_turn.py`) used by `muse_send(..., wait)`.
//!
//! The reply tracker (`ReplyTracker`) is pure and unit-tested. The WebSocket
//! session (`run_session`) uses `tungstenite` (sync, rustls) and is only
//! exercised against a live VM; it cannot be built/tested in this worktree
//! (needs the full Tauri/Windows toolchain), so it is written defensively.

use std::collections::BTreeMap;

use serde_json::{json, Value};

use super::strip_controls;

/// How a session ended (link_client.Outcome).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Closed,
    AuthRejected,
    Forbidden,
    Unpaired,
    Stopped,
}

/// Everything needed to open one session.
#[derive(Clone, Debug)]
pub struct SessionConfig {
    pub noise_host: String,
    pub vm_id: String,
    pub vm_auth_token: String,
}

// ---------------------------------------------------------------------------
// Reply correlation (muse_turn.py) — the subset needed for send-and-wait
// ---------------------------------------------------------------------------

const TERMINAL_TASK_STATUSES: &[&str] =
    &["completed", "failed", "errored", "cancelled", "canceled"];

#[derive(Debug, Clone, Default)]
struct Reply {
    text: String,
    streamed: String,
    done: bool,
    bound: bool,
}

/// Follows one request through Muse's chat events and extracts the reply text.
///
/// This mirrors the parts of `MuseTurn` + `_ReplyReader` that
/// `send_and_wait` relies on for a text turn: it binds to the acknowledged user
/// message id(s), follows assistant messages whose parent chain reaches that
/// id (or that arrive after our own `message.user`), accumulates streamed and
/// final text, and reports when the turn is complete/settled.
pub struct ReplyTracker {
    session_id: Option<String>,
    user_ids: Vec<String>,
    acknowledged: bool,
    after_note: bool,
    busy: bool,
    task_finished: bool,
    replies: BTreeMap<String, Reply>,
    last_seq: i64,
}

impl ReplyTracker {
    pub fn new(session_id: Option<String>) -> Self {
        ReplyTracker {
            session_id,
            user_ids: Vec::new(),
            acknowledged: false,
            after_note: false,
            busy: false,
            task_finished: false,
            replies: BTreeMap::new(),
            last_seq: 0,
        }
    }

    /// Bind the tracker to the user message ids from the chat acknowledgement.
    pub fn acknowledge(&mut self, user_ids: Vec<String>, session_id: Option<String>, fresh: bool) {
        self.user_ids = user_ids;
        if self.session_id.is_none() {
            self.session_id = session_id;
        }
        self.acknowledged = true;
        self.after_note = fresh;
    }

    /// Feed one raw chat event; dedupes by `seq`.
    pub fn take(&mut self, event: &Value) {
        if let Some(seq) = event.get("seq").and_then(Value::as_i64) {
            if seq <= self.last_seq {
                return;
            }
            self.last_seq = seq;
        }
        if !self.acknowledged {
            return; // caller buffers pre-ack events; we only track after ack
        }
        self.route(event);
    }

    fn route(&mut self, event: &Value) {
        if event.get("type").and_then(Value::as_str) != Some("event") {
            return;
        }
        let Some(payload) = event.get("payload") else { return };
        if !payload.is_object() {
            return;
        }
        let chat = payload
            .get("session_id")
            .and_then(Value::as_str)
            .or_else(|| event.get("session_id").and_then(Value::as_str));
        if let (Some(chat), Some(sid)) = (chat, &self.session_id) {
            if chat != sid {
                return; // another chat's events
            }
        }
        let message_id = payload
            .get("message_id")
            .and_then(Value::as_str)
            .or_else(|| event.get("message_id").and_then(Value::as_str))
            .or_else(|| payload.get("id").and_then(Value::as_str))
            .map(str::to_string);

        let name = event.get("event").and_then(Value::as_str).unwrap_or("");
        if name == "message.user" {
            self.after_note = message_id
                .as_deref()
                .map(|id| self.user_ids.iter().any(|u| u == id))
                .unwrap_or(false);
            return;
        }
        let parent = payload
            .get("reply_to_message_id")
            .and_then(Value::as_str)
            .or_else(|| payload.get("parent_message_id").and_then(Value::as_str))
            .map(str::to_string);

        let parent_in_users = parent
            .as_deref()
            .map(|p| self.user_ids.iter().any(|u| u == p))
            .unwrap_or(false);
        let parent_is_reply = parent
            .as_deref()
            .map(|p| self.replies.contains_key(p))
            .unwrap_or(false);
        let msg_is_reply = message_id
            .as_deref()
            .map(|m| self.replies.contains_key(m))
            .unwrap_or(false);

        if !(self.after_note || parent_in_users || msg_is_reply || parent_is_reply) {
            return;
        }

        match name {
            "agent.status" | "task.status" => self.status(payload, name),
            "delta.message_start" | "delta.text_append" | "delta.message_done"
            | "message.assistant" => {
                if let Some(mid) = message_id {
                    self.message(name, payload, &mid, parent_in_users || parent_is_reply);
                }
            }
            _ => {}
        }
    }

    fn status(&mut self, payload: &Value, name: &str) {
        let status = payload.get("status").and_then(Value::as_str);
        let activity = payload.get("activity_code").and_then(Value::as_str);
        let terminal = status.map(|s| TERMINAL_TASK_STATUSES.contains(&s)).unwrap_or(false);
        if name == "task.status" {
            if let Some(_s) = status {
                self.task_finished = terminal;
            }
        }
        if let Some(a) = activity {
            self.busy = !matches!(a, "" | "online" | "idle") && !TERMINAL_TASK_STATUSES.contains(&a);
        } else if let Some(s) = status {
            self.busy = !matches!(s, "" | "idle") && !terminal;
        }
        if name == "task.status" && self.task_finished {
            self.busy = false;
        }
    }

    fn message(&mut self, name: &str, payload: &Value, message_id: &str, parent_bound: bool) {
        if payload.get("role").and_then(Value::as_str) == Some("user") {
            return;
        }
        let entry = self.replies.entry(message_id.to_string()).or_default();
        entry.bound = entry.bound || parent_bound || self.after_note;

        if name == "delta.text_append" {
            if let Some(t) = payload.get("text").and_then(Value::as_str) {
                if !entry.done {
                    let delta = strip_controls(t);
                    entry.text.push_str(&delta);
                    entry.streamed.push_str(&delta);
                }
            }
        }
        if name == "delta.message_done" || name == "message.assistant" {
            let final_text = payload
                .get("display_text")
                .and_then(Value::as_str)
                .or_else(|| payload.get("transcript").and_then(Value::as_str))
                .or_else(|| payload.get("content").and_then(Value::as_str));
            if let Some(ft) = final_text {
                entry.text = strip_controls(ft);
            }
            if name == "delta.message_done"
                || payload.get("display_text_ready").and_then(Value::as_bool) != Some(false)
            {
                entry.done = true;
            }
        }
    }

    /// The reply text so far: bound replies joined by blank lines.
    pub fn reply_text(&self) -> String {
        self.replies
            .values()
            .filter(|r| !r.text.trim().is_empty())
            .map(|r| r.text.trim().to_string())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// Whether Muse is done answering (all replies done and quiet/terminal).
    pub fn complete(&self) -> bool {
        self.acknowledged
            && !self.busy
            && self.replies.values().all(|r| r.done)
            && (self.task_finished || !self.replies.is_empty())
    }

    pub fn settled(&self) -> bool {
        self.acknowledged
            && self.task_finished
            && !self.busy
            && self.replies.values().all(|r| r.done)
    }

    pub fn has_text(&self) -> bool {
        !self.reply_text().is_empty()
    }
}

// ---------------------------------------------------------------------------
// WebSocket + Noise session runner
// ---------------------------------------------------------------------------
//
// Opens a WebSocket to wss://<noise_host>/v1/noise?vm_id=<vm_id> with the
// per-VM bearer, runs the Noise XX handshake, then multiplexes HTTP-over-Noise
// streams (link-control, chat/stream, chat/subscribe) as the SDK's LinkSession
// does. Synchronous (tungstenite + rustls), driven by one reader thread.

use std::collections::HashMap;
use std::net::TcpStream;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

use super::super::transport::{
    DecryptedFrame, Header, NoiseTransport,
};
use super::super::noise::NoiseXXInitiator;
use super::{
    acknowledged_ids, chat_body, classify, encode_message, result_message, ControlEvent,
    DeviceDescription, Invoke, MessageDecoder, APP_ID, CHAT_PATH, CONTROL_PATH, NOISE_PATH,
    SUBSCRIBE_PATH,
};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// A request from a Tauri command into the running session thread.
pub enum SessionCommand {
    /// Send a user chat message and wait for the correlated reply.
    Send {
        text: String,
        session_id: Option<String>,
        timeout: Duration,
        reply: Sender<Result<super::super::MuseReply, String>>,
    },
    /// Answer a relayed `link.invoke` with a `link.result`.
    CommandResult { call_id: String, ok: bool, result: Value },
    /// Re-register with a new command set (muse_set_commands while connected).
    SetCommands(Vec<super::MuseCommand>),
    /// Stop the session.
    Stop,
}

/// What the session thread emits to the service for `muse://command`.
pub struct InvokeEvent {
    pub call_id: String,
    pub name: String,
    pub args: Value,
}

/// Open WebSocket + Noise transport, ready to send application requests.
struct WsNoise {
    ws: WebSocket<MaybeTlsStream<TcpStream>>,
    transport: NoiseTransport,
    decoder: MessageDecoder,
    /// Accumulated response bodies per stream id (for one-shot requests).
    bodies: HashMap<i64, Vec<u8>>,
    /// Response status per stream id.
    statuses: HashMap<i64, i32>,
    /// Stream ids whose body has ended.
    ended: HashMap<i64, bool>,
}

impl WsNoise {
    /// Connect, authenticate, and run the Noise XX handshake.
    fn connect(cfg: &SessionConfig) -> Result<Self, (String, Outcome)> {
        let url = format!(
            "wss://{}{}?vm_id={}",
            cfg.noise_host,
            NOISE_PATH,
            urlencode(&cfg.vm_id)
        );
        let request = tungstenite::http::Request::builder()
            .uri(&url)
            .header("Authorization", format!("Bearer {}", cfg.vm_auth_token))
            .header("Host", cfg.noise_host.clone())
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header("Sec-WebSocket-Key", tungstenite::handshake::client::generate_key())
            .header("User-Agent", "code-rs-muse")
            .header("x-app-id", APP_ID)
            .body(())
            .map_err(|e| (e.to_string(), Outcome::Closed))?;

        let (mut ws, response) = tungstenite::connect(request).map_err(|e| {
            // Map the upgrade status to an outcome where we can see it.
            if let tungstenite::Error::Http(resp) = &e {
                let status = resp.status().as_u16();
                let outcome = if status == 401 { Outcome::AuthRejected } else { Outcome::Forbidden };
                return (format!("VM refused the Noise upgrade: HTTP {status}"), outcome);
            }
            (e.to_string(), Outcome::Closed)
        })?;
        let _ = response;

        if let MaybeTlsStream::Plain(s) = ws.get_ref() {
            let _ = s.set_read_timeout(Some(Duration::from_secs(1)));
        } else if let MaybeTlsStream::Rustls(s) = ws.get_ref() {
            let _ = s.get_ref().set_read_timeout(Some(Duration::from_secs(1)));
        }

        // Noise XX: -> e ; <- e,ee,s,es ; -> s,se ; split.
        let mut init = NoiseXXInitiator::new();
        let m1 = init.write_message1().map_err(|e| (e.to_string(), Outcome::Closed))?;
        ws.send(Message::Binary(m1)).map_err(|e| (e.to_string(), Outcome::Closed))?;

        let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
        let m2 = read_binary(&mut ws, deadline).map_err(|e| (e, Outcome::Closed))?;
        init.read_message2(&m2).map_err(|e| (e.to_string(), Outcome::Closed))?;
        let m3 = init.write_message3().map_err(|e| (e.to_string(), Outcome::Closed))?;
        ws.send(Message::Binary(m3)).map_err(|e| (e.to_string(), Outcome::Closed))?;

        let (send, recv) = init.split().map_err(|e| (e.to_string(), Outcome::Closed))?;
        Ok(WsNoise {
            ws,
            transport: NoiseTransport::new(send, recv),
            decoder: MessageDecoder::new(),
            bodies: HashMap::new(),
            statuses: HashMap::new(),
            ended: HashMap::new(),
        })
    }

    /// Send every encrypted Noise record as one WebSocket binary frame.
    fn send_records(&mut self, records: Vec<Vec<u8>>) -> Result<(), String> {
        for record in records {
            self.ws
                .send(Message::Binary(record))
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Open a streaming request (e.g. /link-control, /chat/subscribe),
    /// returning its stream id. The body stays open for later chunks.
    fn open_stream(&mut self, method: &str, path: &str, headers: Vec<Header>) -> Result<i64, String> {
        let enc = self
            .transport
            .start_stream_request(method, path, headers)
            .map_err(|e| e.to_string())?;
        let id = enc.stream_id;
        self.send_records(enc.frames)?;
        Ok(id)
    }

    /// Send a complete-body request (e.g. /chat/stream) and return its stream id.
    fn send_request(
        &mut self,
        method: &str,
        path: &str,
        body: &[u8],
        headers: Vec<Header>,
    ) -> Result<i64, String> {
        let enc = self
            .transport
            .encrypt_http_request(method, path, body, headers)
            .map_err(|e| e.to_string())?;
        let id = enc.stream_id;
        self.send_records(enc.frames)?;
        Ok(id)
    }

    /// Send a body chunk on an open stream (control messages).
    fn send_body_chunk(&mut self, stream_id: i64, data: &[u8], end: bool) -> Result<(), String> {
        let records = self
            .transport
            .encrypt_body_chunk(stream_id, data, end)
            .map_err(|e| e.to_string())?;
        self.send_records(records)
    }

    /// Pump one inbound WebSocket frame into the transport, recording any
    /// decoded application response/body-chunk. Returns the stream id that
    /// produced data, if any. Times out quietly (returns Ok(None)).
    fn pump_once(&mut self) -> Result<Option<i64>, String> {
        let msg = match self.ws.read() {
            Ok(m) => m,
            Err(tungstenite::Error::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                return Ok(None)
            }
            Err(tungstenite::Error::ConnectionClosed) | Err(tungstenite::Error::AlreadyClosed) => {
                return Err("connection closed".into())
            }
            Err(e) => return Err(e.to_string()),
        };
        let ciphertext = match msg {
            Message::Binary(b) => b,
            Message::Ping(p) => {
                let _ = self.ws.send(Message::Pong(p));
                return Ok(None);
            }
            Message::Close(_) => return Err("connection closed".into()),
            _ => return Ok(None),
        };
        let Some(frame) = self.transport.decrypt_frame(&ciphertext).map_err(|e| e.to_string())? else {
            return Ok(None);
        };
        match frame {
            DecryptedFrame::Response { stream_id, value } => {
                self.statuses.insert(stream_id, value.status);
                let buf = self.bodies.entry(stream_id).or_default();
                buf.extend_from_slice(&value.body);
                if buf.len() > MAX_RESPONSE_BYTES {
                    return Err("response exceeded size budget".into());
                }
                if value.end_body {
                    self.ended.insert(stream_id, true);
                }
                Ok(Some(stream_id))
            }
            DecryptedFrame::BodyChunk { stream_id, value } => {
                let buf = self.bodies.entry(stream_id).or_default();
                buf.extend_from_slice(&value.data);
                if buf.len() > MAX_RESPONSE_BYTES {
                    return Err("response exceeded size budget".into());
                }
                if value.end_body {
                    self.ended.insert(stream_id, true);
                }
                Ok(Some(stream_id))
            }
            DecryptedFrame::Reset { stream_id, value } => {
                self.ended.insert(stream_id, true);
                log::warn!("muse: stream {stream_id} reset: {}", value.reason);
                Ok(Some(stream_id))
            }
        }
    }

    /// Drain buffered body bytes for a stream (for NDJSON / control framing).
    fn take_body(&mut self, stream_id: i64) -> Vec<u8> {
        self.bodies.remove(&stream_id).unwrap_or_default()
    }
}

fn read_binary(
    ws: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    deadline: Instant,
) -> Result<Vec<u8>, String> {
    loop {
        if Instant::now() >= deadline {
            return Err("handshake timed out".into());
        }
        match ws.read() {
            Ok(Message::Binary(b)) => return Ok(b),
            Ok(Message::Ping(p)) => {
                let _ = ws.send(Message::Pong(p));
            }
            Ok(Message::Close(_)) => return Err("closed during handshake".into()),
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// Percent-encode a vm_id for the query string (encodeURIComponent-compatible
/// enough for the ids the VM issues).
fn urlencode(value: &str) -> String {
    const SAFE: &[u8] = b"-_.!~*'()";
    let mut out = String::with_capacity(value.len());
    for &b in value.as_bytes() {
        if b.is_ascii_alphanumeric() || SAFE.contains(&b) {
            out.push(b as char);
        } else {
            out.push('%');
            out.push_str(&format!("{b:02X}"));
        }
    }
    out
}

/// Run one link session until it ends. `commands` is the initial command set;
/// `on_invoke` is called for each `link.invoke` (the service relays it to the
/// workbench as `muse://command`). Commands from the service arrive on `rx`.
///
/// Returns the terminal [`Outcome`]. This is only exercised against a live VM;
/// it is written defensively and cannot be built/tested without the Tauri host
/// toolchain, but reuses the fully unit-tested link/transport/noise layers.
#[allow(clippy::too_many_arguments)]
pub fn run_session(
    cfg: SessionConfig,
    device: DeviceDescription,
    rx: Receiver<SessionCommand>,
    on_invoke: Arc<dyn Fn(InvokeEvent) + Send + Sync>,
    on_registered: Arc<dyn Fn() + Send + Sync>,
) -> Outcome {
    let mut conn = match WsNoise::connect(&cfg) {
        Ok(c) => c,
        Err((msg, outcome)) => {
            log::warn!("muse: session connect failed: {msg}");
            return outcome;
        }
    };

    // Open the control stream and register.
    let register_id = new_uuid();
    let control_stream = match conn.open_stream("POST", CONTROL_PATH, vec![]) {
        Ok(id) => id,
        Err(e) => {
            log::warn!("muse: could not open control stream: {e}");
            return Outcome::Closed;
        }
    };
    let register_msg = device.register_message(&register_id);
    if let Err(e) = conn.send_body_chunk(control_stream, &encode_message(&register_msg), false) {
        log::warn!("muse: register send failed: {e}");
        return Outcome::Closed;
    }

    let mut registered = false;
    let mut pending_send: Option<PendingSend> = None;

    loop {
        // 1) Drain service commands (non-blocking).
        match rx.try_recv() {
            Ok(SessionCommand::Stop) => return Outcome::Stopped,
            Ok(SessionCommand::SetCommands(cmds)) => {
                let mut dev = device.clone();
                dev.commands = cmds;
                let msg = dev.register_message(&new_uuid());
                let _ = conn.send_body_chunk(control_stream, &encode_message(&msg), false);
            }
            Ok(SessionCommand::CommandResult { call_id, ok, result }) => {
                let msg = result_message(&call_id, ok, &result);
                if let Err(e) = conn.send_body_chunk(control_stream, &encode_message(&msg), false) {
                    log::warn!("muse: link.result send failed: {e}");
                }
            }
            Ok(SessionCommand::Send { text, session_id, timeout, reply }) => {
                if pending_send.is_some() {
                    let _ = reply.send(Err("a muse_send is already in progress".into()));
                } else if !registered {
                    let _ = reply.send(Err("not registered with the Muse VM yet".into()));
                } else {
                    match start_send(&mut conn, &device.node_id, &text, session_id.as_deref()) {
                        Ok(ps) => {
                            let ps = PendingSend { reply, deadline: Instant::now() + timeout, ..ps };
                            pending_send = Some(ps);
                        }
                        Err(e) => {
                            let _ = reply.send(Err(e));
                        }
                    }
                }
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => return Outcome::Stopped,
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }

        // 2) Pump one inbound frame.
        let stream_id = match conn.pump_once() {
            Ok(s) => s,
            Err(e) => {
                log::warn!("muse: read loop ended: {e}");
                if let Some(ps) = pending_send.take() {
                    let _ = ps.reply.send(Err("the Muse connection dropped".into()));
                }
                return Outcome::Closed;
            }
        };

        if let Some(sid) = stream_id {
            if sid == control_stream {
                // Control messages are length-prefixed JSON in the body bytes.
                let body = conn.take_body(sid);
                let messages = match conn.decoder.feed(&body) {
                    Ok(m) => m,
                    Err(e) => {
                        log::warn!("muse: control decode error: {e}");
                        return Outcome::Closed;
                    }
                };
                for message in messages {
                    match classify(&message, &register_id) {
                        ControlEvent::Registered => {
                            if !registered {
                                registered = true;
                                on_registered();
                            }
                        }
                        ControlEvent::RegisterRejected => return Outcome::Forbidden,
                        ControlEvent::Unpaired => return Outcome::Unpaired,
                        ControlEvent::Invoke(Invoke { call_id, command, params, .. }) => {
                            on_invoke(InvokeEvent { call_id, name: command, args: params });
                        }
                        ControlEvent::Other => {}
                    }
                }
            } else if let Some(ps) = pending_send.as_mut() {
                ps.feed(&mut conn, sid);
            }
        }

        // 3) Resolve a completed/expired send.
        let resolve = pending_send
            .as_ref()
            .map(|ps| (ps.is_done(), Instant::now() >= ps.deadline));
        if let Some((done, expired)) = resolve {
            if done {
                let ps = pending_send.take().unwrap();
                let reply = ps.reply;
                let answer = super::super::MuseReply {
                    text: ps.tracker.reply_text(),
                    session_id: ps.tracker_session_id,
                };
                let _ = reply.send(Ok(answer));
            } else if expired {
                let ps = pending_send.take().unwrap();
                let text = ps.tracker.reply_text();
                if text.is_empty() {
                    let _ = ps.reply.send(Err("timed out waiting for the Muse reply".into()));
                } else {
                    let session_id = ps.tracker_session_id.clone();
                    let _ = ps.reply.send(Ok(super::super::MuseReply { text, session_id }));
                }
            }
        }

        std::thread::sleep(Duration::from_millis(5));
    }
}

/// State for an in-flight muse_send awaiting its reply.
struct PendingSend {
    reply: Sender<Result<super::super::MuseReply, String>>,
    deadline: Instant,
    chat_stream: i64,
    subscribe_stream: i64,
    tracker: ReplyTracker,
    tracker_session_id: Option<String>,
    ndjson: Vec<u8>,
    acked: bool,
    quiet_since: Option<Instant>,
}

impl PendingSend {
    fn feed(&mut self, conn: &mut WsNoise, sid: i64) {
        if sid == self.subscribe_stream {
            let body = conn.take_body(sid);
            self.ndjson.extend_from_slice(&body);
            // Split complete NDJSON lines.
            while let Some(pos) = self.ndjson.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = self.ndjson.drain(..=pos).collect();
                let line = &line[..line.len() - 1];
                if line.is_empty() {
                    continue;
                }
                if let Ok(event) = serde_json::from_slice::<Value>(line) {
                    self.tracker.take(&event);
                }
            }
            self.quiet_since = Some(Instant::now());
        } else if sid == self.chat_stream {
            // The /chat/stream response is the acknowledgement (one JSON body).
            let body = conn.take_body(sid);
            if !self.acked {
                if let Ok(ack) = serde_json::from_slice::<Value>(&body) {
                    if let Ok((ids, session_id)) = acknowledged_ids(&ack) {
                        if self.tracker_session_id.is_none() {
                            self.tracker_session_id = session_id.clone();
                        }
                        self.tracker.acknowledge(ids, session_id, true);
                        self.acked = true;
                    }
                }
            }
        }
    }

    fn is_done(&self) -> bool {
        if !self.acked {
            return false;
        }
        if self.tracker.settled() {
            return true;
        }
        // Quiet grace: complete + ~3s of silence counts as done.
        if self.tracker.complete() && self.tracker.has_text() {
            if let Some(since) = self.quiet_since {
                return since.elapsed() >= Duration::from_secs(3);
            }
        }
        false
    }
}

/// Open /chat/subscribe (NDJSON) then POST /chat/stream for one user message.
fn start_send(
    conn: &mut WsNoise,
    node_id: &str,
    text: &str,
    session_id: Option<&str>,
) -> Result<PendingSend, String> {
    // Subscribe first so we do not miss the reply events.
    let subscribe_body = match session_id {
        Some(sid) => json!({ "session_id": sid }),
        None => json!({}),
    };
    let subscribe_stream = {
        let enc = conn
            .transport
            .start_stream_request(
                "POST",
                SUBSCRIBE_PATH,
                vec![Header::new("Accept", "application/x-ndjson"), Header::new("Content-Type", "application/json")],
            )
            .map_err(|e| e.to_string())?;
        let id = enc.stream_id;
        conn.send_records(enc.frames)?;
        conn.send_body_chunk(id, subscribe_body.to_string().as_bytes(), true)?;
        id
    };

    let body = chat_body(text, node_id, session_id);
    let chat_stream = conn.send_request(
        "POST",
        CHAT_PATH,
        body.to_string().as_bytes(),
        vec![
            Header::new("Content-Type", "application/json"),
            Header::new("x-request-id", &new_uuid()),
            Header::new("x-app-id", APP_ID),
        ],
    )?;

    Ok(PendingSend {
        reply: std::sync::mpsc::channel().0, // replaced by caller
        deadline: Instant::now() + Duration::from_secs(600),
        chat_stream,
        subscribe_stream,
        tracker: ReplyTracker::new(session_id.map(str::to_string)),
        tracker_session_id: session_id.map(str::to_string),
        ndjson: Vec::new(),
        acked: false,
        quiet_since: None,
    })
}

/// A UUID4 string without external deps (RFC 4122 v4 from getrandom).
fn new_uuid() -> String {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("getrandom");
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}
