// Copyright (c) code-rs contributors. MIT license (see LICENSE.txt).
//
//! Offline tests for the BLE pairing layer: chunk framing, the v5 community
//! handshake against the SDK's published known-answer vector
//! (`vectors/link_pairing_v5.json`, `community_app_v5`), and the full
//! `provision_v2` state machine driven through a fake transport.

use serde_json::{json, Value};

use super::framing::{encode_chunks, ChunkAssembler, CHUNK_MAGIC};
use super::pairing::{
    b64url_decode, b64url_encode, build_transcript, derive_session_keys, parse_provision,
    Credentials, Envelope, PairingSession, PairingState,
};
use super::setup::{SetupController, Transport};
use super::{ble_name, SERVICE_UUID};

// --- framing ---

#[test]
fn chunks_roundtrip_small_and_large() {
    // Small message at a tiny MTU splits into several 0xFE packets.
    let data = b"hello muse, this is a reasonably long setup command payload";
    let packets = encode_chunks(data, 23).unwrap();
    assert!(packets.len() > 1);
    for p in &packets {
        assert_eq!(p[0], CHUNK_MAGIC);
        assert!(p.len() <= 23 - 3);
    }
    let mut asm = ChunkAssembler::new();
    let mut out = None;
    for p in &packets {
        if let Some(msg) = asm.feed(p) {
            out = Some(msg);
        }
    }
    assert_eq!(out.unwrap(), data);
}

#[test]
fn unchunked_packet_is_complete() {
    let mut asm = ChunkAssembler::new();
    // A packet not starting with 0xFE is a whole message.
    assert_eq!(asm.feed(b"idle").unwrap(), b"idle");
}

#[test]
fn assembler_rejects_out_of_order() {
    let mut asm = ChunkAssembler::new();
    // index 1 before index 0 -> reset, nothing returned.
    assert!(asm.feed(&[CHUNK_MAGIC, 1, 2, b'x']).is_none());
}

#[test]
fn ble_name_matches_firmware_format() {
    assert_eq!(ble_name("02:ab:cd:ef:12:34"), "MuseGadget-EF1234");
    assert_eq!(ble_name("02:00:00:00:00:01"), "MuseGadget-000001");
    assert_eq!(SERVICE_UUID.len(), 36);
}

// --- base64url ---

#[test]
fn b64url_roundtrip_unpadded() {
    let data = &[0u8, 1, 2, 250, 251, 255];
    let enc = b64url_encode(data);
    assert!(!enc.contains('='));
    assert_eq!(b64url_decode(&enc).unwrap(), data);
    assert!(b64url_decode("has padding=").is_err());
}

// --- v5 handshake: known-answer vector (community_app_v5) ---

// Public synthetic values from the SDK test vector. Device scalar = 2.
const MOBILE_PUB: &str =
    "BGsX0fLhLEJH-Lzm5WOkQPJ3A32BLeszoPShOUXYmMKWT-NC4v4af5uO5-tKfA-eFivOM1drMV7Oy7ZAaDe_UfU";
const MOBILE_NONCE: &str = "AAECAwQFBgcICQoLDA0ODw";
const DEVICE_NONCE: &str = "EBESExQVFhcYGRobHB0eHw";
const DEVICE_PUB: &str =
    "BHzyexiNA09-ilI4AwS1GsPAiWnid_IbNaYLSPxHZpl4B3dVENuO0EApPZrGn3Qw27p9reY86YIpngS3nSJ4c9E";
const ECDH_SECRET_HEX: &str = "7cf27b188d034f7e8a52380304b51ac3c08969e277f21b35a60b48fc47669978";
const TRANSCRIPT_HASH: &str = "18Rs196tzddPGuj18pZ8rWObWYpwoDUIwocjnUASj_0";
const SESSION_ID: &str = "z0eNLGw5mczvD4a1F2ubSQ";
const MOBILE_TX_KEY_HEX: &str = "ee25e7f1eb3c05cc8465634e5f634deed858862b1c4c35fa434dc926f7facaeb";
const MOBILE_RX_KEY_HEX: &str = "9abb2954b05e2f5f7112e9bfd41fd970602e27ed07453c1cff51f575de5d0b4e";
const CLIENT_FINISHED_CT: &str = "uwNX3KEkix5T6nWce3s2SKPPh-5E31x-UIHFSK6MpMUg562B";
const CLIENT_FINISHED_TAG: &str = "zT4kU6XK-rM7rvt3VEITLg";

fn hex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

fn device_scalar_two() -> p256::SecretKey {
    let mut scalar = [0u8; 32];
    scalar[31] = 2;
    p256::SecretKey::from_bytes((&scalar).into()).unwrap()
}

