// Copyright (c) code-rs contributors. MIT license (see LICENSE.txt).
//
//! Device side of Muse Gadget BLE pairing, protocol version 5, community mode
//! (`pairing_auth = "none"`, policy `confirm_app`). Ported from the SDK's
//! `pairing.py` and cross-checked against the ESP32 firmware (`link_pairing`),
//! so the existing Muse phone apps pair with code-rs unchanged.
//!
//! Wire shape (all BLE JSON, base64url without padding for binary):
//!   app -> device  `pairing_client_hello` { version, pairing_auth,
//!                     pairing_policy, mobile_pub, mobile_nonce }
//!   device -> app  `pairing_ready` { device_pub, device_nonce,
//!                     transcript_hash, session_id, ... }
//!   app -> device  `pairing_encrypted` { session_id, counter, ciphertext, tag }
//!                   wrapping `{ "action": "pairing_client_finished" }`
//!   device -> app  encrypted `{ "type":"status","status":"pairing_confirmed",
//!                     "sdk_token"? }`
//!   app -> device  encrypted `provision_v2` { access_token, refresh_token,
//!                     token_type:"device", api_url_v2, noise_host, ... }
//!   device -> app  encrypted status updates (wifi_connecting, auth_ok, ...)
//!
//! Key schedule (community v5): P-256 ECDH -> HKDF-SHA256 with
//! `salt = SHA256(mobile_nonce || device_nonce || transcript_hash)` and
//! `info = "hatch-link ble setup v1"` -> `session_secret`; then
//! `HKDF-Expand(info="mobile->device")` decrypts app->device records and
//! `HKDF-Expand(info="device->mobile")` encrypts device->app records. The
//! session id is `SHA256("hatch-link session id v1" || transcript_hash ||
//! ecdh_secret)[..16]`. Record nonce = `dir(1) || 0 0 0 || counter_be_u64`;
//! AAD = `"hatch-link ble setup v1|<session_id_b64>|<m2d|d2m>|<counter>"`.

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use hkdf::Hkdf;
use p256::ecdh::diffie_hellman;
use p256::elliptic_curve::sec1::ToEncodedPoint;
use p256::{EncodedPoint, PublicKey, SecretKey};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub const PAIRING_VERSION: i64 = 5;
pub const PAIRING_MODEL: &str = "hatch_link";
pub const PAIRING_SUITE: &str = "p256-hkdf-sha256-aes-gcm-v1";
pub const POLICY_APP: &str = "confirm_app";
pub const AUTH_COMMUNITY: &str = "none";
pub const RECORD_LABEL: &str = "hatch-link ble setup v1";
pub const SESSION_ID_LABEL: &[u8] = b"hatch-link session id v1";

const P256_POINT_BYTES: usize = 65;
const NONCE_BYTES: usize = 16;
const SESSION_ID_BYTES: usize = 16;
const TAG_BYTES: usize = 16;
const DIR_TO_DEVICE: u8 = 0;
const DIR_FROM_DEVICE: u8 = 1;

pub const ERROR_INVALID_HELLO: &str = "error_pairing_invalid_hello";
pub const ERROR_DECRYPT: &str = "error_pairing_decrypt";

/// A handshake step failed; `status` is the wire error to report to the app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingError {
    pub status: String,
}

impl PairingError {
    fn new(status: &str) -> Self {
        PairingError { status: status.into() }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingState {
    Idle,
    WaitClientFinished,
    Ready,
    Provisioning,
}

pub fn b64url_encode(data: &[u8]) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;
    URL_SAFE_NO_PAD.encode(data)
}

pub fn b64url_decode(text: &str) -> Result<Vec<u8>, String> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;
    if text.is_empty() || text.len() % 4 == 1 {
        return Err("invalid base64url length".into());
    }
    if !text.bytes().all(|b| {
        b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
    }) {
        return Err("invalid base64url".into());
    }
    URL_SAFE_NO_PAD
        .decode(text)
        .map_err(|_| "invalid base64url".into())
}

