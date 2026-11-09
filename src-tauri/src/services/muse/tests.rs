// Copyright (c) code-rs contributors. MIT license (see LICENSE.txt).
//
//! Unit tests for the Muse (de)serialization, framing and crypto state
//! machine. No network is used: the Noise responder in `noise.rs` plays the VM.

use super::link::{self, ControlEvent};
use super::noise::{build_nonce_iv, NoiseXXInitiator, NoiseXXResponder};
use super::proto::{self, Reader};
use super::link::session::ReplyTracker;
use super::transport::{
    self, ApplicationResponse, DecryptedFrame, FrameValue, Header, NoiseFrameDecoder,
    NoiseTransport, NoiseTransportFrame, ServiceFrame, ServiceType,
};
use super::{identity, sdk_token_hint, validate_sdk_token};
use serde_json::{json, Value};

// --- proto ---

#[test]
fn varint_roundtrip() {
    for v in [0u64, 1, 127, 128, 300, 16384, u32::MAX as u64, u64::MAX] {
        let mut out = Vec::new();
        proto::encode_varint(v, &mut out);
        let mut r = Reader::new(&out);
        assert_eq!(r.read_varint().unwrap(), v);
        assert!(r.at_end());
    }
}

#[test]
fn int32_negative_roundtrip() {
    let mut out = Vec::new();
    proto::int32_field(1, -5, &mut out);
    let mut r = Reader::new(&out);
    let (field, _wire) = r.read_key().unwrap();
    assert_eq!(field, 1);
    assert_eq!(proto::decode_int32(r.read_varint().unwrap()).unwrap(), -5);
}

#[test]
fn skip_unknown_field() {
    // field 7 varint, then field 2 string.
    let mut out = Vec::new();
    proto::varint_field(7, 42, &mut out);
    proto::string_field(2, "hi", &mut out);
    let mut r = Reader::new(&out);
    let (f1, w1) = r.read_key().unwrap();
    assert_eq!(f1, 7);
    r.skip(w1).unwrap();
    let (f2, _w2) = r.read_key().unwrap();
    assert_eq!(f2, 2);
    assert_eq!(r.read_string().unwrap(), "hi");
}

// --- noise transport frame ---

#[test]
fn noise_frame_roundtrip() {
    let frame = NoiseTransportFrame {
        chunk_id: -12345,
        chunk_index: 2,
        total_chunks: 5,
        payload: b"hello world".to_vec(),
    };
    let encoded = transport::encode_noise_frame(&frame);
    let decoded = transport::decode_noise_frame(&encoded).unwrap();
    assert_eq!(frame, decoded);
}

#[test]
fn noise_frame_defaults_total_to_one() {
    // Empty encoding (all defaults) must decode total_chunks = 1.
    let decoded = transport::decode_noise_frame(&[]).unwrap();
    assert_eq!(decoded.total_chunks, 1);
    assert_eq!(decoded.chunk_index, 0);
    assert_eq!(decoded.chunk_id, 0);
}

#[test]
fn chunking_and_reassembly() {
    let data = vec![7u8; transport::MAX_CHUNK_PAYLOAD * 2 + 100];
    let frames = transport::encode_noise_frames(&data, 99).unwrap();
    assert_eq!(frames.len(), 3);
    let mut decoder = NoiseFrameDecoder::new();
    let mut result = None;
    for f in &frames {
        let frame = transport::decode_noise_frame(f).unwrap();
        if let Some(msg) = decoder.decode(frame).unwrap() {
            result = Some(msg);
        }
    }
    assert_eq!(result.unwrap(), data);
}

#[test]
fn reassembly_rejects_duplicate_index() {
    let mut decoder = NoiseFrameDecoder::new();
    let f = NoiseTransportFrame {
        chunk_id: 1,
        chunk_index: 0,
        total_chunks: 2,
        payload: vec![1],
    };
    assert!(decoder.decode(f.clone()).unwrap().is_none());
    assert!(decoder.decode(f).is_err());
}

// --- noise nonce ---