fn device_nonce_bytes() -> [u8; 16] {
    let v = b64url_decode(DEVICE_NONCE).unwrap();
    let mut out = [0u8; 16];
    out.copy_from_slice(&v);
    out
}

#[test]
fn transcript_and_keys_match_vector() {
    let transcript = build_transcript(
        "hatch-link:02:00:00:00:00:01",
        "homelink-000001",
        "02:00:00:00:00:01",
        "1.0.0",
        MOBILE_PUB,
        DEVICE_PUB,
        MOBILE_NONCE,
        DEVICE_NONCE,
    );
    use sha2::{Digest, Sha256};
    let th = Sha256::digest(transcript.as_bytes());
    assert_eq!(b64url_encode(&th), TRANSCRIPT_HASH);

    let (rx_key, tx_key, session_id) = derive_session_keys(
        &hex(ECDH_SECRET_HEX),
        &b64url_decode(MOBILE_NONCE).unwrap(),
        &b64url_decode(DEVICE_NONCE).unwrap(),
        &b64url_decode(TRANSCRIPT_HASH).unwrap(),
    );
    // rx_key decrypts app->device (mobile_tx), tx_key encrypts device->app
    // (mobile_rx), per derive_session_keys' contract.
    assert_eq!(rx_key.to_vec(), hex(MOBILE_TX_KEY_HEX));
    assert_eq!(tx_key.to_vec(), hex(MOBILE_RX_KEY_HEX));
    assert_eq!(b64url_encode(&session_id), SESSION_ID);
}

#[test]
fn handshake_produces_vector_session() {
    let mut session = PairingSession::new(
        "homelink-000001",
        "hatch-link:02:00:00:00:00:01",
        "02:00:00:00:00:01",
        "1.0.0",
        Some("mgst_demo".into()),
    )
    .with_test_material(device_scalar_two(), device_nonce_bytes());

    let hello = json!({
        "action": "pairing_client_hello",
        "version": 5,
        "pairing_auth": "none",
        "pairing_policy": "confirm_app",
        "mobile_pub": MOBILE_PUB,
        "mobile_nonce": MOBILE_NONCE,
    });
    let ready = session.handle_hello(&hello).unwrap();
    assert_eq!(ready["device_pub"], DEVICE_PUB);
    assert_eq!(ready["device_nonce"], DEVICE_NONCE);
    assert_eq!(ready["transcript_hash"], TRANSCRIPT_HASH);
    assert_eq!(ready["session_id"], SESSION_ID);
    assert_eq!(session.state(), PairingState::WaitClientFinished);

    // The app's encrypted pairing_client_finished (counter 0) decrypts to the
    // fixed plaintext and confirms the session.
    let envelope = Envelope {
        session_id: SESSION_ID.into(),
        counter: 0,
        ciphertext: b64url_decode(CLIENT_FINISHED_CT).unwrap(),
        tag: b64url_decode(CLIENT_FINISHED_TAG).unwrap(),
    };
    let plaintext = session.decrypt(&envelope).unwrap();
    assert_eq!(plaintext, "{\"action\":\"pairing_client_finished\"}");
    let command: Value = serde_json::from_str(&plaintext).unwrap();
    assert!(session.handle_client_finished(&command));
    assert!(session.confirmed());

    // A device->app status now encrypts; the app (holding mobile_rx) can open
    // it. We verify it against the same key the vector pins.
    let status = session.encrypt_status("pairing_confirmed").unwrap();
    assert_eq!(status.counter, 0);
    let opened = open_as_mobile(&status, &hex(MOBILE_RX_KEY_HEX), SESSION_ID, 1);
    let parsed: Value = serde_json::from_slice(&opened).unwrap();
    assert_eq!(parsed["type"], "status");
    assert_eq!(parsed["status"], "pairing_confirmed");
    assert_eq!(parsed["sdk_token"], "mgst_demo");
}

/// Open a device->mobile record the way the phone would (direction d2m = 1).
fn open_as_mobile(env: &Envelope, mobile_rx_key: &[u8], session_id: &str, _dummy: u8) -> Vec<u8> {
    use aes_gcm::aead::{Aead, Payload};
    use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
    let cipher = Aes256Gcm::new_from_slice(mobile_rx_key).unwrap();
    let mut iv = [0u8; 12];
    iv[0] = 1; // from device
    iv[4..].copy_from_slice(&env.counter.to_be_bytes());
    let aad = format!("hatch-link ble setup v1|{session_id}|d2m|{}", env.counter);
    let mut combined = env.ciphertext.clone();
    combined.extend_from_slice(&env.tag);
    cipher
        .decrypt(Nonce::from_slice(&iv), Payload { msg: &combined, aad: aad.as_bytes() })
        .unwrap()
}