/// Canonical v5 transcript; its SHA-256 is the `transcript_hash` both sides
/// bind. `build_transcript` in `pairing.py`, community `confirm_app` branch.
#[allow(clippy::too_many_arguments)]
pub fn build_transcript(
    device_id: &str,
    node_id: &str,
    mac: &str,
    firmware_version: &str,
    mobile_pub_b64: &str,
    device_pub_b64: &str,
    mobile_nonce_b64: &str,
    device_nonce_b64: &str,
) -> String {
    [
        format!("hatch-link-pairing-v{PAIRING_VERSION}"),
        format!("version={PAIRING_VERSION}"),
        "initiator_role=mobile".into(),
        "responder_role=link".into(),
        format!("device_id={device_id}"),
        format!("node_id={node_id}"),
        format!("mac={mac}"),
        format!("model={PAIRING_MODEL}"),
        format!("firmware_version={firmware_version}"),
        format!("selected_cipher_suite={PAIRING_SUITE}"),
        format!("pairing_auth={AUTH_COMMUNITY}"),
        "pairing_auth_epoch=0".into(),
        format!("pairing_policy={POLICY_APP}"),
        "confirm_timeout_seconds=0".into(),
        format!("mobile_pub={mobile_pub_b64}"),
        format!("device_pub={device_pub_b64}"),
        format!("mobile_nonce={mobile_nonce_b64}"),
        format!("device_nonce={device_nonce_b64}"),
    ]
    .join("\n")
}

/// Returns `(mobile_tx_key, mobile_rx_key, session_id)`. `mobile_tx_key`
/// *decrypts* records the device receives; `mobile_rx_key` *encrypts* records
/// the device sends (`derive_session_keys` in `pairing.py`).
pub fn derive_session_keys(
    ecdh_secret: &[u8],
    mobile_nonce: &[u8],
    device_nonce: &[u8],
    transcript_hash: &[u8],
) -> ([u8; 32], [u8; 32], [u8; SESSION_ID_BYTES]) {
    let mut salt_hasher = Sha256::new();
    salt_hasher.update(mobile_nonce);
    salt_hasher.update(device_nonce);
    salt_hasher.update(transcript_hash);
    let salt = salt_hasher.finalize();

    // session_secret = HKDF(salt, ikm=ecdh_secret, info=RECORD_LABEL, len=32).
    let hk = Hkdf::<Sha256>::new(Some(&salt), ecdh_secret);
    let mut session_secret = [0u8; 32];
    hk.expand(RECORD_LABEL.as_bytes(), &mut session_secret)
        .expect("hkdf session secret");

    // Per-direction keys are HKDF-Expand of the session secret (no salt), i.e.
    // HKDFExpand(prk=session_secret, info=..., len=32).
    let expand = |info: &[u8]| -> [u8; 32] {
        let hk = Hkdf::<Sha256>::from_prk(&session_secret).expect("prk len");
        let mut out = [0u8; 32];
        hk.expand(info, &mut out).expect("hkdf expand");
        out
    };
    let mobile_tx = expand(b"mobile->device");
    let mobile_rx = expand(b"device->mobile");

    let mut sid_hasher = Sha256::new();
    sid_hasher.update(SESSION_ID_LABEL);
    sid_hasher.update(transcript_hash);
    sid_hasher.update(ecdh_secret);
    let sid_full = sid_hasher.finalize();
    let mut session_id = [0u8; SESSION_ID_BYTES];
    session_id.copy_from_slice(&sid_full[..SESSION_ID_BYTES]);

    (mobile_tx, mobile_rx, session_id)
}

fn record_nonce(direction: u8, counter: u64) -> [u8; 12] {
    let mut iv = [0u8; 12];
    iv[0] = direction;
    iv[4..].copy_from_slice(&counter.to_be_bytes());
    iv
}

fn record_aad(session_id_b64: &str, direction: u8, counter: u64) -> Vec<u8> {
    let arrow = if direction == DIR_TO_DEVICE { "m2d" } else { "d2m" };
    format!("{RECORD_LABEL}|{session_id_b64}|{arrow}|{counter}").into_bytes()
}

/// The fields of the sealed `pairing_encrypted` envelope the app sends/receives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    pub session_id: String,
    pub counter: u64,
    pub ciphertext: Vec<u8>,
    pub tag: Vec<u8>,
}

impl Envelope {
    pub fn to_json(&self) -> Value {
        json!({
            "type": "pairing_encrypted",
            "session_id": self.session_id,
            "counter": self.counter.to_string(),
            "ciphertext": b64url_encode(&self.ciphertext),
            "tag": b64url_encode(&self.tag),
        })
    }

