// Copyright (c) code-rs contributors. MIT license (see LICENSE.txt).
//
//! Noise XX handshake and cipher state, mirroring the SDK's
//! `noise/noise_xx.py` byte-for-byte.
//!
//! Protocol: `Noise_XX_25519_AESGCM_SHA256`, initiator = gadget.
//!
//! NOTE: the AEAD nonce is **big-endian** (4 zero bytes ++ big-endian u64),
//! which differs from the Noise spec's little-endian. This is why we cannot use
//! the `snow` crate unmodified and hand-roll the state machine on RustCrypto
//! primitives instead; the wire bytes then match the SDK and its test vectors.

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey, StaticSecret};

type HmacSha256 = Hmac<Sha256>;

pub const PROTOCOL_NAME: &[u8] = b"Noise_XX_25519_AESGCM_SHA256";
pub const DH_KEY_LEN: usize = 32;
pub const AEAD_TAG_LEN: usize = 16;
pub const MIN_MSG2_LEN: usize = DH_KEY_LEN + (DH_KEY_LEN + AEAD_TAG_LEN) + AEAD_TAG_LEN;
pub const MIN_MSG3_LEN: usize = DH_KEY_LEN + AEAD_TAG_LEN + AEAD_TAG_LEN;

#[derive(Debug)]
pub struct NoiseError(pub String);

impl std::fmt::Display for NoiseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "noise: {}", self.0)
    }
}
impl std::error::Error for NoiseError {}

type R<T> = Result<T, NoiseError>;

/// 4 zero bytes ++ big-endian u64 counter (noise_xx.py:build_nonce_iv).
pub fn build_nonce_iv(nonce: u64) -> [u8; 12] {
    let mut iv = [0u8; 12];
    iv[4..].copy_from_slice(&nonce.to_be_bytes());
    iv
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut mac = <HmacSha256 as Mac>::new_from_slice(key).expect("hmac key");
    mac.update(data);
    mac.finalize().into_bytes().into()
}

/// HKDF per the Noise spec (2 or 3 outputs), matching `_hkdf` in noise_xx.py.
fn hkdf(chaining_key: &[u8], ikm: &[u8], outputs: usize) -> Vec<[u8; 32]> {
    let temp_key = hmac_sha256(chaining_key, ikm);
    let o1 = hmac_sha256(&temp_key, &[0x01]);
    let mut in2 = o1.to_vec();
    in2.push(0x02);
    let o2 = hmac_sha256(&temp_key, &in2);
    if outputs == 2 {
        return vec![o1, o2];
    }
    let mut in3 = o2.to_vec();
    in3.push(0x03);
    let o3 = hmac_sha256(&temp_key, &in3);
    vec![o1, o2, o3]
}

/// AEAD cipher state with a monotonic nonce (noise_xx.py:CipherState).
pub struct CipherState {
    cipher: Option<Aes256Gcm>,
    nonce: u64,
}

impl CipherState {
    pub fn new() -> Self {
        CipherState { cipher: None, nonce: 0 }
    }

    pub fn initialize_key(&mut self, key: &[u8; 32]) {
        self.cipher = Some(Aes256Gcm::new(key.into()));
        self.nonce = 0;
    }

    pub fn has_key(&self) -> bool {
        self.cipher.is_some()
    }

    pub fn encrypt_with_ad(&mut self, ad: &[u8], plaintext: &[u8]) -> R<Vec<u8>> {
        let Some(cipher) = &self.cipher else {
            return Ok(plaintext.to_vec());
        };
        let iv = build_nonce_iv(self.nonce);
        self.nonce += 1;
        cipher
            .encrypt(Nonce::from_slice(&iv), Payload { msg: plaintext, aad: ad })
            .map_err(|_| NoiseError("encrypt failed".into()))
    }

    pub fn decrypt_with_ad(&mut self, ad: &[u8], ciphertext: &[u8]) -> R<Vec<u8>> {
        let Some(cipher) = &self.cipher else {
            return Ok(ciphertext.to_vec());
        };
        let iv = build_nonce_iv(self.nonce);
        self.nonce += 1;
        cipher
            .decrypt(Nonce::from_slice(&iv), Payload { msg: ciphertext, aad: ad })
            .map_err(|_| NoiseError("decrypt failed".into()))
    }
}

impl Default for CipherState {
    fn default() -> Self {
        Self::new()
    }
}

struct SymmetricState {
    ck: [u8; 32],
    h: [u8; 32],
    cipher: CipherState,
}

impl SymmetricState {
    fn new() -> Self {
        // initialize(): h = protocol name padded to 32 bytes, ck = h, then mix_hash(b"").
        let mut h = [0u8; 32];
        h[..PROTOCOL_NAME.len()].copy_from_slice(PROTOCOL_NAME);
        let mut s = SymmetricState { ck: h, h, cipher: CipherState::new() };
        s.mix_hash(b"");
        s
    }