#[test]
fn rejects_bad_hello() {
    let mut session = PairingSession::new("homelink-x", "hatch-link:x", "02:00:00:00:00:01", "1.0.0", None);
    // Wrong policy.
    let bad = json!({
        "action": "pairing_client_hello", "version": 5,
        "pairing_auth": "none", "pairing_policy": "confirm_press",
        "mobile_pub": MOBILE_PUB, "mobile_nonce": MOBILE_NONCE,
    });
    assert!(session.handle_hello(&bad).is_err());
}

// --- provision_v2 parsing ---

#[test]
fn parse_provision_validates_required_fields() {
    let good = json!({
        "action": "provision_v2", "ssid": "x", "password": "",
        "access_token": "a", "refresh_token": "r", "token_type": "device",
        "username": "u", "api_url_v2": "https://api.example", "noise_host": "h",
    });
    let creds = parse_provision(&good).unwrap();
    assert_eq!(creds.access_token, "a");
    assert_eq!(creds.api_url_v2, "https://api.example");
    assert_eq!(creds.noise_host, "h");

    // http (not https) api_url_v2 is dropped.
    let mut http = good.clone();
    http["api_url_v2"] = json!("http://insecure");
    assert_eq!(parse_provision(&http).unwrap().api_url_v2, "");

    // Missing token_type=device is rejected.
    let mut bad = good.clone();
    bad["token_type"] = json!("user");
    assert_eq!(parse_provision(&bad).unwrap_err(), "error_missing_credentials");
}

// --- full flow through the SetupController with a fake transport ---

/// Reassembles device->app notifications and records decoded messages, playing
/// the app's record layer so we can assert the full exchange.
struct FakeTransport {
    inner: std::sync::Mutex<FakeInner>,
}

struct FakeInner {
    assembler: ChunkAssembler,
    plaintext: Vec<String>,
    encrypted: Vec<Value>,
    disconnects: Vec<f64>,
}

impl FakeTransport {
    fn new() -> Self {
        FakeTransport {
            inner: std::sync::Mutex::new(FakeInner {
                assembler: ChunkAssembler::new(),
                plaintext: Vec::new(),
                encrypted: Vec::new(),
                disconnects: Vec::new(),
            }),
        }
    }
}

impl Transport for FakeTransport {
    fn send_packets(&self, packets: &[Vec<u8>]) {
        let mut inner = self.inner.lock().unwrap();
        for packet in packets {
            if let Some(message) = inner.assembler.feed(packet) {
                if packet[0] == CHUNK_MAGIC {
                    // Framed JSON.
                    if let Ok(v) = serde_json::from_slice::<Value>(&message) {
                        if v.get("type").and_then(Value::as_str) == Some("pairing_encrypted") {
                            inner.encrypted.push(v);
                        } else {
                            inner.plaintext.push(String::from_utf8_lossy(&message).to_string());
                        }
                    }
                } else {
                    inner.plaintext.push(String::from_utf8_lossy(&message).to_string());
                }
            }
        }
    }
    fn mtu(&self) -> usize {
        185
    }
    fn disconnect(&self, delay: f64) {
        self.inner.lock().unwrap().disconnects.push(delay);
    }
}

/// Minimal mobile side of the record layer, keyed from the vector.
struct Mobile {
    tx_key: Vec<u8>, // mobile->device (seal)
    rx_key: Vec<u8>, // device->mobile (open)
    session_id: String,
    tx_counter: u64,
}

impl Mobile {
    fn seal(&mut self, command: &Value) -> Value {
        use aes_gcm::aead::{Aead, Payload};
        use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
        let counter = self.tx_counter;
        self.tx_counter += 1;
        let cipher = Aes256Gcm::new_from_slice(&self.tx_key).unwrap();
        let mut iv = [0u8; 12];
        iv[4..].copy_from_slice(&counter.to_be_bytes());
        let aad = format!("hatch-link ble setup v1|{}|m2d|{counter}", self.session_id);
        let sealed = cipher
            .encrypt(
                Nonce::from_slice(&iv),
                Payload { msg: serde_json::to_vec(command).unwrap().as_slice(), aad: aad.as_bytes() },
            )
            .unwrap();
        let (ct, tag) = sealed.split_at(sealed.len() - 16);
        json!({
            "action": "pairing_encrypted",
            "session_id": self.session_id,
            "counter": counter.to_string(),
            "ciphertext": b64url_encode(ct),
            "tag": b64url_encode(tag),
        })
    }

    fn open(&self, env: &Value) -> Value {
        let env = Envelope::from_json(env).unwrap();
        let data = open_as_mobile(&env, &self.rx_key, &self.session_id, 0);
        serde_json::from_slice(&data).unwrap()
    }
}