    pub fn from_json(value: &Value) -> Result<Self, String> {
        let session_id = value
            .get("session_id")
            .and_then(Value::as_str)
            .ok_or("missing session_id")?
            .to_string();
        let counter_str = value
            .get("counter")
            .and_then(Value::as_str)
            .ok_or("missing counter")?;
        if !counter_str.bytes().all(|b| b.is_ascii_digit()) || counter_str.is_empty() {
            return Err("invalid counter".into());
        }
        let counter: u64 = counter_str.parse().map_err(|_| "counter overflow")?;
        let ciphertext = b64url_decode(
            value.get("ciphertext").and_then(Value::as_str).ok_or("missing ciphertext")?,
        )?;
        let tag = b64url_decode(value.get("tag").and_then(Value::as_str).ok_or("missing tag")?)?;
        Ok(Envelope { session_id, counter, ciphertext, tag })
    }
}

/// One device's pairing state machine. Not internally synchronized; the setup
/// controller drives it from a single worker thread (as the SDK does).
pub struct PairingSession {
    node_id: String,
    device_id: String,
    mac: String,
    firmware_version: String,
    sdk_token: Option<String>,

    state: PairingState,
    rx_key: Option<[u8; 32]>,
    tx_key: Option<[u8; 32]>,
    session_id_b64: String,
    rx_counter: u64,
    tx_counter: u64,

    // Test seams (mirroring pairing.py's generate_key / random_bytes hooks).
    device_secret_override: Option<SecretKey>,
    device_nonce_override: Option<[u8; NONCE_BYTES]>,
}

impl PairingSession {
    pub fn new(
        node_id: &str,
        device_id: &str,
        mac: &str,
        firmware_version: &str,
        sdk_token: Option<String>,
    ) -> Self {
        PairingSession {
            node_id: node_id.into(),
            device_id: device_id.into(),
            mac: mac.into(),
            firmware_version: if firmware_version.is_empty() {
                "unknown".into()
            } else {
                firmware_version.into()
            },
            sdk_token,
            state: PairingState::Idle,
            rx_key: None,
            tx_key: None,
            session_id_b64: String::new(),
            rx_counter: 0,
            tx_counter: 0,
            device_secret_override: None,
            device_nonce_override: None,
        }
    }

    /// Test hook: force the device's ephemeral P-256 scalar and nonce so a
    /// known-answer vector can be reproduced byte for byte.
    #[cfg(test)]
    pub fn with_test_material(mut self, secret: SecretKey, nonce: [u8; NONCE_BYTES]) -> Self {
        self.device_secret_override = Some(secret);
        self.device_nonce_override = Some(nonce);
        self
    }

    pub fn state(&self) -> PairingState {
        self.state
    }

    pub fn confirmed(&self) -> bool {
        matches!(self.state, PairingState::Ready | PairingState::Provisioning)
    }

    pub fn reset(&mut self) {
        self.state = PairingState::Idle;
        self.rx_key = None;
        self.tx_key = None;
        self.session_id_b64.clear();
        self.rx_counter = 0;
        self.tx_counter = 0;
    }

    /// Public metadata for the plaintext `get_device_info` response.
    pub fn device_info(&self) -> Value {
        json!({
            "device_id": self.device_id,
            "mac": self.mac,
            "model": PAIRING_MODEL,
            "pairing_protocol": PAIRING_VERSION,
            "pairing_auth": AUTH_COMMUNITY,
            "pairing_auth_epoch": 0,
            "pairing_policy": POLICY_APP,
        })
    }