    fn mix_hash(&mut self, data: &[u8]) {
        let mut hasher = Sha256::new();
        hasher.update(self.h);
        hasher.update(data);
        self.h = hasher.finalize().into();
    }

    fn mix_key(&mut self, ikm: &[u8]) {
        let out = hkdf(&self.ck, ikm, 2);
        self.ck = out[0];
        let mut cs = CipherState::new();
        cs.initialize_key(&out[1]);
        self.cipher = cs;
    }

    fn encrypt_and_hash(&mut self, plaintext: &[u8]) -> R<Vec<u8>> {
        let ct = self.cipher.encrypt_with_ad(&self.h, plaintext)?;
        self.mix_hash(&ct);
        Ok(ct)
    }

    fn decrypt_and_hash(&mut self, ciphertext: &[u8]) -> R<Vec<u8>> {
        let pt = self.cipher.decrypt_with_ad(&self.h, ciphertext)?;
        self.mix_hash(ciphertext);
        Ok(pt)
    }

    fn split(&mut self) -> (CipherState, CipherState) {
        let out = hkdf(&self.ck, b"", 2);
        let mut c1 = CipherState::new();
        c1.initialize_key(&out[0]);
        let mut c2 = CipherState::new();
        c2.initialize_key(&out[1]);
        (c1, c2)
    }
}

fn dh(private: &StaticSecret, public_bytes: &[u8]) -> R<[u8; 32]> {
    if public_bytes.len() != DH_KEY_LEN {
        return Err(NoiseError("x25519: invalid public key length".into()));
    }
    let mut pk = [0u8; 32];
    pk.copy_from_slice(public_bytes);
    let shared = private.diffie_hellman(&PublicKey::from(pk));
    if shared.as_bytes() == &[0u8; 32] {
        return Err(NoiseError("x25519: DH produced all-zeros output".into()));
    }
    Ok(*shared.as_bytes())
}

#[derive(PartialEq, Eq, Debug, Clone, Copy)]
enum Phase {
    Initialized,
    Msg1Sent,
    Msg2Read,
    Msg3Sent,
    Split,
    Dead,
}

/// Noise XX initiator (the gadget side). One-shot; `split()` consumes the
/// handshake and returns `(send, recv)` cipher states.
pub struct NoiseXXInitiator {
    ss: SymmetricState,
    // Ephemeral/static kept as StaticSecret so we can DH multiple times.
    e: Option<StaticSecret>,
    e_pub: Option<[u8; 32]>,
    s: Option<StaticSecret>,
    re: Option<[u8; 32]>,
    rs: Option<[u8; 32]>,
    phase: Phase,
}

fn gen_keypair() -> (StaticSecret, [u8; 32]) {
    // EphemeralSecret would be ideal, but XX needs the private key for two DHs,
    // so a StaticSecret generated from the OS RNG is used and dropped with self.
    let mut raw = [0u8; 32];
    getrandom::fill(&mut raw).expect("getrandom");
    let secret = StaticSecret::from(raw);
    let public = PublicKey::from(&secret);
    (secret, *public.as_bytes())
}

impl NoiseXXInitiator {
    pub fn new() -> Self {
        NoiseXXInitiator {
            ss: SymmetricState::new(),
            e: None,
            e_pub: None,
            s: None,
            re: None,
            rs: None,
            phase: Phase::Initialized,
        }
    }

    /// Message 1: `-> e`.
    pub fn write_message1(&mut self) -> R<Vec<u8>> {
        if self.phase != Phase::Initialized {
            return self.die("write_message1 in wrong phase");
        }
        let (secret, public) = gen_keypair();
        self.ss.mix_hash(&public);
        self.ss.encrypt_and_hash(b"")?; // empty payload (no key yet -> plaintext)
        self.e = Some(secret);
        self.e_pub = Some(public);
        self.phase = Phase::Msg1Sent;
        Ok(public.to_vec())
    }

    /// Message 2: `<- e, ee, s, es` (empty payload). Returns the decrypted payload.
    pub fn read_message2(&mut self, msg: &[u8]) -> R<Vec<u8>> {
        if self.phase != Phase::Msg1Sent {
            return self.die("read_message2 in wrong phase");
        }
        if msg.len() < MIN_MSG2_LEN {
            return self.die("message 2 too short");
        }
        let mut off = 0;
        let mut re = [0u8; 32];
        re.copy_from_slice(&msg[off..off + DH_KEY_LEN]);
        self.ss.mix_hash(&re);
        off += DH_KEY_LEN;
        self.re = Some(re);

        let e = self.e.as_ref().ok_or_else(|| NoiseError("missing e".into()))?;
        let ee = dh(e, &re)?;
        self.ss.mix_key(&ee);

        let enc_s = &msg[off..off + DH_KEY_LEN + AEAD_TAG_LEN];
        let rs_vec = self.ss.decrypt_and_hash(enc_s)?;
        off += DH_KEY_LEN + AEAD_TAG_LEN;
        let mut rs = [0u8; 32];
        rs.copy_from_slice(&rs_vec);
        self.rs = Some(rs);

        let e = self.e.as_ref().unwrap();
        let es = dh(e, &rs)?;
        self.ss.mix_key(&es);

        let payload = self.ss.decrypt_and_hash(&msg[off..])?;
        self.phase = Phase::Msg2Read;
        Ok(payload)
    }

