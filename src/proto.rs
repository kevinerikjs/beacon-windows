//! The Phoros wire contract (phoros/docs/wire-format.md): framing, packets and the
//! encrypted connection. Byte-for-byte what Beam on iOS speaks with Beacon on the Mac.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use hmac::{Hmac, Mac};
use rand::RngCore;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::{self, Read, Write};
use x25519_dalek::{PublicKey, StaticSecret};

pub const MAGIC: u32 = 0x4245_414D; // "BEAM"
pub const MAX_FRAME: usize = 8 << 20;
/// Video payload (16-byte fragment header + bitstream) stays at or under this.
pub const MAX_VIDEO_PAYLOAD: usize = 1400;

pub const VIDEO: u8 = 0x01;
pub const AUDIO: u8 = 0x02;
pub const CONTROL: u8 = 0x03;
pub const HEARTBEAT: u8 = 0x04;
pub const PARAMETER_SETS: u8 = 0x05;
pub const VIDEO_KEYFRAME: u8 = 0x06;
pub const INPUT: u8 = 0x07;

/// A packet body: 10-byte header and payload. The frame length prefix is added on write.
pub fn packet(kind: u8, flags: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(10 + payload.len());
    out.extend_from_slice(&MAGIC.to_be_bytes());
    out.push(kind);
    out.push(flags);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Host→client JSON always rides in a `.control` packet.
pub fn control_packet(message: &Value) -> Vec<u8> {
    packet(CONTROL, 0, message.to_string().as_bytes())
}

/// Splits one encoded frame into `.video`/`.videoKeyframe` packets.
pub fn video_packets(bitstream: &[u8], frame_number: u32, pts_micros: i64, keyframe: bool) -> Vec<Vec<u8>> {
    let chunk = MAX_VIDEO_PAYLOAD - 16;
    let count = bitstream.len().div_ceil(chunk).max(1);
    let kind = if keyframe { VIDEO_KEYFRAME } else { VIDEO };
    (0..count)
        .map(|i| {
            let body = &bitstream[i * chunk..((i + 1) * chunk).min(bitstream.len())];
            let mut payload = Vec::with_capacity(16 + body.len());
            payload.extend_from_slice(&frame_number.to_be_bytes());
            payload.extend_from_slice(&(i as u16).to_be_bytes());
            payload.extend_from_slice(&(count as u16).to_be_bytes());
            payload.extend_from_slice(&pts_micros.to_be_bytes());
            payload.extend_from_slice(body);
            packet(kind, 0, &payload)
        })
        .collect()
}

/// An AudioChunkHeader and the audio: the `.audio` packet payload on TCP, the body after the
/// codec byte on UDP.
pub fn audio_payload(sequence: u32, pts_micros: i64, audio: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(12 + audio.len());
    payload.extend_from_slice(&sequence.to_be_bytes());
    payload.extend_from_slice(&pts_micros.to_be_bytes());
    payload.extend_from_slice(audio);
    payload
}

/// A received frame body.
pub enum Frame {
    Packet { kind: u8, flags: u8, payload: Vec<u8> },
    Json(Value),
    Garbage,
}

pub fn classify(body: Vec<u8>) -> Frame {
    if body.len() >= 10 && u32::from_be_bytes([body[0], body[1], body[2], body[3]]) == MAGIC {
        let len = u32::from_be_bytes([body[6], body[7], body[8], body[9]]) as usize;
        if body.len() < 10 + len {
            return Frame::Garbage;
        }
        return Frame::Packet { kind: body[4], flags: body[5] & 0x0F, payload: body[10..10 + len].to_vec() };
    }
    match serde_json::from_slice(&body) {
        Ok(v @ Value::Object(_)) => Frame::Json(v),
        _ => Frame::Garbage,
    }
}

pub fn read_frame(r: &mut impl Read, sealed: bool) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME + if sealed { Cipher::OVERHEAD } else { 0 } {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame over limit"));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    Ok(body)
}

pub fn write_frame(w: &mut impl Write, body: &[u8]) -> io::Result<()> {
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(body);
    w.write_all(&out)
}

// MARK: - Encrypted connection (phoros/Sources/PhorosSession/SecureChannel.swift)

/// One direction: AES-256-GCM, nonce = 4 zero bytes ‖ UInt64 BE frame counter.
pub struct Cipher {
    aead: Aes256Gcm,
    counter: u64,
}

impl Cipher {
    pub const OVERHEAD: usize = 16;

    pub fn new(key: &[u8; 32]) -> Self {
        Cipher { aead: Aes256Gcm::new(key.into()), counter: 0 }
    }

    fn nonce(&mut self) -> [u8; 12] {
        let mut n = [0u8; 12];
        n[4..].copy_from_slice(&self.counter.to_be_bytes());
        self.counter += 1;
        n
    }

    pub fn seal(&mut self, plaintext: &[u8]) -> Vec<u8> {
        let n = self.nonce();
        self.aead.encrypt(Nonce::from_slice(&n), plaintext).expect("AES-GCM seal cannot fail")
    }

    /// `None` ends the connection: tampered, replayed, reordered or dropped frame.
    pub fn open(&mut self, sealed: &[u8]) -> Option<Vec<u8>> {
        let n = self.nonce();
        self.aead.decrypt(Nonce::from_slice(&n), sealed).ok()
    }
}

const LABEL: &[u8] = b"phoros-secure/1";
const HOST_PROOF: &[u8] = b"phoros-secure/1 host";

/// (client→host, host→client, confirm) keys.
#[allow(clippy::too_many_arguments)]
pub fn key_schedule(
    mode: &str,
    device_id: Option<&str>,
    secret: Option<&[u8; 32]>,
    agreement: &[u8; 32],
    client_key: &[u8],
    client_nonce: &[u8],
    host_key: &[u8],
    host_nonce: &[u8],
) -> ([u8; 32], [u8; 32], [u8; 32]) {
    let id = device_id.unwrap_or("").as_bytes();
    let mut transcript = Vec::new();
    transcript.extend_from_slice(LABEL);
    transcript.extend_from_slice(mode.as_bytes());
    transcript.extend_from_slice(&(id.len() as u32).to_be_bytes());
    transcript.extend_from_slice(id);
    for part in [client_key, client_nonce, host_key, host_nonce] {
        transcript.extend_from_slice(part);
    }
    let mut ikm = agreement.to_vec();
    if let Some(s) = secret {
        ikm.extend_from_slice(s);
    }
    let salt = Sha256::digest(&transcript);
    let mut okm = [0u8; 96];
    hkdf::Hkdf::<Sha256>::new(Some(&salt), &ikm)
        .expand(b"phoros-secure/1 keys", &mut okm)
        .expect("96 bytes is a valid HKDF length");
    let mut keys = [[0u8; 32]; 3];
    for (i, k) in keys.iter_mut().enumerate() {
        k.copy_from_slice(&okm[i * 32..(i + 1) * 32]);
    }
    (keys[0], keys[1], keys[2])
}

pub struct Accepted {
    pub reply: Value,
    pub send: Cipher,
    pub recv: Cipher,
    /// The device whose secret keyed the channel; `None` for a pairing connection.
    pub device_id: Option<String>,
}

/// Host side of `secure_hello`. `Err` carries the `secure_reject` to send before closing.
pub fn secure_accept(hello: &Value, stored_secret: impl Fn(&str) -> Option<[u8; 32]>) -> Result<Accepted, Value> {
    let reject = |e: &str| json!({"type": "secure_reject", "error": e});
    let b64 = |k: &str| hello.get(k).and_then(Value::as_str).and_then(|s| B64.decode(s).ok());
    let mode = hello.get("mode").and_then(Value::as_str).unwrap_or("");
    let (Some(ck), Some(cn)) = (b64("key"), b64("nonce")) else { return Err(reject("Malformed encrypted hello")) };
    let (Ok(ck), true, true) = (<[u8; 32]>::try_from(ck.as_slice()), cn.len() == 32, mode == "pair" || mode == "authenticate") else {
        return Err(reject("Malformed encrypted hello"));
    };
    let mut device_id = None;
    let mut secret = None;
    if mode == "authenticate" {
        let id = hello.get("deviceID").and_then(Value::as_str);
        let Some((id, s)) = id.and_then(|id| stored_secret(id).map(|s| (id, s))) else {
            return Err(reject("unknown_device"));
        };
        device_id = Some(id.to_string());
        secret = Some(s);
    }
    let ephemeral = StaticSecret::random_from_rng(rand::rngs::OsRng);
    let host_key = PublicKey::from(&ephemeral);
    let mut host_nonce = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut host_nonce);
    let agreement = ephemeral.diffie_hellman(&PublicKey::from(ck));
    let (c2h, h2c, confirm) = key_schedule(
        mode, device_id.as_deref(), secret.as_ref(), agreement.as_bytes(), &ck, &cn, host_key.as_bytes(), &host_nonce,
    );
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&confirm).expect("any key length");
    mac.update(HOST_PROOF);
    let proof = mac.finalize().into_bytes();
    Ok(Accepted {
        reply: json!({
            "type": "secure_accept",
            "key": B64.encode(host_key.as_bytes()),
            "nonce": B64.encode(host_nonce),
            "proof": B64.encode(proof),
        }),
        send: Cipher::new(&h2c),
        recv: Cipher::new(&c2h),
        device_id,
    })
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unhex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// Compares without leaking where the first mismatch is.
pub fn secrets_match(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Same inputs and outputs as SecureChannelTests.testKeySchedulePinned in Phoros.
    #[test]
    fn key_schedule_matches_phoros() {
        let client = StaticSecret::from([1u8; 32]);
        let host = StaticSecret::from([2u8; 32]);
        let agreement = client.diffie_hellman(&PublicKey::from(&host));
        let (c2h, h2c, confirm) = key_schedule(
            "authenticate", Some("phone-1"), Some(&[0x42; 32]), agreement.as_bytes(),
            PublicKey::from(&client).as_bytes(), &[3; 32], PublicKey::from(&host).as_bytes(), &[4; 32],
        );
        assert_eq!(hex(&c2h), "666c36fb4546f7f7b583a53fa1fb337c529eaf66bc1f7aedbe5f4f00966189ee");
        assert_eq!(hex(&h2c), "9bfd722397ece61f1ed3a12183e9065d7526eb54b7ef4571ef082b0fa6707cea");
        assert_eq!(hex(&confirm), "1e198e8ae6ac532373002a3a5ee3097fedc31b1a8b7d16b999c49274869d0946");
        assert_eq!(hex(&Cipher::new(&c2h).seal(b"phoros")), "d2ccd69a0f05681f92a2d4f3086058e6107d07829553");
    }

    #[test]
    fn cipher_rejects_replay() {
        let mut tx = Cipher::new(&[7; 32]);
        let mut rx = Cipher::new(&[7; 32]);
        let a = tx.seal(b"one");
        assert_eq!(rx.open(&a).as_deref(), Some(&b"one"[..]));
        assert!(rx.open(&a).is_none());
    }

    #[test]
    fn video_fragments_fit_and_number() {
        let packets = video_packets(&vec![9u8; 3000], 5, 77, true);
        assert_eq!(packets.len(), 3);
        for (i, p) in packets.iter().enumerate() {
            assert!(p.len() - 10 <= MAX_VIDEO_PAYLOAD);
            assert_eq!(p[4], VIDEO_KEYFRAME);
            assert_eq!(&p[10..14], &5u32.to_be_bytes());
            assert_eq!(u16::from_be_bytes([p[14], p[15]]) as usize, i);
            assert_eq!(u16::from_be_bytes([p[16], p[17]]), 3);
            assert_eq!(&p[18..26], &77i64.to_be_bytes());
        }
    }
}
