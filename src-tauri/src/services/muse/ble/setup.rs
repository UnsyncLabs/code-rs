// Copyright (c) code-rs contributors. MIT license (see LICENSE.txt).
//
//! BLE setup command dispatch — the protocol logic behind the GATT RX/TX
//! characteristics, independent of the platform BLE stack (`ble_setup.py`).
//!
//! A `Transport` delivers reassembled writes via [`SetupController::on_write`]
//! and sends framed notifications via [`Transport::send_packets`]. Messages are
//! handled one at a time in arrival order. The controller verifies the device
//! token (over the real Muse API) before telling the app `auth_ok`, and hands
//! the validated [`Credentials`] to its `commit` callback so the caller can
//! persist them (keychain + muse.json) under its own lock.

use serde_json::Value;

use super::framing::{encode_chunks, ChunkAssembler};
use super::pairing::{parse_provision, Credentials, Envelope, PairingError, PairingSession};

/// Actions that must only ever run inside the encrypted channel.
fn is_sensitive(action: &str) -> bool {
    const SENSITIVE: &[&str] = &[
        "provision", "provision_v2", "wifi_scan", "ota", "device.ota", "unpair",
        "set_wifi", "set_auth",
    ];
    SENSITIVE.contains(&action)
}

/// Statuses the device is still allowed to send in plaintext (handshake errors).
fn plaintext_status_allowed(status: &str) -> bool {
    const ALLOWED: &[&str] = &[
        "error_encryption_required",
        "error_pairing_invalid_hello",
        "error_pairing_unavailable",
        "error_pairing_decrypt",
    ];
    ALLOWED.contains(&status)
}

/// What the controller needs from the platform BLE layer.
pub trait Transport: Send {
    /// Notify each packet in order; blocks until all are queued.
    fn send_packets(&self, packets: &[Vec<u8>]);
    /// Negotiated ATT MTU (used to size chunks).
    fn mtu(&self) -> usize;
    /// Ask the platform to drop the link after `delay` seconds.
    fn disconnect(&self, delay: f64);
}

/// Outcome of verifying+committing the provisioned credentials. On success the
/// caller has persisted the device token; on failure it returns the wire status
/// string the app should see (e.g. `auth_failed`, `error_storage`).
pub type Commit<'a> = dyn FnMut(&Credentials) -> Result<(), String> + Send + 'a;

pub struct SetupController<'a> {
    pairing: PairingSession,
    node_id: String,
    version: String,
    network_online: bool,
    transport: &'a dyn Transport,
    commit: Box<Commit<'a>>,
    assembler: ChunkAssembler,
    plaintext_blocked: bool,
    provisioning: bool,
    completed: bool,
}

impl<'a> SetupController<'a> {
    pub fn new(
        pairing: PairingSession,
        node_id: &str,
        version: &str,
        network_online: bool,
        transport: &'a dyn Transport,
        commit: Box<Commit<'a>>,
    ) -> Self {
        SetupController {
            pairing,
            node_id: node_id.to_string(),
            version: version.to_string(),
            network_online,
            transport,
            commit,
            assembler: ChunkAssembler::new(),
            plaintext_blocked: false,
            provisioning: false,
            completed: false,
        }
    }

    /// True once `provision_v2` has verified and committed a device token.
    pub fn completed(&self) -> bool {
        self.completed
    }

    /// Feed one raw BLE write (one GATT packet). Dispatches a full message once
    /// reassembly completes.
    pub fn on_write(&mut self, packet: &[u8]) {
        if let Some(message) = self.assembler.feed(packet) {
            self.handle_message(&message, false);
        }
    }

    /// The BLE client disconnected: clear the in-flight pairing session.
    pub fn on_disconnect(&mut self) {
        self.assembler.reset();
        self.plaintext_blocked = false;
        self.provisioning = false;
        self.pairing.reset();
    }

