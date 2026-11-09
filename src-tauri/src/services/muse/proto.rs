// Copyright (c) code-rs contributors. MIT license (see LICENSE.txt).
//
//! Minimal protobuf wire codec, mirroring the SDK's `noise/_proto.py`.
//!
//! Only the varint and length-delimited wire types the Muse envelope uses are
//! implemented here; fixed32/fixed64 are skipped on read. This keeps the Muse
//! service self-contained without pulling in a protobuf toolchain.

pub const WIRE_VARINT: u64 = 0;
pub const WIRE_FIXED64: u64 = 1;
pub const WIRE_DELIMITED: u64 = 2;
pub const WIRE_FIXED32: u64 = 5;

#[derive(Debug)]
pub struct ProtoError(pub String);

impl std::fmt::Display for ProtoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "proto: {}", self.0)
    }
}

impl std::error::Error for ProtoError {}

type R<T> = Result<T, ProtoError>;

const FIRST_RESERVED: u64 = 19000;
const LAST_RESERVED: u64 = 19999;
const MAX_FIELD_NUMBER: u64 = (1 << 29) - 1;

fn valid_field_number(n: u64) -> bool {
    n != 0 && n <= MAX_FIELD_NUMBER && !(FIRST_RESERVED..=LAST_RESERVED).contains(&n)
}

// --- encoding ---

pub fn encode_varint(mut value: u64, out: &mut Vec<u8>) {
    loop {
        let byte = (value & 0x7F) as u8;
        value >>= 7;
        if value != 0 {
            out.push(byte | 0x80);
        } else {
            out.push(byte);
            return;
        }
    }
}

fn encode_key(field: u64, wire: u64, out: &mut Vec<u8>) {
    debug_assert!(valid_field_number(field));
    encode_varint((field << 3) | wire, out);
}

pub fn varint_field(field: u64, value: u64, out: &mut Vec<u8>) {
    encode_key(field, WIRE_VARINT, out);
    encode_varint(value, out);
}

/// Encode an int64 as protobuf stores it (two's-complement in a u64 varint).
pub fn int64_field(field: u64, value: i64, out: &mut Vec<u8>) {
    varint_field(field, value as u64, out);
}

/// Encode an int32 (sign-extended to 64 bits, as protobuf does for negatives).
pub fn int32_field(field: u64, value: i32, out: &mut Vec<u8>) {
    varint_field(field, value as i64 as u64, out);
}

pub fn uint32_field(field: u64, value: u32, out: &mut Vec<u8>) {
    varint_field(field, value as u64, out);
}

pub fn bool_field(field: u64, value: bool, out: &mut Vec<u8>) {
    varint_field(field, if value { 1 } else { 0 }, out);
}

pub fn delimited_field(field: u64, payload: &[u8], out: &mut Vec<u8>) {
    encode_key(field, WIRE_DELIMITED, out);
    encode_varint(payload.len() as u64, out);
    out.extend_from_slice(payload);
}

pub fn string_field(field: u64, value: &str, out: &mut Vec<u8>) {
    delimited_field(field, value.as_bytes(), out);
}

pub fn bytes_field(field: u64, value: &[u8], out: &mut Vec<u8>) {
    delimited_field(field, value, out);
}

// --- decoding ---

pub struct Reader<'a> {
    data: &'a [u8],
    pub offset: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Reader { data, offset: 0 }
    }

    pub fn at_end(&self) -> bool {
        self.offset >= self.data.len()
    }

    pub fn read_varint(&mut self) -> R<u64> {
        let mut value: u64 = 0;
        let mut shift = 0u32;
        for i in 0..10 {
            if self.offset >= self.data.len() {
                return Err(ProtoError("truncated varint".into()));
            }
            let byte = self.data[self.offset];
            self.offset += 1;
            if i == 9 && (byte & 0xFE) != 0 {
                return Err(ProtoError("malformed varint".into()));
            }
            value |= ((byte & 0x7F) as u64) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
            shift += 7;
        }
        Err(ProtoError("malformed varint".into()))
    }

    /// Returns `(field_number, wire_type)`.
    pub fn read_key(&mut self) -> R<(u64, u64)> {
        let key = self.read_varint()?;
        let field = key >> 3;
        let wire = key & 0x07;
        if !valid_field_number(field) {
            return Err(ProtoError("invalid field number".into()));
        }
        if !matches!(wire, WIRE_VARINT | WIRE_FIXED64 | WIRE_DELIMITED | WIRE_FIXED32) {
            return Err(ProtoError("invalid wire type".into()));
        }
        Ok((field, wire))
    }

    pub fn read_delimited(&mut self) -> R<&'a [u8]> {
        let len = self.read_varint()? as usize;
        let end = self
            .offset
            .checked_add(len)
            .ok_or_else(|| ProtoError("length overflow".into()))?;
        if end > self.data.len() {
            return Err(ProtoError("truncated delimited field".into()));
        }
        let slice = &self.data[self.offset..end];
        self.offset = end;
        Ok(slice)
    }

    pub fn read_string(&mut self) -> R<String> {
        let bytes = self.read_delimited()?;
        String::from_utf8(bytes.to_vec()).map_err(|_| ProtoError("invalid utf-8 string".into()))
    }

    pub fn skip(&mut self, wire: u64) -> R<()> {
        match wire {
            WIRE_VARINT => {
                self.read_varint()?;
            }
            WIRE_FIXED64 => {
                let end = self.offset + 8;
                if end > self.data.len() {
                    return Err(ProtoError("truncated fixed64".into()));
                }
                self.offset = end;
            }
            WIRE_DELIMITED => {
                self.read_delimited()?;
            }
            WIRE_FIXED32 => {
                let end = self.offset + 4;
                if end > self.data.len() {
                    return Err(ProtoError("truncated fixed32".into()));
                }
                self.offset = end;
            }
            _ => return Err(ProtoError("invalid wire type".into())),
        }
        Ok(())
    }
}

/// Decode an int32 from the raw varint value (protobuf sign handling).
pub fn decode_int32(value: u64) -> R<i32> {
    let signed = if value >= 1 << 63 {
        (value as i128) - (1i128 << 64)
    } else {
        value as i128
    };
    if signed < i32::MIN as i128 || signed > i32::MAX as i128 {
        return Err(ProtoError("int32 out of range".into()));
    }
    Ok(signed as i32)
}

pub fn decode_int64(value: u64) -> i64 {
    value as i64
}

pub fn decode_uint32(value: u64) -> R<u32> {
    if value > u32::MAX as u64 {
        return Err(ProtoError("uint32 out of range".into()));
    }
    Ok(value as u32)
}