#[test]
fn nonce_iv_is_big_endian() {
    assert_eq!(build_nonce_iv(0), [0u8; 12]);
    assert_eq!(
        build_nonce_iv(1),
        [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
        "nonce counter must be big-endian in the last 8 bytes"
    );
    assert_eq!(
        build_nonce_iv(0x0102030405060708),
        [0, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8]
    );
}

// --- noise XX handshake + transport end to end ---

#[test]
fn noise_xx_handshake_and_transport_roundtrip() {
    let mut init = NoiseXXInitiator::new();
    let mut resp = NoiseXXResponder::new();

    let m1 = init.write_message1().unwrap();
    let m2 = resp.read_message1_and_write_message2(&m1).unwrap();
    init.read_message2(&m2).unwrap();
    let m3 = init.write_message3().unwrap();
    resp.read_message3(&m3).unwrap();

    let (isend, irecv) = init.split().unwrap();
    let (rsend, rrecv) = resp.split().unwrap();

    // Initiator -> responder transport frame.
    let mut itx = NoiseTransport::new(isend, irecv);
    // Encrypt a request and have the responder cipher states decrypt the raw
    // Noise records to confirm the keys match (initiator.send == responder.recv).
    let enc = itx
        .encrypt_http_request("POST", "/link-control", b"", vec![])
        .unwrap();
    assert!(enc.stream_id >= 1);
    // The responder's recv state (rrecv) should decrypt the initiator's first
    // Noise record to a valid NoiseTransportFrame.
    let mut rrecv = rrecv;
    let plain = rrecv.decrypt_with_ad(b"", &enc.frames[0]).unwrap();
    let frame = transport::decode_noise_frame(&plain).unwrap();
    assert!(!frame.payload.is_empty());

    // Responder builds an ApplicationResponse and sends it back; the initiator
    // transport must decrypt and decode it.
    let resp_frame = ServiceFrame {
        stream_id: enc.stream_id,
        value: FrameValue::Response(ApplicationResponse {
            status: 200,
            headers: vec![Header::new("content-type", "application/json")],
            body: b"{}".to_vec(),
            end_body: true,
        }),
    };
    let frame_bytes = transport::encode_service_frame(&resp_frame);
    // ServiceResponse wraps the frame bytes in field #1.
    let mut service_response = Vec::new();
    proto::bytes_field(1, &frame_bytes, &mut service_response);
    let noise_frames = transport::encode_noise_frames(&service_response, 7).unwrap();
    let mut rsend = rsend;
    let ciphertext = rsend.encrypt_with_ad(b"", &noise_frames[0]).unwrap();

    let decrypted = itx.decrypt_frame(&ciphertext).unwrap().unwrap();
    match decrypted {
        DecryptedFrame::Response { stream_id, value } => {
            assert_eq!(stream_id, enc.stream_id);
            assert_eq!(value.status, 200);
            assert_eq!(value.body, b"{}");
            assert!(value.end_body);
            assert_eq!(value.headers[0].key, "content-type");
        }
        other => panic!("expected response, got {other:?}"),
    }
}

#[test]
fn service_request_omits_daemon_service() {
    // DAEMON (0) must be omitted so only field #2 (payload) is present.
    let bytes = transport::encode_service_request(ServiceType::Daemon, b"abc");
    let mut r = Reader::new(&bytes);
    let (field, _w) = r.read_key().unwrap();
    assert_eq!(field, 2, "daemon service type should be omitted");
    assert_eq!(r.read_delimited().unwrap(), b"abc");
}

// --- link message framing ---

#[test]
fn length_prefixed_message_roundtrip() {
    let msg = json!({ "method": "link.result", "id": "abc", "ok": true });
    let encoded = link::encode_message(&msg);
    // u32 LE length prefix.
    let len = u32::from_le_bytes([encoded[0], encoded[1], encoded[2], encoded[3]]) as usize;
    assert_eq!(len, encoded.len() - 4);

    let mut decoder = link::MessageDecoder::new();
    // Feed in two halves to exercise buffering across chunks.
    let (a, b) = encoded.split_at(encoded.len() / 2);
    assert!(decoder.feed(a).unwrap().is_empty());
    let out = decoder.feed(b).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0], msg);
}

#[test]
fn decoder_skips_keepalive_empty_messages() {
    let mut decoder = link::MessageDecoder::new();
    // Zero-length message = keepalive, then a real message.
    let mut buf = vec![0u8, 0, 0, 0];
    buf.extend_from_slice(&link::encode_message(&json!({ "x": 1 })));
    let out = decoder.feed(&buf).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0], json!({ "x": 1 }));
}

// --- classify ---

