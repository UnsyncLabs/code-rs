// Copyright (c) code-rs contributors. MIT license (see LICENSE.txt).
//
//! Chunked BLE framing, mirroring the SDK's `ble_framing.py` and the ESP32
//! firmware (`ble_server.c`): every packet over one ATT notification/write is
//! framed as `0xFE`, chunk index, total chunks, then a payload fragment. A
//! packet that does not start with `0xFE` is a complete, unchunked message.

pub const CHUNK_MAGIC: u8 = 0xFE;
pub const HEADER_BYTES: usize = 3;
pub const MAX_PACKET_BYTES: usize = 160;
pub const MAX_CHUNKS: usize = 255;
pub const MAX_MESSAGE_BYTES: usize = 8192;
pub const DEFAULT_ATT_MTU: usize = 23;

/// Split `data` into framed packets that each fit one notification at `mtu`.
///
/// Matches `encode_chunks` in `ble_framing.py`: the usable payload per packet
/// is `min(mtu - 3, 160) - 3` bytes. An empty message still produces one
/// (empty-payload) chunk.
pub fn encode_chunks(data: &[u8], mtu: usize) -> Result<Vec<Vec<u8>>, String> {
    let notify_max = if mtu > 3 { mtu - 3 } else { 20 }.min(MAX_PACKET_BYTES);
    let usable = notify_max.saturating_sub(HEADER_BYTES).max(1);
    let mut fragments: Vec<&[u8]> = data.chunks(usable).collect();
    if fragments.is_empty() {
        fragments.push(&[]);
    }
    if fragments.len() > MAX_CHUNKS {
        return Err(format!(
            "message needs {} chunks (max {MAX_CHUNKS})",
            fragments.len()
        ));
    }
    let total = fragments.len() as u8;
    Ok(fragments
        .into_iter()
        .enumerate()
        .map(|(i, frag)| {
            let mut packet = Vec::with_capacity(HEADER_BYTES + frag.len());
            packet.push(CHUNK_MAGIC);
            packet.push(i as u8);
            packet.push(total);
            packet.extend_from_slice(frag);
            packet
        })
        .collect())
}

/// Reassembles chunked writes, strictly in order (`ChunkAssembler`).
///
/// Chunk index 0, or a change in the declared total, starts a new message. An
/// out-of-order chunk or an oversize message discards what has been collected.
pub struct ChunkAssembler {
    max_bytes: usize,
    buf: Vec<u8>,
    total: u8,
    next: u8,
}

impl ChunkAssembler {
    pub fn new() -> Self {
        ChunkAssembler {
            max_bytes: MAX_MESSAGE_BYTES,
            buf: Vec::new(),
            total: 0,
            next: 0,
        }
    }

    pub fn reset(&mut self) {
        self.buf.clear();
        self.total = 0;
        self.next = 0;
    }

    /// Add one write; returns a complete message once one is available.
    pub fn feed(&mut self, packet: &[u8]) -> Option<Vec<u8>> {
        if packet.len() < HEADER_BYTES || packet[0] != CHUNK_MAGIC {
            // Unchunked, complete message.
            return Some(packet.to_vec());
        }
        let index = packet[1];
        let total = packet[2];
        let fragment = &packet[HEADER_BYTES..];
        if total == 0 {
            self.reset();
            return None;
        }
        if index == 0 || total != self.total {
            self.reset();
            self.total = total;
        }
        if index != self.next || index >= self.total {
            self.reset();
            return None;
        }
        if self.buf.len() + fragment.len() > self.max_bytes {
            self.reset();
            return None;
        }
        self.buf.extend_from_slice(fragment);
        self.next = index + 1;
        if self.next < self.total {
            return None;
        }
        let message = std::mem::take(&mut self.buf);
        self.reset();
        Some(message)
    }
}

impl Default for ChunkAssembler {
    fn default() -> Self {
        Self::new()
    }
}