    /// Start a session from `pairing_client_hello`; returns the `pairing_ready`
    /// JSON to send back in plaintext.
    pub fn handle_hello(&mut self, message: &Value) -> Result<Value, PairingError> {
        let version_ok = message
            .get("version")
            .and_then(Value::as_i64)
            .map(|v| v == PAIRING_VERSION)
            .unwrap_or(false);
        if !version_ok
            || message.get("pairing_auth").and_then(Value::as_str) != Some(AUTH_COMMUNITY)
            || message.get("pairing_policy").and_then(Value::as_str) != Some(POLICY_APP)
        {
            return Err(PairingError::new(ERROR_INVALID_HELLO));
        }
        self.reset();

        let mobile_pub_b64 = message.get("mobile_pub").and_then(Value::as_str).unwrap_or("");
        let mobile_nonce_b64 = message.get("mobile_nonce").and_then(Value::as_str).unwrap_or("");
        let mobile_pub = b64url_decode(mobile_pub_b64).map_err(|_| PairingError::new(ERROR_INVALID_HELLO))?;
        let mobile_nonce = b64url_decode(mobile_nonce_b64).map_err(|_| PairingError::new(ERROR_INVALID_HELLO))?;
        if mobile_pub.len() != P256_POINT_BYTES
            || mobile_pub[0] != 0x04
            || mobile_nonce.len() != NONCE_BYTES
        {
            return Err(PairingError::new(ERROR_INVALID_HELLO));
        }
        let peer_point = EncodedPoint::from_bytes(&mobile_pub)
            .map_err(|_| PairingError::new(ERROR_INVALID_HELLO))?;
        let peer = PublicKey::from_sec1_bytes(peer_point.as_bytes())
            .map_err(|_| PairingError::new(ERROR_INVALID_HELLO))?;

        let device_secret = self
            .device_secret_override
            .clone()
            .unwrap_or_else(generate_p256_secret);
        let device_pub_point = device_secret.public_key().to_encoded_point(false);
        let device_pub = device_pub_point.as_bytes().to_vec();
        let device_nonce = self.device_nonce_override.unwrap_or_else(random_nonce);

        let transcript = build_transcript(
            &self.device_id,
            &self.node_id,
            &self.mac,
            &self.firmware_version,
            &b64url_encode(&mobile_pub),
            &b64url_encode(&device_pub),
            &b64url_encode(&mobile_nonce),
            &b64url_encode(&device_nonce),
        );
        let transcript_hash = Sha256::digest(transcript.as_bytes());

        let ecdh = diffie_hellman(device_secret.to_nonzero_scalar(), peer.as_affine());
        let ecdh_secret = ecdh.raw_secret_bytes();
        if ecdh_secret.iter().all(|b| *b == 0) {
            return Err(PairingError::new(ERROR_INVALID_HELLO));
        }

        let (rx_key, tx_key, session_id) =
            derive_session_keys(ecdh_secret, &mobile_nonce, &device_nonce, &transcript_hash);
        self.rx_key = Some(rx_key);
        self.tx_key = Some(tx_key);
        self.session_id_b64 = b64url_encode(&session_id);
        self.rx_counter = 0;
        self.tx_counter = 0;
        self.state = PairingState::WaitClientFinished;

        Ok(json!({
            "type": "pairing_ready",
            "version": PAIRING_VERSION,
            "device_id": self.device_id,
            "node_id": self.node_id,
            "mac": self.mac,
            "model": PAIRING_MODEL,
            "firmware_version": self.firmware_version,
            "pairing_auth": AUTH_COMMUNITY,
            "pairing_auth_epoch": 0,
            "pairing_policy": POLICY_APP,
            "device_pub": b64url_encode(&device_pub),
            "device_nonce": b64url_encode(&device_nonce),
            "transcript_hash": b64url_encode(&transcript_hash),
            "session_id": self.session_id_b64,
        }))
    }

    /// Open one mobile->device record. Any failure clears the session.
    pub fn decrypt(&mut self, envelope: &Envelope) -> Result<String, PairingError> {
        let Some(rx_key) = self.rx_key else {
            self.reset();
            return Err(PairingError::new(ERROR_DECRYPT));
        };
        if self.state == PairingState::Idle {
            self.reset();
            return Err(PairingError::new(ERROR_DECRYPT));
        }
        if envelope.session_id != self.session_id_b64
            || envelope.counter != self.rx_counter
            || envelope.tag.len() != TAG_BYTES
        {
            self.reset();
            return Err(PairingError::new(ERROR_DECRYPT));
        }
        let Ok(cipher) = Aes256Gcm::new_from_slice(&rx_key) else {
            self.reset();
            return Err(PairingError::new(ERROR_DECRYPT));
        };
        let mut combined = envelope.ciphertext.clone();
        combined.extend_from_slice(&envelope.tag);
        let aad = record_aad(&self.session_id_b64, DIR_TO_DEVICE, envelope.counter);
        let plaintext = cipher
            .decrypt(
                Nonce::from_slice(&record_nonce(DIR_TO_DEVICE, envelope.counter)),
                Payload { msg: &combined, aad: &aad },
            )
            .map_err(|_| {
                PairingError::new(ERROR_DECRYPT)
            });
        let plaintext = match plaintext {
            Ok(p) => p,
            Err(e) => {
                self.reset();
                return Err(e);
            }
        };
        let text = match String::from_utf8(plaintext) {
            Ok(t) => t,
            Err(_) => {
                self.reset();
                return Err(PairingError::new(ERROR_DECRYPT));
            }
        };
        self.rx_counter += 1;
        Ok(text)
    }

