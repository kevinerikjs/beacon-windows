//! A stand-in for Beam on iOS, for testing Beacon for Windows without a phone. It pairs over
//! an encrypted connection (reads the code from stdin), reconnects as the paired device and
//! authenticates (asking for AAC), takes the UDP offer with a phoros-core client peer, then
//! drives: ping, clock sync, a controller, the window list, video hold, a zoom, a quality
//! change, and finally stops acknowledging frames so the host must fall back to TCP. The
//! video it receives on both transports is saved as Annex B for ffprobe.
//!
//!   cargo run --release --example fake_phone -- 127.0.0.1:7979 out.h264 [hevc]

#[path = "../src/proto.rs"]
#[allow(dead_code)]
mod proto;

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use hmac::{Hmac, Mac};
use phoros_core::peer::*;
use proto::{Cipher, Frame};
use serde_json::{json, Value};
use sha2::Sha256;
use std::collections::BTreeMap;
use std::ffi::{c_void, CString};
use std::io::Write;
use std::net::TcpStream;
use std::sync::mpsc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use x25519_dalek::{PublicKey, StaticSecret};

const DEVICE: &str = "fake-phone-0001";

enum Ev {
    Tcp(u8, u8, Vec<u8>),
    RtcVideo(bool, Vec<u8>),
    RtcLane(u8, Vec<u8>),
}

static EVENTS: OnceLock<mpsc::Sender<Ev>> = OnceLock::new();

struct Link {
    stream: TcpStream,
    send: Cipher,
    recv: Cipher,
}

impl Link {
    fn open(addr: &str, secret: Option<[u8; 32]>) -> Link {
        let mut stream = TcpStream::connect(addr).expect("connect");
        let mine = StaticSecret::random_from_rng(rand::rngs::OsRng);
        let my_key = PublicKey::from(&mine);
        let nonce = [9u8; 32];
        let mode = if secret.is_some() { "authenticate" } else { "pair" };
        let mut hello = json!({"type": "secure_hello", "mode": mode, "key": B64.encode(my_key.as_bytes()), "nonce": B64.encode(nonce)});
        if secret.is_some() {
            hello["deviceID"] = json!(DEVICE);
        }
        proto::write_frame(&mut stream, hello.to_string().as_bytes()).unwrap();
        let reply: Value = serde_json::from_slice(&proto::read_frame(&mut stream, false).unwrap()).unwrap();
        assert_eq!(reply["type"], "secure_accept", "{reply}");
        let b = |k: &str| B64.decode(reply[k].as_str().unwrap()).unwrap();
        let host_key: [u8; 32] = b("key").try_into().unwrap();
        let agreement = mine.diffie_hellman(&PublicKey::from(host_key));
        let id = secret.map(|_| DEVICE);
        let (c2h, h2c, confirm) = proto::key_schedule(mode, id, secret.as_ref(), agreement.as_bytes(), my_key.as_bytes(), &nonce, &host_key, &b("nonce"));
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&confirm).unwrap();
        mac.update(b"phoros-secure/1 host");
        mac.verify_slice(&b("proof")).expect("host proof verifies: it holds our secret");
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        Link { stream, send: Cipher::new(&c2h), recv: Cipher::new(&h2c) }
    }

    fn send(&mut self, m: Value) {
        let sealed = self.send.seal(m.to_string().as_bytes());
        proto::write_frame(&mut self.stream, &sealed).unwrap();
    }

    fn send_packet(&mut self, kind: u8, flags: u8, payload: &[u8]) {
        let sealed = self.send.seal(&proto::packet(kind, flags, payload));
        proto::write_frame(&mut self.stream, &sealed).unwrap();
    }

    fn next(&mut self) -> (u8, u8, Vec<u8>) {
        let sealed = proto::read_frame(&mut self.stream, true).expect("frame from host");
        let body = self.recv.open(&sealed).expect("sealed frame opens");
        match proto::classify(body) {
            Frame::Packet { kind, flags, payload } => (kind, flags, payload),
            _ => panic!("host sends every frame as a packet"),
        }
    }

    fn until_message(&mut self, kind: &str) -> Value {
        loop {
            let (k, _, p) = self.next();
            if k == proto::CONTROL {
                let v: Value = serde_json::from_slice(&p).unwrap();
                if v["type"] == kind {
                    return v;
                }
            }
        }
    }
}

