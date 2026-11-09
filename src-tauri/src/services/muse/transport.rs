// Copyright (c) code-rs contributors. MIT license (see LICENSE.txt).
//
//! Noise transport framing and service envelope, mirroring the SDK's
//! `noise/framing.py`, `noise/envelope.py` and `noise/transport.py`.

use super::noise::CipherState;
use super::proto::{self, Reader};

pub const MAX_CHUNK_PAYLOAD: usize = 65489;
pub const MAX_TOTAL_CHUNKS: u32 = 256;
pub const MAX_ASSEMBLY_BYTES: usize = 16 * 1024 * 1024;

// ---------------------------------------------------------------------------
// NoiseTransportFrame (framing.py)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NoiseTransportFrame {
    pub chunk_id: i64,
    pub chunk_index: u32,
    pub total_chunks: u32,
    pub payload: Vec<u8>,
}

pub fn encode_noise_frame(frame: &NoiseTransportFrame) -> Vec<u8> {
    let mut out = Vec::new();
    if frame.chunk_id != 0 {
        proto::int64_field(1, frame.chunk_id, &mut out);
    }
    if frame.chunk_index != 0 {
        proto::uint32_field(2, frame.chunk_index, &mut out);
    }
    if frame.total_chunks != 0 {
        proto::uint32_field(3, frame.total_chunks, &mut out);
    }
    if !frame.payload.is_empty() {
        proto::bytes_field(4, &frame.payload, &mut out);
    }
    out
}

pub fn decode_noise_frame(data: &[u8]) -> Result<NoiseTransportFrame, proto::ProtoError> {
    let mut frame = NoiseTransportFrame {
        total_chunks: 1,
        ..Default::default()
    };
    let mut r = Reader::new(data);
    while !r.at_end() {
        let (field, wire) = r.read_key()?;
        match field {
            1 => frame.chunk_id = proto::decode_int64(r.read_varint()?),
            2 => frame.chunk_index = proto::decode_uint32(r.read_varint()?)?,
            3 => frame.total_chunks = proto::decode_uint32(r.read_varint()?)?,
            4 => frame.payload = r.read_delimited()?.to_vec(),
            _ => r.skip(wire)?,
        }
    }
    Ok(frame)
}

/// Split `data` into serialized `NoiseTransportFrame`s with a shared chunk id.
pub fn encode_noise_frames(data: &[u8], chunk_id: i64) -> Result<Vec<Vec<u8>>, proto::ProtoError> {
    let total = (data.len().div_ceil(MAX_CHUNK_PAYLOAD)).max(1);
    if total as u32 > MAX_TOTAL_CHUNKS {
        return Err(proto::ProtoError("payload too large for noise framing".into()));
    }
    if data.is_empty() {
        return Ok(vec![encode_noise_frame(&NoiseTransportFrame {
            chunk_id,
            chunk_index: 0,
            total_chunks: 1,
            payload: Vec::new(),
        })]);
    }
    let mut frames = Vec::with_capacity(total);
    for i in 0..total {
        let start = i * MAX_CHUNK_PAYLOAD;
        let end = (start + MAX_CHUNK_PAYLOAD).min(data.len());
        frames.push(encode_noise_frame(&NoiseTransportFrame {
            chunk_id,
            chunk_index: i as u32,
            total_chunks: total as u32,
            payload: data[start..end].to_vec(),
        }));
    }
    Ok(frames)
}

fn random_i64() -> i64 {
    let mut buf = [0u8; 8];
    getrandom::fill(&mut buf).expect("getrandom");
    i64::from_le_bytes(buf)
}

// ---------------------------------------------------------------------------
// Service envelope (envelope.py)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceType {
    Daemon = 0,
    Sentinel = 1,
    Vault = 2,
    Authd = 3,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    pub key: String,
    pub value: String,
}

