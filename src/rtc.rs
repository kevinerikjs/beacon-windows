//! Media over UDP (Phoros rtc2, phoros/docs/realtime.md): a phoros-core peer (ICE lite, DTLS,
//! SCTP, RTP with NACK/RTX/FEC) that owns its socket. Video is RTP; parameter sets go on the
//! reliable lane, audio on the audio lane, each message tagged as PhorosPeerTransport does.
//! Pairing, auth and control stay on the TCP connection.

use crate::session::Session;
use phoros_core::peer::*;
use std::ffi::{c_void, CString};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, Weak};
use std::time::{Duration, Instant};

pub const LANE_RELIABLE: u32 = 0;
pub const LANE_AUDIO: u32 = 2;
pub const TAG_PARAMETER_SETS: u8 = 2;
pub const TAG_AUDIO: u8 = 5;
const TAG_CONTROL: u8 = 3;
const TAG_INPUT: u8 = 4;
const TAG_FRAME_ACK: u8 = 6;
const FIRST_PORT: u16 = 7981;
/// Frames went out and none was acknowledged for this long: the link died without ICE noticing.
const STALL: Duration = Duration::from_millis(700);

struct Shared {
    session: Weak<Session>,
    connected: AtomicBool,
    last_ack: Mutex<Instant>,
    last_send: Mutex<Option<Instant>>,
}

pub struct Peer {
    handle: Mutex<*mut PhorosPeer>,
    shared: *const Shared,
    pub address: String,
}

// SAFETY: the core locks internally; `handle` is guarded so destroy never races a call.
unsafe impl Send for Peer {}
unsafe impl Sync for Peer {}

impl Peer {
    /// Binds a host peer on `ip` (the address the phone reached us on) and returns it with
    /// the info string for `transport_offer`.
    pub fn offer(session: Weak<Session>, ip: std::net::IpAddr, desired_bitrate: u64) -> Option<(Peer, String)> {
        for port in FIRST_PORT..FIRST_PORT + 10 {
            let address = SocketAddr::new(ip, port).to_string();
            let shared = Box::into_raw(Box::new(Shared {
                session: session.clone(),
                connected: AtomicBool::new(false),
                last_ack: Mutex::new(Instant::now()),
                last_send: Mutex::new(None),
            }));
            let c_addr = CString::new(address.clone()).ok()?;
            // SAFETY: callbacks get `shared`, which lives until after destroy (see Drop).
            let handle = unsafe { phoros_peer_create(shared as *mut c_void, Some(on_event), Some(on_data), true, c_addr.as_ptr()) };
            let peer = Peer { handle: Mutex::new(handle), shared, address };
            if handle.is_null() {
                return None;
            }
            // SAFETY: a live handle.
            if unsafe { phoros_peer_run_own_socket(handle) } != 0 {
                continue; // port taken by another phone's peer: try the next
            }
            unsafe { phoros_peer_set_desired_bitrate(handle, desired_bitrate) };
            let mut buf = vec![0u8; 512];
            let n = unsafe { phoros_peer_local_info(handle, buf.as_mut_ptr() as *mut _, buf.len()) };
            if n <= 0 {
                return None;
            }
            buf.truncate(n as usize);
            return Some((peer, String::from_utf8(buf).ok()?));
        }
        None
    }

    fn with<T>(&self, f: impl FnOnce(*mut PhorosPeer) -> T) -> Option<T> {
        let h = self.handle.lock().unwrap();
        (!h.is_null()).then(|| f(*h))
    }

    pub fn set_remote(&self, info: &str, address: &str) -> bool {
        let (Ok(info), Ok(address)) = (CString::new(info), CString::new(address)) else { return false };
        // SAFETY: valid C strings for the call.
        self.with(|h| unsafe { phoros_peer_set_remote(h, info.as_ptr(), address.as_ptr(), 0) } == 0).unwrap_or(false)
    }

    fn shared(&self) -> &Shared {
        // SAFETY: freed only in Drop, after the handle is destroyed.
        unsafe { &*self.shared }
    }