unsafe extern "C" fn on_event(_: *mut c_void, _: u32, _: i64) {}

unsafe extern "C" fn on_data(_: *mut c_void, channel: u32, bytes: *const u8, len: usize) {
    let d = std::slice::from_raw_parts(bytes, len);
    let tx = EVENTS.get().unwrap();
    if channel == PHOROS_CHANNEL_VIDEO && len >= 10 {
        let _ = tx.send(Ev::RtcVideo(d[8] != 0, d[10..].to_vec()));
    } else if len > 0 {
        let _ = tx.send(Ev::RtcLane(d[0], d[1..].to_vec()));
    }
}

fn controller_report(buttons: u32, seq: u16) -> Vec<u8> {
    let mut p = buttons.to_be_bytes().to_vec();
    p.extend_from_slice(&[0; 10]);
    p.extend_from_slice(&seq.to_be_bytes());
    p
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let addr = args.get(1).map_or("127.0.0.1:7979", String::as_str);
    let out_path = args.get(2).map_or("fake_phone.h264", String::as_str);
    let hevc = args.get(3).map(String::as_str) == Some("hevc");
    let codecs: Vec<&str> = if hevc { vec!["hevc", "h264"] } else { vec!["h264"] };

    // 1. Pair over an encrypted connection.
    let mut link = Link::open(addr, None);
    link.send(json!({"type": "hello", "deviceID": DEVICE, "deviceName": "Fake Phone", "supportedVideoCodecs": codecs}));
    let challenge = link.until_message("challenge");
    println!("challenge from {}; enter the code:", challenge["deviceName"]);
    let mut code = String::new();
    std::io::stdin().read_line(&mut code).unwrap();
    link.send(json!({"type": "code_verify", "deviceID": DEVICE, "code": "000000-wrong"}));
    assert_eq!(link.until_message("pair_failed")["error"], "Incorrect code");
    link.send(json!({"type": "code_verify", "deviceID": DEVICE, "code": code.trim()}));
    let paired = link.until_message("pair_success");
    assert_eq!(paired["supportsEncryption"], true);
    let secret = proto::unhex32(paired["sharedSecret"].as_str().unwrap()).unwrap();
    println!("PASS paired");
    drop(link);

    // 2. Reconnect as the paired device and authenticate.
    let mut link = Link::open(addr, Some(secret));
    link.send(json!({
        "type": "auth_request", "deviceID": DEVICE, "sharedSecret": proto::hex(&secret),
        "supportedAudioCodecs": ["aac_lc", "pcm_f32le"], "supportedVideoCodecs": codecs, "maximumFrameRate": 120.0,
    }));
    let auth = link.until_message("auth_success");
    assert_eq!(auth["selectedAudioCodec"], "aac_lc");
    assert_eq!(auth["supportsWindowSelection"], true);
    println!(
        "PASS authenticated: video {}, controller {}, buttons {}",
        auth["selectedVideoCodec"], auth["supportsControllerInput"], auth["phoneControls"].as_array().unwrap().len()
    );

    // 3. Everything from here is read on threads into one queue.
    let (tx, rx) = mpsc::channel();
    let _ = EVENTS.set(tx.clone());
    let mut reader = Link { stream: link.stream.try_clone().unwrap(), send: Cipher::new(&[0; 32]), recv: link.recv };
    link.recv = Cipher::new(&[0; 32]);
    reader.stream.set_read_timeout(None).unwrap();
    std::thread::spawn(move || loop {
        let (k, f, p) = reader.next();
        if tx.send(Ev::Tcp(k, f, p)).is_err() {
            break;
        }
    });

    let mut out = std::fs::File::create(out_path).unwrap();
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut count = |k: &str| *counts.entry(k.to_string()).or_default() += 1;
    let mut peer: *mut PhorosPeer = std::ptr::null_mut();
    let mut acking = true;
    let mut rtc_acks = 0u32;
    let mut frame: Vec<u8> = Vec::new();
    let mut current = None;
    let started = Instant::now();
    let mut done = [false; 10];
    let mut resumed_at = None;
    let mut first_key_after_resume = None;
    let mut fallback_at = None;
    let mut tcp_frames_after_fallback = 0;
    let mut audio_codecs = std::collections::BTreeSet::new();
    fn step(done: &mut [bool; 10], i: usize, at_ms: u64, t: Duration) -> bool {
        let due = !done[i] && t >= Duration::from_millis(at_ms);
        done[i] |= due;
        due
    }
    while started.elapsed() < Duration::from_secs(14) {
        let t = started.elapsed();
        if step(&mut done, 0, 1000, t) {
            link.send(json!({"type": "ping"}));
            link.send(json!({"type": "clock_probe", "payload": {"id": 7, "sentAt": 123}}));
        }
        if step(&mut done, 1, 1500, t) {
            for seq in 0..5u16 {
                link.send_packet(proto::INPUT, 1, &controller_report(if seq % 2 == 0 { 1 } else { 0 }, seq));
            }
            link.send_packet(proto::INPUT, 0, &controller_report(0, 5)); // controller gone
        }
        if step(&mut done, 2, 2000, t) {
            link.send(json!({"type": "window_list_request"}));
        }
        if step(&mut done, 3, 3000, t) {
            link.send(json!({"type": "video_pause"}));
        }
        if step(&mut done, 4, 4000, t) {
            link.send(json!({"type": "video_resume"}));
            resumed_at = Some(Instant::now());
        }
        if step(&mut done, 5, 5000, t) {
            link.send(json!({"type": "viewport_lock_request", "payload": {"locked": true, "x": 0.0, "y": 0.0, "width": 0.5, "height": 0.5}}));
        }
        if step(&mut done, 6, 6500, t) {
            link.send(json!({"type": "viewport_lock_request", "payload": {"locked": false, "x": 0.0, "y": 0.0, "width": 1.0, "height": 1.0}}));
        }
        if step(&mut done, 7, 8000, t) {
            link.send(json!({"type": "quality_request", "payload": {"preset": "720p30"}}));
        }
        if step(&mut done, 8, 10000, t) {
            acking = false; // the UDP link "dies" without ICE noticing: the host must fall back
            println!("stopped acknowledging frames at {t:?}");
        }
        let ev = match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(e) => e,
            Err(_) => continue,
        };
        let paused = done[3] && !done[4];
        match ev {
            Ev::RtcVideo(key, annex_b) => {
                count(if key { "rtc_keyframes" } else { "rtc_frames" });
                if paused && t > Duration::from_millis(3300) {
                    count("VIDEO_WHILE_PAUSED");
                }
                if resumed_at.is_some() && key && first_key_after_resume.is_none() {
                    first_key_after_resume = resumed_at.map(|r: Instant| r.elapsed());
                }
                out.write_all(&annex_b).unwrap();
                if acking && !peer.is_null() {
                    let mut ack = vec![6u8];
                    ack.extend_from_slice(&rtc_acks.to_be_bytes());
                    rtc_acks += 1;
                    unsafe { phoros_peer_send(peer, 1, ack.as_ptr(), ack.len()) };
                }
            }
            Ev::RtcLane(tag, body) => match tag {
                2 => {
                    count("rtc_parameter_sets");
                    out.write_all(&body[1..]).unwrap();
                }
                5 => {
                    audio_codecs.insert(body[0]);
                    count("rtc_audio");
                }
                other => count(&format!("rtc_tag_{other}")),
            },
            Ev::Tcp(kind, flags, payload) => match kind {
                proto::VIDEO | proto::VIDEO_KEYFRAME => {
                    if paused && t > Duration::from_millis(3300) {
                        count("VIDEO_WHILE_PAUSED");
                    }
                    let n = u32::from_be_bytes(payload[..4].try_into().unwrap());
                    let (index, total) = (u16::from_be_bytes([payload[4], payload[5]]), u16::from_be_bytes([payload[6], payload[7]]));
                    if index == 0 {
                        frame.clear();
                        current = Some(n);
                    }
                    if current == Some(n) {
                        frame.extend_from_slice(&payload[16..]);
                        if index + 1 == total {
                            out.write_all(&frame).unwrap();
                            count(if kind == proto::VIDEO_KEYFRAME { "tcp_keyframes" } else { "tcp_frames" });
                            if fallback_at.is_some() {
                                tcp_frames_after_fallback += 1;
                            }
                            if resumed_at.is_some() && kind == proto::VIDEO_KEYFRAME && first_key_after_resume.is_none() {
                                first_key_after_resume = resumed_at.map(|r: Instant| r.elapsed());
                            }
                            current = None;
                        }
                    }
                }
                proto::PARAMETER_SETS => {
                    assert_eq!(flags, hevc as u8, "codec flag");
                    count("tcp_parameter_sets");
                    out.write_all(&payload).unwrap();
                }
                proto::AUDIO => {
                    audio_codecs.insert(flags);
                    count("tcp_audio");
                }
                proto::HEARTBEAT => count("heartbeats"),
                proto::CONTROL => {
                    let v: Value = serde_json::from_slice(&payload).unwrap();
                    let kind = v["type"].as_str().unwrap().to_string();
                    match kind.as_str() {
                        "clock_reply" => assert_eq!((v["payload"]["id"].as_u64(), v["payload"]["sentAt"].as_u64()), (Some(7), Some(123))),
                        "window_list" => println!("window list: {} windows", v["payload"]["windows"].as_array().unwrap().len()),
                        "quality_changed" => println!("quality_changed → {}", v["payload"]["preset"]),
                        "audio_format_changed" => println!("audio format {}", v["payload"]),
                        "transport_fallback" => {
                            println!("host fell back to TCP at {t:?}");
                            fallback_at = Some(t);
                        }
                        "transport_offer" => {
                            let p = &v["payload"];
                            assert_eq!(p["kind"], "rtc2");
                            let local = CString::new("127.0.0.1:7982").unwrap();
                            peer = unsafe { phoros_peer_create(std::ptr::null_mut(), Some(on_event), Some(on_data), false, local.as_ptr()) };
                            assert!(!peer.is_null());
                            assert_eq!(unsafe { phoros_peer_run_own_socket(peer) }, 0);
                            let (info, address) = (CString::new(p["info"].as_str().unwrap()).unwrap(), CString::new(p["address"].as_str().unwrap()).unwrap());
                            unsafe { phoros_peer_set_remote(peer, info.as_ptr(), address.as_ptr(), 0) };
                            let mut buf = vec![0u8; 512];
                            let n = unsafe { phoros_peer_local_info(peer, buf.as_mut_ptr() as *mut _, buf.len()) };
                            buf.truncate(n as usize);
                            link.send(json!({"type": "transport_answer", "payload": {"kind": "rtc2", "address": "127.0.0.1:7982", "info": String::from_utf8(buf).unwrap()}}));
                            println!("took the UDP offer at {}", p["address"]);
                        }
                        _ => {}
                    }
                    count(&kind);
                }
                other => count(&format!("tcp_unknown_{other}")),
            },
        }
    }
    println!("{counts:#?}");
    let n = |k: &str| counts.get(k).copied().unwrap_or(0);
    assert!(n("pong") == 1 && n("clock_reply") == 1, "pong and clock reply");
    assert_eq!(n("window_list"), 1, "window list");
    assert!(n("rtc_frames") + n("rtc_keyframes") > 30, "video over UDP");
    assert_eq!(n("VIDEO_WHILE_PAUSED"), 0, "no video while held");
    let k = first_key_after_resume.expect("video came back after resume");
    println!("first keyframe {k:?} after resume");
    assert!(k < Duration::from_millis(2500), "keyframe on demand, well before the 5 s periodic one");
    let fb = fallback_at.expect("host fell back to TCP when acks stopped");
    assert!(fb < Duration::from_secs(12), "fallback within ~1 s of the last ack");
    assert!(tcp_frames_after_fallback > 10, "video on TCP after the fallback");
    assert!(!audio_codecs.contains(&0), "no PCM to a client that negotiated AAC");
    println!("audio codecs seen: {audio_codecs:?}");
    println!("PASS stream; video saved to {out_path}");
    unsafe { phoros_peer_destroy(peer) };
}
