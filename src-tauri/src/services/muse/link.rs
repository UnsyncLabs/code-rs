// Copyright (c) code-rs contributors. MIT license (see LICENSE.txt).
//
//! The link protocol layer (link_client.py): length-prefixed JSON control
//! messages, the device description / register message, and the parsing of
//! `link.invoke` / `link.result` / `link.unpaired`.
//!
//! This module is deliberately transport-agnostic and side-effect free so it
//! can be unit-tested without a network. The WebSocket + Noise session that
//! drives it lives in `session` below and is only compiled where `tungstenite`
//! is available.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const NOISE_PATH: &str = "/v1/noise";
pub const CONTROL_PATH: &str = "/link-control";
pub const CHAT_PATH: &str = "/chat/stream";
pub const SUBSCRIBE_PATH: &str = "/chat/subscribe";
pub const APP_ID: &str = "musegadget";
pub const MAX_INBOUND_MESSAGE: u32 = 4 * 1024 * 1024;
pub const MAX_CONTROL_MESSAGE_BYTES: usize = 256 * 1024;

// ---------------------------------------------------------------------------
// Device description / commands (Contract M MuseCommand + register params)
// ---------------------------------------------------------------------------

/// A command code-rs exposes to the Muse agent (Contract M).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct MuseCommand {
    pub name: String,
    pub description: String,
    /// JSON Schema object describing the command's input.
    pub input_schema: Value,
}

#[derive(Clone, Debug)]
pub struct DeviceDescription {
    pub node_id: String,
    pub display_name: String,
    pub version: String,
    /// Commands keyed by name, serialized into `commands_v2`.
    pub commands: Vec<MuseCommand>,
}

impl DeviceDescription {
    /// Build the `commands_v2` map Muse expects. The SDK's spec shape is
    /// `{ description, required, optional, timeout_ms? }`; we map a JSON-Schema
    /// `inputSchema` onto `required`/`optional` so the Muse agent can call it.
    pub fn commands_v2(&self) -> Value {
        let mut map = serde_json::Map::new();
        for cmd in &self.commands {
            map.insert(cmd.name.clone(), command_spec(&cmd.description, &cmd.input_schema));
        }
        Value::Object(map)
    }

    pub fn register_params(&self) -> Value {
        json!({
            "node_id": self.node_id,
            "display_name": self.display_name,
            "platform": "linux",
            "version": self.version,
            "device_family": "homehub",
            "model_id": "linux",
            "is_wakeup_supported": false,
            "commands_v2": self.commands_v2(),
        })
    }

    pub fn register_message(&self, request_id: &str) -> Value {
        json!({
            "type": "req",
            "id": request_id,
            "method": "link.register",
            "params": self.register_params(),
        })
    }
}

/// Convert a JSON-Schema object into the SDK command spec shape.
fn command_spec(description: &str, input_schema: &Value) -> Value {
    let props = input_schema.get("properties").and_then(Value::as_object);
    let required: Vec<&str> = input_schema
        .get("required")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();

    let mut required_map = serde_json::Map::new();
    let mut optional_map = serde_json::Map::new();
    if let Some(props) = props {
        for (name, schema) in props {
            let entry = json!({
                "type": schema.get("type").and_then(Value::as_str).unwrap_or("string"),
                "description": schema.get("description").and_then(Value::as_str).unwrap_or(""),
            });
            if required.contains(&name.as_str()) {
                required_map.insert(name.clone(), entry);
            } else {
                optional_map.insert(name.clone(), entry);
            }
        }
    }
    json!({
        "description": description,
        "required": Value::Object(required_map),
        "optional": Value::Object(optional_map),
    })
}

// ---------------------------------------------------------------------------
// Length-prefixed JSON framing (link_client.py:encode_message / MessageDecoder)
// ---------------------------------------------------------------------------

/// Encode a control message: `u32 little-endian length` ++ compact JSON.
pub fn encode_message(obj: &Value) -> Vec<u8> {
    let data = serde_json::to_vec(obj).expect("json");
    let mut out = Vec::with_capacity(4 + data.len());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&data);
    out
}

/// Splits the control stream into length-prefixed JSON messages across chunks.
#[derive(Default)]
pub struct MessageDecoder {
    buf: Vec<u8>,
}

impl MessageDecoder {
    pub fn new() -> Self {
        MessageDecoder::default()
    }