#[test]
fn classify_register_ack_and_reject() {
    let ok = json!({ "id": "rid", "ok": true });
    assert_eq!(link::classify(&ok, "rid"), ControlEvent::Registered);

    let rejected = json!({ "id": "rid", "error": "nope" });
    assert_eq!(link::classify(&rejected, "rid"), ControlEvent::RegisterRejected);

    let ok_false = json!({ "id": "rid", "ok": false });
    assert_eq!(link::classify(&ok_false, "rid"), ControlEvent::RegisterRejected);
}

#[test]
fn classify_unpaired() {
    assert_eq!(
        link::classify(&json!({ "event": "link.unpaired" }), "rid"),
        ControlEvent::Unpaired
    );
    assert_eq!(
        link::classify(&json!({ "event": "node.unpaired" }), "rid"),
        ControlEvent::Unpaired
    );
}

#[test]
fn classify_invoke() {
    let msg = json!({
        "method": "link.invoke",
        "id": "call-1",
        "command": "do_thing",
        "params": { "a": 1 },
        "timeout_ms": 5000,
    });
    match link::classify(&msg, "rid") {
        ControlEvent::Invoke(inv) => {
            assert_eq!(inv.call_id, "call-1");
            assert_eq!(inv.command, "do_thing");
            assert_eq!(inv.params, json!({ "a": 1 }));
            assert_eq!(inv.timeout_ms, Some(5000));
        }
        other => panic!("expected invoke, got {other:?}"),
    }
}

#[test]
fn result_message_shapes() {
    let ok = link::result_message("c1", true, &json!({ "exit_code": 0 }));
    assert_eq!(ok["method"], "link.result");
    assert_eq!(ok["id"], "c1");
    assert_eq!(ok["ok"], true);
    assert_eq!(ok["payload"], json!({ "exit_code": 0 }));

    let err = link::result_message("c2", false, &Value::String("boom".into()));
    assert_eq!(err["ok"], false);
    assert_eq!(err["error"], "boom");
}

// --- device description / commands_v2 ---

#[test]
fn commands_v2_maps_schema_to_required_optional() {
    let dd = link::DeviceDescription {
        node_id: "homelink-abcdef".into(),
        display_name: "code-rs".into(),
        version: "1.2.3".into(),
        commands: vec![link::MuseCommand {
            name: "search".into(),
            description: "Search the workspace".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "the query" },
                    "limit": { "type": "number", "description": "max results" }
                },
                "required": ["query"]
            }),
        }],
    };
    let params = dd.register_params();
    assert_eq!(params["platform"], "linux");
    assert_eq!(params["device_family"], "homehub");
    let spec = &params["commands_v2"]["search"];
    assert_eq!(spec["description"], "Search the workspace");
    assert!(spec["required"].get("query").is_some());
    assert!(spec["optional"].get("limit").is_some());
    assert!(spec["required"].get("limit").is_none());
}

#[test]
fn register_message_shape() {
    let dd = link::DeviceDescription {
        node_id: "homelink-abcdef".into(),
        display_name: "code-rs".into(),
        version: "1.2.3".into(),
        commands: vec![],
    };
    let msg = dd.register_message("rid-1");
    assert_eq!(msg["type"], "req");
    assert_eq!(msg["id"], "rid-1");
    assert_eq!(msg["method"], "link.register");
    assert_eq!(msg["params"]["node_id"], "homelink-abcdef");
}

// --- chat ---

#[test]
fn chat_body_fields() {
    let body = link::chat_body("hi", "homelink-abcdef", Some("side-1"));
    assert_eq!(body["message"], "hi");
    assert_eq!(body["output_modality"], "text");
    assert_eq!(body["device_id"], "homelink-abcdef");
    assert_eq!(body["session_id"], "side-1");

    let main = link::chat_body("hi", "homelink-abcdef", None);
    assert!(main.get("session_id").is_none());
}

#[test]
fn acknowledged_ids_reads_result_or_top_level() {
    let wrapped = json!({ "result": { "message_id": "m1", "session_id": "s1" } });
    let (ids, sid) = link::acknowledged_ids(&wrapped).unwrap();
    assert_eq!(ids, vec!["m1".to_string()]);
    assert_eq!(sid.as_deref(), Some("s1"));

    let top = json!({ "message_id": "m2" });
    let (ids, sid) = link::acknowledged_ids(&top).unwrap();
    assert_eq!(ids, vec!["m2".to_string()]);
    assert!(sid.is_none());

    assert!(link::acknowledged_ids(&json!({ "nope": 1 })).is_err());
}