fn write_json(controller: &mut SetupController, transport: &FakeTransport, value: &Value) {
    let _ = transport;
    for packet in encode_chunks(value.to_string().as_bytes(), 185).unwrap() {
        controller.on_write(&packet);
    }
}

#[test]
fn full_provision_flow_with_fake_transport() {
    let transport = FakeTransport::new();
    let committed: std::sync::Arc<std::sync::Mutex<Option<Credentials>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    let slot = committed.clone();
    let commit = Box::new(move |creds: &Credentials| -> Result<(), String> {
        *slot.lock().unwrap() = Some(creds.clone());
        Ok(())
    });

    let pairing = PairingSession::new(
        "homelink-000001",
        "hatch-link:02:00:00:00:00:01",
        "02:00:00:00:00:01",
        "1.0.0",
        Some("mgst_demo".into()),
    )
    .with_test_material(device_scalar_two(), device_nonce_bytes());

    let mut controller =
        SetupController::new(pairing, "homelink-000001", "1.0.0", true, &transport, commit);

    // 1) get_device_info in plaintext.
    write_json(&mut controller, &transport, &json!({ "action": "get_device_info" }));
    {
        let inner = transport.inner.lock().unwrap();
        let info: Value = serde_json::from_str(inner.plaintext.last().unwrap()).unwrap();
        assert_eq!(info["type"], "device_info");
        assert_eq!(info["node_id"], "homelink-000001");
        assert_eq!(info["pairing_policy"], "confirm_app");
    }

    // 2) pairing_client_hello -> pairing_ready (plaintext).
    write_json(
        &mut controller,
        &transport,
        &json!({
            "action": "pairing_client_hello", "version": 5,
            "pairing_auth": "none", "pairing_policy": "confirm_app",
            "mobile_pub": MOBILE_PUB, "mobile_nonce": MOBILE_NONCE,
        }),
    );
    let (ready, mut mobile) = {
        let inner = transport.inner.lock().unwrap();
        let ready: Value = serde_json::from_str(inner.plaintext.last().unwrap()).unwrap();
        let mobile = Mobile {
            tx_key: hex(MOBILE_TX_KEY_HEX),
            rx_key: hex(MOBILE_RX_KEY_HEX),
            session_id: ready["session_id"].as_str().unwrap().to_string(),
            tx_counter: 0,
        };
        (ready, mobile)
    };
    assert_eq!(ready["session_id"], SESSION_ID);

    // 3) encrypted pairing_client_finished -> pairing_confirmed status.
    let finished = mobile.seal(&json!({ "action": "pairing_client_finished" }));
    write_json(&mut controller, &transport, &finished);
    {
        let inner = transport.inner.lock().unwrap();
        let confirmed = mobile.open(inner.encrypted.last().unwrap());
        assert_eq!(confirmed["status"], "pairing_confirmed");
    }

    // 4) encrypted provision_v2 -> wifi_connecting, wifi_connected, auth_ok.
    let provision = mobile.seal(&json!({
        "action": "provision_v2", "ssid": "x", "password": "",
        "access_token": "device-access", "refresh_token": "device-refresh",
        "token_type": "device", "username": "someone",
        "api_url_v2": "https://api.example", "noise_host": "hatch.example",
    }));
    write_json(&mut controller, &transport, &provision);

    assert!(controller.completed());
    let creds = committed.lock().unwrap().clone().unwrap();
    assert_eq!(creds.access_token, "device-access");
    assert_eq!(creds.refresh_token, "device-refresh");
    assert_eq!(creds.noise_host, "hatch.example");
    assert_eq!(creds.api_url_v2, "https://api.example");

    // The final encrypted statuses include auth_ok.
    let statuses: Vec<String> = {
        let inner = transport.inner.lock().unwrap();
        inner
            .encrypted
            .iter()
            .map(|e| mobile.open(e)["status"].as_str().unwrap_or("").to_string())
            .collect()
    };
    assert!(statuses.contains(&"auth_ok".to_string()), "statuses: {statuses:?}");
}

#[test]
fn sensitive_action_in_plaintext_is_refused() {
    let transport = FakeTransport::new();
    let commit = Box::new(|_: &Credentials| Ok(()));
    let pairing = PairingSession::new("homelink-x", "hatch-link:x", "02:00:00:00:00:01", "1.0.0", None);
    let mut controller =
        SetupController::new(pairing, "homelink-x", "1.0.0", true, &transport, commit);
    // provision_v2 in the clear, before any handshake.
    write_json(&mut controller, &transport, &json!({ "action": "provision_v2" }));
    let inner = transport.inner.lock().unwrap();
    assert!(inner.plaintext.iter().any(|s| s == "error_encryption_required"));
}