    pub fn feed(&mut self, data: &[u8]) -> Result<Vec<Value>, String> {
        self.buf.extend_from_slice(data);
        let mut messages = Vec::new();
        loop {
            if self.buf.len() < 4 {
                break;
            }
            let length =
                u32::from_le_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
            if length as u32 > MAX_INBOUND_MESSAGE {
                return Err(format!("inbound message too large: {length}"));
            }
            if self.buf.len() < 4 + length {
                break;
            }
            let raw = self.buf[4..4 + length].to_vec();
            self.buf.drain(..4 + length);
            if raw.is_empty() {
                continue; // keepalive
            }
            match serde_json::from_slice::<Value>(&raw) {
                Ok(v) if v.is_object() => messages.push(v),
                _ => {
                    log::warn!("muse: dropping malformed control message ({} bytes)", length);
                }
            }
        }
        Ok(messages)
    }
}

// ---------------------------------------------------------------------------
// Control message semantics
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlEvent {
    /// Registration acknowledged (matches our register id, no method).
    Registered,
    /// Registration rejected by the VM.
    RegisterRejected,
    /// The Muse removed this device.
    Unpaired,
    /// A command invocation request from the Muse.
    Invoke(Invoke),
    /// Anything else (ignored).
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invoke {
    pub call_id: String,
    pub command: String,
    pub params: Value,
    pub timeout_ms: Option<u64>,
}

/// Classify one decoded control message against our register id.
pub fn classify(message: &Value, register_id: &str) -> ControlEvent {
    if message.get("id").and_then(Value::as_str) == Some(register_id)
        && message.get("method").is_none()
    {
        let rejected = message.get("error").is_some()
            || message.get("ok").and_then(Value::as_bool) == Some(false);
        return if rejected {
            ControlEvent::RegisterRejected
        } else {
            ControlEvent::Registered
        };
    }
    match message.get("event").and_then(Value::as_str) {
        Some("link.unpaired") | Some("node.unpaired") => return ControlEvent::Unpaired,
        _ => {}
    }
    if message.get("method").and_then(Value::as_str) == Some("link.invoke") {
        if let Some(call_id) = message.get("id").and_then(Value::as_str) {
            return ControlEvent::Invoke(Invoke {
                call_id: call_id.to_string(),
                command: message
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                params: message.get("params").cloned().unwrap_or(Value::Null),
                timeout_ms: message.get("timeout_ms").and_then(Value::as_u64),
            });
        }
    }
    ControlEvent::Other
}

/// Build a `link.result` control message.
pub fn result_message(call_id: &str, ok: bool, result: &Value) -> Value {
    if ok {
        json!({ "method": "link.result", "id": call_id, "ok": true, "payload": result })
    } else {
        let error = result.as_str().map(|s| s.to_string()).unwrap_or_else(|| result.to_string());
        json!({ "method": "link.result", "id": call_id, "ok": false, "error": error })
    }
}

// ---------------------------------------------------------------------------
// Chat (link_client.py:send_chat, service.py:send_and_wait)
// ---------------------------------------------------------------------------

/// Body for `POST /chat/stream`.
pub fn chat_body(message: &str, node_id: &str, session_id: Option<&str>) -> Value {
    let mut body = json!({
        "message": message,
        "output_modality": "text",
        "device_id": node_id,
    });
    if let Some(sid) = session_id {
        body["session_id"] = json!(sid);
    }
    body
}

/// Extract the acknowledgement from a `/chat/stream` response body
/// (`acknowledgement()` in link_client.py). Returns `(message_ids, session_id)`.
pub fn acknowledged_ids(response: &Value) -> Result<(Vec<String>, Option<String>), String> {
    let result = response.get("result").unwrap_or(response);
    if !result.is_object() {
        return Err("Muse returned an invalid chat acknowledgement".into());
    }
    let mut ids = Vec::new();
    for key in ["message_id", "reply_to_message_id"] {
        if let Some(v) = result.get(key).and_then(Value::as_str) {
            if !v.is_empty() {
                ids.push(v.to_string());
            }
        }
    }
    if ids.is_empty() {
        return Err("Muse chat acknowledgement omitted the user message ID".into());
    }
    let session_id = result.get("session_id").and_then(Value::as_str).map(str::to_string);
    Ok((ids, session_id))
}

/// Strip C0/C1 control characters except tab and newline (muse_turn.py).
pub fn strip_controls(text: &str) -> String {
    text.chars()
        .filter(|c| {
            let u = *c as u32;
            *c == '\t' || *c == '\n' || !(u <= 0x08 || (0x0b..=0x1f).contains(&u) || (0x7f..=0x9f).contains(&u))
        })
        .collect()
}

pub use session::{Outcome, SessionConfig};

pub mod session;