    /// Message 3: `-> s, se` (empty payload).
    pub fn write_message3(&mut self) -> R<Vec<u8>> {
        if self.phase != Phase::Msg2Read {
            return self.die("write_message3 in wrong phase");
        }
        let (secret, public) = gen_keypair();
        let enc_s = self.ss.encrypt_and_hash(&public)?;
        let re = self.re.ok_or_else(|| NoiseError("missing re".into()))?;
        let se = dh(&secret, &re)?;
        self.ss.mix_key(&se);
        let enc_payload = self.ss.encrypt_and_hash(b"")?;
        self.s = Some(secret);
        self.phase = Phase::Msg3Sent;
        let mut out = enc_s;
        out.extend_from_slice(&enc_payload);
        Ok(out)
    }

    /// Consume the handshake, returning `(send, recv)` cipher states.
    pub fn split(mut self) -> R<(CipherState, CipherState)> {
        if self.phase != Phase::Msg3Sent {
            return Err(NoiseError("split in wrong phase".into()));
        }
        self.phase = Phase::Split;
        Ok(self.ss.split())
    }

    pub fn remote_static_public_key(&self) -> Option<[u8; 32]> {
        self.rs
    }

    pub fn handshake_hash(&self) -> [u8; 32] {
        self.ss.h
    }

    fn die<T>(&mut self, why: &str) -> R<T> {
        self.phase = Phase::Dead;
        Err(NoiseError(why.into()))
    }
}

impl Default for NoiseXXInitiator {
    fn default() -> Self {
        Self::new()
    }
}

/// Test-only responder, mirroring `NoiseXXResponder` in the SDK, so the
/// handshake and transport can be exercised without a network peer.
#[cfg(test)]
pub struct NoiseXXResponder {
    ss: SymmetricState,
    e: Option<StaticSecret>,
    e_pub: Option<[u8; 32]>,
    s: Option<StaticSecret>,
    re: Option<[u8; 32]>,
    payload: Vec<u8>,
    phase: Phase,
}

#[cfg(test)]
impl NoiseXXResponder {
    pub fn new() -> Self {
        NoiseXXResponder {
            ss: SymmetricState::new(),
            e: None,
            e_pub: None,
            s: None,
            re: None,
            payload: Vec::new(),
            phase: Phase::Initialized,
        }
    }

    pub fn read_message1_and_write_message2(&mut self, msg1: &[u8]) -> R<Vec<u8>> {
        if msg1.len() < DH_KEY_LEN {
            return Err(NoiseError("message 1 too short".into()));
        }
        let mut re = [0u8; 32];
        re.copy_from_slice(&msg1[..DH_KEY_LEN]);
        self.ss.mix_hash(&re);
        self.ss.decrypt_and_hash(&msg1[DH_KEY_LEN..])?;
        self.re = Some(re);

        let (e_secret, e_pub) = gen_keypair();
        self.ss.mix_hash(&e_pub);
        let ee = dh(&e_secret, &re)?;
        self.ss.mix_key(&ee);

        let (s_secret, s_pub) = gen_keypair();
        let enc_s = self.ss.encrypt_and_hash(&s_pub)?;
        let es = dh(&s_secret, &re)?;
        self.ss.mix_key(&es);
        let enc_payload = self.ss.encrypt_and_hash(&self.payload.clone())?;

        self.e = Some(e_secret);
        self.e_pub = Some(e_pub);
        self.s = Some(s_secret);
        self.phase = Phase::Msg2Read;

        let mut out = e_pub.to_vec();
        out.extend_from_slice(&enc_s);
        out.extend_from_slice(&enc_payload);
        Ok(out)
    }

    pub fn read_message3(&mut self, msg3: &[u8]) -> R<()> {
        if msg3.len() < MIN_MSG3_LEN {
            return Err(NoiseError("message 3 too short".into()));
        }
        let rs_vec = self.ss.decrypt_and_hash(&msg3[..DH_KEY_LEN + AEAD_TAG_LEN])?;
        let mut rs = [0u8; 32];
        rs.copy_from_slice(&rs_vec);
        let e = self.e.as_ref().unwrap();
        let se = dh(e, &rs)?;
        self.ss.mix_key(&se);
        self.ss.decrypt_and_hash(&msg3[DH_KEY_LEN + AEAD_TAG_LEN..])?;
        self.phase = Phase::Msg3Sent;
        Ok(())
    }

    /// Returns `(send, recv)` for the responder (swapped relative to initiator).
    pub fn split(mut self) -> R<(CipherState, CipherState)> {
        let (c1, c2) = self.ss.split();
        Ok((c2, c1))
    }
}