    /// Confirm the session after the first decrypted record, which must be
    /// exactly `{"action":"pairing_client_finished"}`. Returns true on success.
    pub fn handle_client_finished(&mut self, command: &Value) -> bool {
        let ok = command
            == &json!({ "action": "pairing_client_finished" })
            && self.state == PairingState::WaitClientFinished
            && self.rx_counter == 1;
        if !ok {
            self.reset();
            return false;
        }
        self.state = PairingState::Ready;
        true
    }

    /// Enter provisioning from a confirmed (Ready) session.
    pub fn mark_provisioning(&mut self) -> bool {
        if self.state == PairingState::Ready {
            self.state = PairingState::Provisioning;
            true
        } else {
            self.state == PairingState::Provisioning
        }
    }

    /// Seal a device->mobile record. Returns None when there is no session.
    pub fn encrypt_json(&mut self, plaintext: &str) -> Option<Envelope> {
        let tx_key = self.tx_key?;
        if self.state == PairingState::Idle {
            return None;
        }
        let counter = self.tx_counter;
        let cipher = Aes256Gcm::new_from_slice(&tx_key).ok()?;
        let aad = record_aad(&self.session_id_b64, DIR_FROM_DEVICE, counter);
        let sealed = cipher
            .encrypt(
                Nonce::from_slice(&record_nonce(DIR_FROM_DEVICE, counter)),
                Payload { msg: plaintext.as_bytes(), aad: &aad },
            )
            .ok()?;
        self.tx_counter += 1;
        let (ct, tag) = sealed.split_at(sealed.len() - TAG_BYTES);
        Some(Envelope {
            session_id: self.session_id_b64.clone(),
            counter,
            ciphertext: ct.to_vec(),
            tag: tag.to_vec(),
        })
    }

    /// Seal a `{"type":"status","status":...}` record, adding the SDK token on
    /// `pairing_confirmed` as the firmware does.
    pub fn encrypt_status(&mut self, status: &str) -> Option<Envelope> {
        let mut message = json!({ "type": "status", "status": status });
        if status == "pairing_confirmed" {
            if let Some(tok) = &self.sdk_token {
                message["sdk_token"] = json!(tok);
            }
        }
        let text = serde_json::to_string(&message).ok()?;
        self.encrypt_json(&text)
    }
}

fn random_nonce() -> [u8; NONCE_BYTES] {
    let mut n = [0u8; NONCE_BYTES];
    getrandom::fill(&mut n).expect("getrandom");
    n
}

/// Generate a P-256 secret key from the OS RNG by rejection sampling: draw 32
/// random bytes and retry until they form a valid non-zero scalar. Avoids
/// bridging `getrandom` to `rand_core`, whose versions diverge across crates.
fn generate_p256_secret() -> SecretKey {
    loop {
        let mut raw = [0u8; 32];
        getrandom::fill(&mut raw).expect("getrandom");
        if let Ok(secret) = SecretKey::from_bytes((&raw).into()) {
            return secret;
        }
    }
}

/// Credentials delivered by `provision_v2`. The Wi-Fi fields are intentionally
/// dropped: a desktop that can pair is already online.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Credentials {
    pub access_token: String,
    pub refresh_token: String,
    pub username: String,
    pub api_url_v2: String,
    pub noise_host: String,
}

/// Parse and validate a decrypted `provision_v2` command. Returns the wire
/// status string to answer with on failure (`error_missing_credentials`).
pub fn parse_provision(command: &Value) -> Result<Credentials, String> {
    let text = |key: &str| command.get(key).and_then(Value::as_str).unwrap_or("");
    if command.get("ssid").and_then(Value::as_str).is_none()
        || command.get("password").and_then(Value::as_str).is_none()
        || text("access_token").is_empty()
        || text("refresh_token").is_empty()
        || text("token_type") != "device"
    {
        return Err("error_missing_credentials".into());
    }
    let api_url_v2 = {
        let v = text("api_url_v2");
        if v.starts_with("https://") { v.to_string() } else { String::new() }
    };
    Ok(Credentials {
        access_token: text("access_token").to_string(),
        refresh_token: text("refresh_token").to_string(),
        username: text("username").to_string(),
        api_url_v2,
        noise_host: text("noise_host").to_string(),
    })
}