    fn handle_message(&mut self, raw: &[u8], decrypted: bool) {
        let Ok(text) = std::str::from_utf8(raw) else {
            self.send_status("error_invalid_command");
            return;
        };
        let Ok(command) = serde_json::from_str::<Value>(text) else {
            self.send_status("error_invalid_command");
            return;
        };
        if !command.is_object() {
            self.send_status("error_invalid_command");
            return;
        }
        let action = command.get("action").and_then(Value::as_str).unwrap_or("");

        if !decrypted && action == "pairing_client_hello" {
            self.handle_hello(&command);
        } else if !decrypted && action == "pairing_encrypted" {
            self.handle_record(&command);
        } else if action == "get_device_info" {
            let info = self.device_info();
            self.send_json(&info);
        } else if !decrypted && self.plaintext_blocked {
            // ignore stray plaintext once a handshake has started
        } else if !decrypted && is_sensitive(action) {
            self.send_status("error_encryption_required");
        } else if decrypted && action == "pairing_client_finished" {
            self.handle_client_finished(&command);
        } else if decrypted && is_sensitive(action) && !self.pairing.confirmed() {
            self.send_status("error_pairing_confirm_required");
        } else if decrypted && action == "wifi_scan" {
            // This device only sets up when already online; report nothing.
            self.send_encrypted_json(&serde_json::json!({
                "type": "wifi_scan_result", "networks": []
            }));
        } else if decrypted && action == "provision_v2" {
            self.handle_provision(&command);
        } else {
            self.send_status("error_unknown_action");
        }
    }

    fn device_info(&self) -> Value {
        let mut info = self.pairing.device_info();
        if let Value::Object(map) = &mut info {
            map.insert("type".into(), Value::String("device_info".into()));
            map.insert("node_id".into(), Value::String(self.node_id.clone()));
            map.insert("version".into(), Value::String(self.version.clone()));
            map.insert("build_sha".into(), Value::String(String::new()));
            map.insert("network_ready".into(), Value::Bool(self.network_online));
        }
        info
    }

    fn handle_hello(&mut self, command: &Value) {
        match self.pairing.handle_hello(command) {
            Ok(ready) => {
                self.plaintext_blocked = true;
                self.send_json(&ready);
            }
            Err(PairingError { status }) => self.send_status(&status),
        }
    }

    fn handle_record(&mut self, envelope_json: &Value) {
        let envelope = match Envelope::from_json(envelope_json) {
            Ok(e) => e,
            Err(_) => {
                self.send_status("error_pairing_decrypt");
                self.transport.disconnect(0.3);
                return;
            }
        };
        match self.pairing.decrypt(&envelope) {
            Ok(plaintext) => self.handle_message(plaintext.as_bytes(), true),
            Err(PairingError { status }) => {
                self.send_status(&status);
                self.transport.disconnect(0.3);
            }
        }
    }

    fn handle_client_finished(&mut self, command: &Value) {
        if self.pairing.handle_client_finished(command) {
            self.send_status("pairing_confirmed");
        } else {
            self.send_status("error_pairing_decrypt");
            self.transport.disconnect(0.3);
        }
    }

    fn handle_provision(&mut self, command: &Value) {
        let credentials = match parse_provision(command) {
            Ok(c) => c,
            Err(status) => {
                self.send_status(&status);
                return;
            }
        };
        if self.provisioning {
            self.send_status("error_operation_in_progress");
            return;
        }
        if !self.pairing.mark_provisioning() {
            self.send_status("error_pairing_confirm_required");
            return;
        }
        self.provisioning = true;

        self.send_status("wifi_connecting");
        if !self.network_online {
            self.send_status("wifi_failed");
            self.provisioning = false;
            return;
        }
        self.send_status("wifi_connected");

        match (self.commit)(&credentials) {
            Ok(()) => {
                self.send_status("auth_ok");
                self.completed = true;
            }
            Err(status) => {
                self.send_status(&status);
                self.transport.disconnect(0.5);
            }
        }
        self.provisioning = false;
    }

    // -- Sending ---------------------------------------------------------------

    fn send_status(&mut self, status: &str) {
        if let Some(envelope) = self.pairing.encrypt_status(status) {
            self.send_text(&envelope.to_json().to_string());
            return;
        }
        // No session yet: only a few handshake errors may go out in plaintext.
        if self.plaintext_blocked || !plaintext_status_allowed(status) {
            return;
        }
        self.transport.send_packets(&[status.as_bytes().to_vec()]);
    }

    fn send_json(&mut self, obj: &Value) {
        self.send_text(&obj.to_string());
    }

    fn send_encrypted_json(&mut self, obj: &Value) {
        if let Some(envelope) = self.pairing.encrypt_json(&obj.to_string()) {
            self.send_text(&envelope.to_json().to_string());
        }
    }

    fn send_text(&self, text: &str) {
        match encode_chunks(text.as_bytes(), self.transport.mtu()) {
            Ok(packets) => self.transport.send_packets(&packets),
            Err(e) => log::warn!("muse/ble: cannot frame {} byte message: {e}", text.len()),
        }
    }
}