    /// One Annex B frame over RTP. False when the core refused it.
    pub fn send_video(&self, annex_b: &[u8], pts: i64, hevc: bool, keyframe: bool) -> bool {
        *self.shared().last_send.lock().unwrap() = Some(Instant::now());
        let codec = if hevc { PHOROS_CODEC_H265 } else { PHOROS_CODEC_H264 };
        // SAFETY: the slice is valid for the call.
        self.with(|h| unsafe { phoros_peer_send_video(h, annex_b.as_ptr(), annex_b.len(), pts, codec, keyframe) } == 0).unwrap_or(false)
    }

    /// A tagged message on a data channel. The reliable lane waits briefly for room; audio is
    /// dropped when the association is full, its lifetime is short anyway.
    pub fn send(&self, lane: u32, tag: u8, payload: &[u8]) -> bool {
        let mut message = Vec::with_capacity(1 + payload.len());
        message.push(tag);
        message.extend_from_slice(payload);
        for _ in 0..if lane == LANE_RELIABLE { 100 } else { 1 } {
            // SAFETY: the message is valid for the call.
            match self.with(|h| unsafe { phoros_peer_send(h, lane, message.as_ptr(), message.len()) }) {
                Some(0) => return true,
                Some(s) if s == phoros_core::PHOROS_ERR_BUSY => std::thread::sleep(Duration::from_millis(1)),
                _ => return false,
            }
        }
        false
    }

    pub fn is_stalled(&self) -> bool {
        let s = self.shared();
        let last_ack = *s.last_ack.lock().unwrap();
        s.last_send.lock().unwrap().is_some_and(|sent| sent > last_ack) && last_ack.elapsed() > STALL
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        let h = std::mem::replace(&mut *self.handle.lock().unwrap(), std::ptr::null_mut()) as usize;
        let shared = self.shared as usize;
        // On a fresh thread: destroy joins the peer's socket thread, which may be the one
        // dropping the last reference (from inside a callback).
        std::thread::spawn(move || {
            // SAFETY: destroy joins the socket thread, so no callback runs after it; then `shared` is free.
            unsafe {
                if h != 0 {
                    phoros_peer_destroy(h as *mut PhorosPeer);
                }
                drop(Box::from_raw(shared as *mut Shared));
            }
        });
    }
}

unsafe extern "C" fn on_event(user: *mut c_void, kind: u32, value: i64) {
    let shared = &*(user as *const Shared);
    let Some(session) = shared.session.upgrade() else { return };
    match kind {
        PHOROS_PEER_EVENT_CONNECTED => {
            if !shared.connected.swap(true, Ordering::SeqCst) {
                *shared.last_ack.lock().unwrap() = Instant::now();
                session.rtc_link(true);
            }
        }
        // is::IceConnectionState: 2 connected, 3 completed, 4 disconnected
        PHOROS_PEER_EVENT_ICE_STATE if shared.connected.load(Ordering::SeqCst) => {
            if value == 4 {
                session.rtc_link(false);
            } else if value >= 2 {
                session.rtc_link(true);
            }
        }
        PHOROS_PEER_EVENT_KEYFRAME_REQUEST => session.rtc_keyframe_request(),
        PHOROS_PEER_EVENT_DISCONNECTED => session.rtc_failed("peer disconnected"),
        _ => {} // shortcut: TWCC bandwidth estimates are ignored; the ffmpeg encoder can't follow a live bitrate.
    }
}

unsafe extern "C" fn on_data(user: *mut c_void, _channel: u32, bytes: *const u8, len: usize) {
    let shared = &*(user as *const Shared);
    if len == 0 {
        return;
    }
    let message = std::slice::from_raw_parts(bytes, len);
    match message[0] {
        TAG_FRAME_ACK => *shared.last_ack.lock().unwrap() = Instant::now(),
        TAG_INPUT if len > 1 => {
            if let Some(s) = shared.session.upgrade() {
                s.controller_report(&message[2..], message[1] & 1 != 0);
            }
        }
        TAG_CONTROL => {
            if let (Some(s), Ok(v)) = (shared.session.upgrade(), serde_json::from_slice(&message[1..])) {
                s.handle_json(&v);
            }
        }
        _ => {}
    }
}