impl Header {
    pub fn new(key: &str, value: &str) -> Self {
        Header { key: key.into(), value: value.into() }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ApplicationRequest {
    pub verb: String,
    pub path: String,
    pub headers: Vec<Header>,
    pub body: Vec<u8>,
    pub end_body: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ApplicationResponse {
    pub status: i32,
    pub headers: Vec<Header>,
    pub body: Vec<u8>,
    pub end_body: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BodyChunk {
    pub data: Vec<u8>,
    pub end_body: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reset {
    pub code: i32,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrameValue {
    Request(ApplicationRequest),
    Response(ApplicationResponse),
    BodyChunk(BodyChunk),
    Reset(Reset),
    Empty,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceFrame {
    pub stream_id: i64,
    pub value: FrameValue,
}

fn encode_header(h: &Header) -> Vec<u8> {
    let mut out = Vec::new();
    if !h.key.is_empty() {
        proto::string_field(1, &h.key, &mut out);
    }
    if !h.value.is_empty() {
        proto::string_field(2, &h.value, &mut out);
    }
    out
}

fn decode_header(data: &[u8]) -> Result<Header, proto::ProtoError> {
    let mut h = Header { key: String::new(), value: String::new() };
    let mut r = Reader::new(data);
    while !r.at_end() {
        let (field, wire) = r.read_key()?;
        match field {
            1 => h.key = r.read_string()?,
            2 => h.value = r.read_string()?,
            _ => r.skip(wire)?,
        }
    }
    Ok(h)
}

fn encode_application_request(req: &ApplicationRequest) -> Vec<u8> {
    let mut out = Vec::new();
    if !req.verb.is_empty() {
        proto::string_field(1, &req.verb, &mut out);
    }
    if !req.path.is_empty() {
        proto::string_field(2, &req.path, &mut out);
    }
    for h in &req.headers {
        proto::delimited_field(3, &encode_header(h), &mut out);
    }
    if !req.body.is_empty() {
        proto::bytes_field(4, &req.body, &mut out);
    }
    if req.end_body {
        proto::bool_field(5, true, &mut out);
    }
    out
}

fn decode_application_response(data: &[u8]) -> Result<ApplicationResponse, proto::ProtoError> {
    let mut resp = ApplicationResponse::default();
    let mut r = Reader::new(data);
    while !r.at_end() {
        let (field, wire) = r.read_key()?;
        match field {
            1 => resp.status = proto::decode_int32(r.read_varint()?)?,
            2 => resp.headers.push(decode_header(r.read_delimited()?)?),
            3 => resp.body = r.read_delimited()?.to_vec(),
            4 => resp.end_body = r.read_varint()? != 0,
            _ => r.skip(wire)?,
        }
    }
    Ok(resp)
}

fn encode_body_chunk(c: &BodyChunk) -> Vec<u8> {
    let mut out = Vec::new();
    if !c.data.is_empty() {
        proto::bytes_field(1, &c.data, &mut out);
    }
    if c.end_body {
        proto::bool_field(2, true, &mut out);
    }
    out
}

fn decode_body_chunk(data: &[u8]) -> Result<BodyChunk, proto::ProtoError> {
    let mut c = BodyChunk::default();
    let mut r = Reader::new(data);
    while !r.at_end() {
        let (field, wire) = r.read_key()?;
        match field {
            1 => c.data = r.read_delimited()?.to_vec(),
            2 => c.end_body = r.read_varint()? != 0,
            _ => r.skip(wire)?,
        }
    }
    Ok(c)
}

fn encode_reset(reset: &Reset) -> Vec<u8> {
    let mut out = Vec::new();
    if reset.code != 0 {
        proto::int32_field(1, reset.code, &mut out);
    }
    if !reset.reason.is_empty() {
        proto::string_field(2, &reset.reason, &mut out);
    }
    out
}

fn decode_reset(data: &[u8]) -> Result<Reset, proto::ProtoError> {
    let mut reset = Reset::default();
    let mut r = Reader::new(data);
    while !r.at_end() {
        let (field, wire) = r.read_key()?;
        match field {
            1 => reset.code = proto::decode_int32(r.read_varint()?)?,
            2 => reset.reason = r.read_string()?,
            _ => r.skip(wire)?,
        }
    }
    Ok(reset)
}

pub fn encode_service_frame(frame: &ServiceFrame) -> Vec<u8> {
    let mut out = Vec::new();
    if frame.stream_id != 0 {
        proto::int64_field(1, frame.stream_id, &mut out);
    }
    match &frame.value {
        FrameValue::Request(r) => proto::delimited_field(2, &encode_application_request(r), &mut out),
        FrameValue::Response(r) => {
            // Responses are decoded, not encoded, by the client; included for symmetry.
            let mut inner = Vec::new();
            if r.status != 0 {
                proto::int32_field(1, r.status, &mut inner);
            }
            for h in &r.headers {
                proto::delimited_field(2, &encode_header(h), &mut inner);
            }
            if !r.body.is_empty() {
                proto::bytes_field(3, &r.body, &mut inner);
            }
            if r.end_body {
                proto::bool_field(4, true, &mut inner);
            }
            proto::delimited_field(3, &inner, &mut out);
        }
        FrameValue::BodyChunk(c) => proto::delimited_field(4, &encode_body_chunk(c), &mut out),
        FrameValue::Reset(r) => proto::delimited_field(5, &encode_reset(r), &mut out),
        FrameValue::Empty => {}
    }
    out
}

pub fn decode_service_frame(data: &[u8]) -> Result<ServiceFrame, proto::ProtoError> {
    let mut stream_id = 0i64;
    let mut value = FrameValue::Empty;
    let mut r = Reader::new(data);
    while !r.at_end() {
        let (field, wire) = r.read_key()?;
        match field {
            1 => stream_id = proto::decode_int64(r.read_varint()?),
            2 => value = FrameValue::Request(decode_application_request(r.read_delimited()?)?),
            3 => value = FrameValue::Response(decode_application_response(r.read_delimited()?)?),
            4 => value = FrameValue::BodyChunk(decode_body_chunk(r.read_delimited()?)?),
            5 => value = FrameValue::Reset(decode_reset(r.read_delimited()?)?),
            _ => r.skip(wire)?,
        }
    }
    Ok(ServiceFrame { stream_id, value })
}

fn decode_application_request(data: &[u8]) -> Result<ApplicationRequest, proto::ProtoError> {
    let mut req = ApplicationRequest::default();
    let mut r = Reader::new(data);
    while !r.at_end() {
        let (field, wire) = r.read_key()?;
        match field {
            1 => req.verb = r.read_string()?,
            2 => req.path = r.read_string()?,
            3 => req.headers.push(decode_header(r.read_delimited()?)?),
            4 => req.body = r.read_delimited()?.to_vec(),
            5 => req.end_body = r.read_varint()? != 0,
            _ => r.skip(wire)?,
        }
    }
    Ok(req)
}

pub fn encode_service_request(service: ServiceType, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    if service != ServiceType::Daemon {
        proto::varint_field(1, service as u64, &mut out);
    }
    if !payload.is_empty() {
        proto::bytes_field(2, payload, &mut out);
    }
    out
}

/// Decode a `ServiceResponse` (field #1 bytes = the inner ServiceFrame bytes).
pub fn decode_service_response(data: &[u8]) -> Result<Vec<u8>, proto::ProtoError> {
    let mut payload = Vec::new();
    let mut r = Reader::new(data);
    while !r.at_end() {
        let (field, wire) = r.read_key()?;
        match field {
            1 => payload = r.read_delimited()?.to_vec(),
            _ => r.skip(wire)?,
        }
    }
    Ok(payload)
}

// ---------------------------------------------------------------------------
// Reassembly (framing.py:NoiseFrameDecoder) — single-stream, in-order tolerant
// ---------------------------------------------------------------------------

use std::collections::HashMap;

#[derive(Default)]
struct Assembly {
    chunks: HashMap<u32, Vec<u8>>,
    total: u32,
    total_bytes: usize,
}

#[derive(Default)]
pub struct NoiseFrameDecoder {
    pending: HashMap<i64, Assembly>,
}

impl NoiseFrameDecoder {
    pub fn new() -> Self {
        NoiseFrameDecoder::default()
    }

    /// Feed one decoded `NoiseTransportFrame`; returns the full message when the
    /// last chunk of an assembly arrives.
    pub fn decode(&mut self, frame: NoiseTransportFrame) -> Result<Option<Vec<u8>>, proto::ProtoError> {
        if frame.total_chunks < 1 || frame.total_chunks > MAX_TOTAL_CHUNKS {
            return Err(proto::ProtoError("invalid totalChunks".into()));
        }
        if frame.chunk_index >= frame.total_chunks {
            return Err(proto::ProtoError("chunkIndex out of range".into()));
        }
        if frame.payload.len() > MAX_CHUNK_PAYLOAD {
            return Err(proto::ProtoError("payload too large for noise frame".into()));
        }
        let asm = self
            .pending
            .entry(frame.chunk_id)
            .or_insert_with(|| Assembly { total: frame.total_chunks, ..Default::default() });
        if asm.total != frame.total_chunks {
            self.pending.remove(&frame.chunk_id);
            return Err(proto::ProtoError("inconsistent totalChunks".into()));
        }
        if asm.chunks.contains_key(&frame.chunk_index) {
            self.pending.remove(&frame.chunk_id);
            return Err(proto::ProtoError("duplicate chunkIndex".into()));
        }
        asm.total_bytes += frame.payload.len();
        if asm.total_bytes > MAX_ASSEMBLY_BYTES {
            self.pending.remove(&frame.chunk_id);
            return Err(proto::ProtoError("assembly exceeded byte budget".into()));
        }
        let total = asm.total;
        asm.chunks.insert(frame.chunk_index, frame.payload);
        if (asm.chunks.len() as u32) < total {
            return Ok(None);
        }
        let asm = self.pending.remove(&frame.chunk_id).unwrap();
        let mut out = Vec::with_capacity(asm.total_bytes);
        for i in 0..total {
            out.extend_from_slice(asm.chunks.get(&i).expect("chunk present"));
        }
        Ok(Some(out))
    }
}

// ---------------------------------------------------------------------------
// NoiseTransport (transport.py)
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct TransportError(pub String);
impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "transport: {}", self.0)
    }
}
impl std::error::Error for TransportError {}

const EMPTY_AD: &[u8] = b"";

/// Encrypts application requests and decrypts service responses over a Noise
/// session. `stream_id`s are allocated by the caller via `next_stream_id`.
pub struct NoiseTransport {
    send: CipherState,
    recv: CipherState,
    decoder: NoiseFrameDecoder,
    next_stream_id: i64,
}

#[derive(Debug)]
pub struct EncryptedFrames {
    pub stream_id: i64,
    pub frames: Vec<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub enum DecryptedFrame {
    Response { stream_id: i64, value: ApplicationResponse },
    BodyChunk { stream_id: i64, value: BodyChunk },
    Reset { stream_id: i64, value: Reset },
}

impl NoiseTransport {
    pub fn new(send: CipherState, recv: CipherState) -> Self {
        NoiseTransport {
            send,
            recv,
            decoder: NoiseFrameDecoder::new(),
            next_stream_id: 1,
        }
    }

    fn alloc_stream_id(&mut self) -> i64 {
        let id = self.next_stream_id;
        self.next_stream_id += 1;
        id
    }

    fn encrypt_frame(&mut self, frame: &ServiceFrame) -> Result<Vec<Vec<u8>>, TransportError> {
        let frame_bytes = encode_service_frame(frame);
        let request_bytes = encode_service_request(ServiceType::Daemon, &frame_bytes);
        let proto_frames =
            encode_noise_frames(&request_bytes, random_i64()).map_err(|e| TransportError(e.to_string()))?;
        let mut out = Vec::with_capacity(proto_frames.len());
        for pf in proto_frames {
            out.push(
                self.send
                    .encrypt_with_ad(EMPTY_AD, &pf)
                    .map_err(|e| TransportError(e.to_string()))?,
            );
        }
        Ok(out)
    }

    /// One-shot HTTP request with a complete body (`end_body = true`).
    pub fn encrypt_http_request(
        &mut self,
        method: &str,
        path: &str,
        body: &[u8],
        headers: Vec<Header>,
    ) -> Result<EncryptedFrames, TransportError> {
        self.application_request(method, path, body, headers, true)
    }

    /// Open a streaming request, leaving the body open for later chunks.
    pub fn start_stream_request(
        &mut self,
        method: &str,
        path: &str,
        headers: Vec<Header>,
    ) -> Result<EncryptedFrames, TransportError> {
        self.application_request(method, path, &[], headers, false)
    }

    fn application_request(
        &mut self,
        method: &str,
        path: &str,
        body: &[u8],
        headers: Vec<Header>,
        end_body: bool,
    ) -> Result<EncryptedFrames, TransportError> {
        let stream_id = self.alloc_stream_id();
        let frame = ServiceFrame {
            stream_id,
            value: FrameValue::Request(ApplicationRequest {
                verb: method.into(),
                path: path.into(),
                headers,
                body: body.to_vec(),
                end_body,
            }),
        };
        let frames = self.encrypt_frame(&frame)?;
        Ok(EncryptedFrames { stream_id, frames })
    }

    pub fn encrypt_body_chunk(
        &mut self,
        stream_id: i64,
        data: &[u8],
        end_body: bool,
    ) -> Result<Vec<Vec<u8>>, TransportError> {
        let frame = ServiceFrame {
            stream_id,
            value: FrameValue::BodyChunk(BodyChunk { data: data.to_vec(), end_body }),
        };
        self.encrypt_frame(&frame)
    }

    pub fn encrypt_reset(
        &mut self,
        stream_id: i64,
        reason: &str,
        code: i32,
    ) -> Result<Vec<Vec<u8>>, TransportError> {
        let frame = ServiceFrame {
            stream_id,
            value: FrameValue::Reset(Reset { code, reason: reason.into() }),
        };
        self.encrypt_frame(&frame)
    }

    /// Decrypt one inbound WebSocket binary frame. Returns `None` when more
    /// chunks are needed to complete the current assembly.
    pub fn decrypt_frame(&mut self, ciphertext: &[u8]) -> Result<Option<DecryptedFrame>, TransportError> {
        let plain = self
            .recv
            .decrypt_with_ad(EMPTY_AD, ciphertext)
            .map_err(|e| TransportError(e.to_string()))?;
        let frame = decode_noise_frame(&plain).map_err(|e| TransportError(e.to_string()))?;
        let Some(reassembled) = self
            .decoder
            .decode(frame)
            .map_err(|e| TransportError(e.to_string()))?
        else {
            return Ok(None);
        };
        let payload = decode_service_response(&reassembled).map_err(|e| TransportError(e.to_string()))?;
        if payload.is_empty() {
            return Err(TransportError("empty ServiceResponse payload".into()));
        }
        let frame = decode_service_frame(&payload).map_err(|e| TransportError(e.to_string()))?;
        Ok(Some(match frame.value {
            FrameValue::Response(v) => DecryptedFrame::Response { stream_id: frame.stream_id, value: v },
            FrameValue::BodyChunk(v) => DecryptedFrame::BodyChunk { stream_id: frame.stream_id, value: v },
            FrameValue::Reset(v) => DecryptedFrame::Reset { stream_id: frame.stream_id, value: v },
            FrameValue::Request(_) => {
                return Err(TransportError("unexpected request frame from server".into()))
            }
            FrameValue::Empty => return Ok(None),
        }))
    }
}
