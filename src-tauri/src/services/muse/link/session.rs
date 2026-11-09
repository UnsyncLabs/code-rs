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

use serde_json::Value;

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
// The live session orchestration (connect, handshake, control stream, read
// loop, chat send/subscribe) requires `tungstenite` + `rustls`, which are only
// available when the Tauri crate is built on a supported host. It is factored
// out behind `cfg(feature = "muse-net")` so the deterministic layers above can
// be compiled and unit-tested without pulling the networking stack in.
//
// TODO(muse-net): wire `run_session` into the service once the pairing
// transport (see docs/rust-rewrite/MUSE_PROTOCOL.md "Open question") provides a
// device token. The implementation uses:
//   * tungstenite::connect with an Authorization: Bearer <vm_auth_token> header
//     to wss://<noise_host>/v1/noise?vm_id=<vm_id>
//   * NoiseXXInitiator: send msg1 (binary), recv msg2, send msg3, split()
//   * NoiseTransport over the socket; open POST /link-control; send
//     link.register; read loop dispatching ControlEvent (Invoke -> callback,
//     Unpaired -> Outcome::Unpaired, RegisterRejected -> Forbidden)
//   * for muse_send: open POST /chat/subscribe (NDJSON), POST /chat/stream,
//     drive a ReplyTracker to the answer within the timeout.