#[test]
fn strip_controls_keeps_tab_newline() {
    assert_eq!(link::strip_controls("a\tb\nc"), "a\tb\nc");
    assert_eq!(link::strip_controls("a\x07b\x1bc"), "abc");
}

// --- reply tracker (muse_turn) ---

#[test]
fn reply_tracker_streams_then_finalizes() {
    let mut t = ReplyTracker::new(Some("s1".into()));
    t.acknowledge(vec!["u1".into()], Some("s1".into()), false);
    // our own user message event flips after_note on.
    t.take(&json!({ "type": "event", "event": "message.user",
        "payload": { "session_id": "s1", "message_id": "u1" } }));
    // assistant streams deltas with parent = our user message.
    t.take(&json!({ "type": "event", "event": "delta.text_append",
        "payload": { "session_id": "s1", "message_id": "a1", "reply_to_message_id": "u1", "text": "Hel" } }));
    t.take(&json!({ "type": "event", "event": "delta.text_append",
        "payload": { "session_id": "s1", "message_id": "a1", "reply_to_message_id": "u1", "text": "lo" } }));
    assert_eq!(t.reply_text(), "Hello");
    assert!(!t.complete());
    // done with a final display_text and a terminal task status.
    t.take(&json!({ "type": "event", "event": "delta.message_done",
        "payload": { "session_id": "s1", "message_id": "a1", "reply_to_message_id": "u1", "display_text": "Hello!" } }));
    t.take(&json!({ "type": "event", "event": "task.status",
        "payload": { "session_id": "s1", "status": "completed", "reply_to_message_id": "u1" } }));
    assert_eq!(t.reply_text(), "Hello!");
    assert!(t.has_text());
    assert!(t.complete());
    assert!(t.settled());
}

#[test]
fn reply_tracker_ignores_other_chats() {
    let mut t = ReplyTracker::new(Some("s1".into()));
    t.acknowledge(vec!["u1".into()], Some("s1".into()), false);
    t.take(&json!({ "type": "event", "event": "delta.text_append",
        "payload": { "session_id": "other", "message_id": "x", "reply_to_message_id": "u1", "text": "nope" } }));
    assert_eq!(t.reply_text(), "");
}

#[test]
fn reply_tracker_dedupes_by_seq() {
    let mut t = ReplyTracker::new(Some("s1".into()));
    t.acknowledge(vec!["u1".into()], Some("s1".into()), true);
    let ev = json!({ "type": "event", "event": "delta.text_append", "seq": 5,
        "payload": { "session_id": "s1", "message_id": "a1", "reply_to_message_id": "u1", "text": "x" } });
    t.take(&ev);
    t.take(&ev); // same seq ignored
    assert_eq!(t.reply_text(), "x");
}

// --- sdk token + identity ---

#[test]
fn sdk_token_validation() {
    // 43 chars after mgst_: 42 base64url + 1 from the restricted final set.
    let good = format!("mgst_{}A", "a".repeat(42));
    assert!(validate_sdk_token(&good).is_ok());

    // wrong final char (not in the 4-bit set).
    let bad_final = format!("mgst_{}B", "a".repeat(42));
    assert!(validate_sdk_token(&bad_final).is_err());

    assert!(validate_sdk_token("nope").is_err());
    assert!(validate_sdk_token(&format!("mgst_{}A", "a".repeat(41))).is_err());
}

#[test]
fn sdk_token_hint_shows_prefix() {
    let tok = format!("mgst_{}A", "a".repeat(42));
    assert_eq!(sdk_token_hint(Some(&tok)), "mgst_aaaaaaa");
    assert_eq!(sdk_token_hint(None), "none");
}

#[test]
fn identity_derivation() {
    let mac = "02:ab:cd:ef:12:34";
    assert!(identity::is_valid(mac));
    assert_eq!(identity::suffix(mac), "ef1234");
    assert_eq!(identity::node_id(mac), "homelink-ef1234");
    assert_eq!(identity::device_id(mac), "hatch-link:02:ab:cd:ef:12:34");

    let generated = identity::generate_mac();
    assert!(identity::is_valid(&generated), "generated mac {generated} invalid");
    // Locally administered + unicast: first octet has 0x02 set, 0x01 clear.
    let first = u8::from_str_radix(&generated[..2], 16).unwrap();
    assert_eq!(first & 0x03, 0x02);
}
