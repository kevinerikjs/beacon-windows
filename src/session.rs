//! One connected Beam client: the encrypted handshake, pairing, authentication, control
//! messages, and media on whichever transport carries it (UDP once the phone takes the
//! offer, the TCP connection otherwise). Mirrors beacon-macos StreamSession.swift.

use crate::proto::{self, Cipher, Frame};
use crate::{gamepad, host, input, rtc, store};
use serde_json::{json, Value};
use std::io::Write;
use std::net::{IpAddr, Shutdown, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, Weak};
use std::time::{Duration, Instant};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
pub struct State {
    pub authenticated: bool,
    pub device_id: Option<String>,
    pub wants_audio: bool,
    /// 0 PCM, 1 AAC-LC: the AudioCodecID this client listed on this connection.
    pub audio_codec: u8,
    pub decodes_hevc: bool,
    pub maximum_frame_rate: Option<f64>,
    pub bitrate_cap: Option<u64>,
    held: bool,
    has_parameter_sets: bool,
    needs_keyframe: bool,
}

pub struct Session {
    pub id: u64,
    me: Weak<Session>,
    pub state: Mutex<State>,
    /// Device whose pairing secret keyed this connection; `None` on a pairing connection.
    secure_device: Option<String>,
    local_ip: Option<IpAddr>,
    tx: mpsc::Sender<(Vec<u8>, usize)>,
    queued_video: AtomicUsize,
    streaming: AtomicBool,
    alive: AtomicBool,
    stream: TcpStream,
    wrong_codes: AtomicUsize,
    rtc: Mutex<Option<Arc<rtc::Peer>>>,
    /// Media goes over UDP while this is set.
    rtc_ready: AtomicBool,
    gamepad: Mutex<gamepad::Pad>,
}

/// The heartbeat timeout: a client that sends nothing for this long is gone.
const SILENCE_LIMIT: Duration = Duration::from_secs(30);

pub fn run(mut stream: TcpStream) {
    let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let Ok(first) = proto::read_frame(&mut stream, false) else { return };
    let hello = match proto::classify(first) {
        Frame::Json(v) if v["type"] == "secure_hello" => v,
        other => {
            // Beam before 3.6 speaks plaintext. This host only accepts encrypted connections.
            let kind = match &other {
                Frame::Json(v) => v["type"].as_str().unwrap_or("").to_string(),
                _ => String::new(),
            };
            log!("[{peer}] refused a plaintext client ({kind}): update Beam on the phone");
            let reply = if kind == "hello" { "pair_failed" } else { "auth_failed" };
            let msg = json!({"type": reply, "error": "This PC needs an encrypted connection. Update Beam to connect."});
            let _ = proto::write_frame(&mut stream, &proto::control_packet(&msg));
            return;
        }
    };
    let accepted = match proto::secure_accept(&hello, store::secret) {
        Ok(a) => a,
        Err(reject) => {
            log!("[{peer}] encrypted hello rejected: {}", reject["error"]);
            let _ = proto::write_frame(&mut stream, reject.to_string().as_bytes());
            return;
        }
    };
    if proto::write_frame(&mut stream, accepted.reply.to_string().as_bytes()).is_err() {
        return;
    }
    let Ok(write_half) = stream.try_clone() else { return };
    let Ok(shutdown_handle) = stream.try_clone() else { return };
    let (tx, rx) = mpsc::channel();
    let session = Arc::new_cyclic(|me| Session {
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        me: me.clone(),
        state: Mutex::new(State { wants_audio: true, ..State::default() }),
        secure_device: accepted.device_id,
        local_ip: stream.local_addr().ok().map(|a| a.ip()),
        tx,
        queued_video: AtomicUsize::new(0),
        streaming: AtomicBool::new(false),
        alive: AtomicBool::new(true),
        stream: shutdown_handle,
        wrong_codes: AtomicUsize::new(0),
        rtc: Mutex::new(None),
        rtc_ready: AtomicBool::new(false),
        gamepad: Mutex::default(),
    });
    log!("[{peer}] encrypted connection #{} ({})", session.id, session.secure_device.as_deref().map_or("pairing", |_| "paired device"));
    {
        let s = session.clone();
        std::thread::spawn(move || s.write_loop(write_half, accepted.send, rx));
    }
    host().sessions.lock().unwrap().push(session.clone());

    let _ = stream.set_read_timeout(Some(SILENCE_LIMIT));
    let mut recv: Cipher = accepted.recv;
    while let Ok(sealed) = proto::read_frame(&mut stream, true) {
        let Some(body) = recv.open(&sealed) else {
            log!("[{peer}] a sealed frame did not open; closing");
            break;
        };
        match proto::classify(body) {
            Frame::Json(v) => session.handle_json(&v),
            Frame::Packet { kind: proto::CONTROL, payload, .. } => {
                if let Ok(v) = serde_json::from_slice::<Value>(&payload) {
                    session.handle_json(&v)
                }
            }
            Frame::Packet { kind: proto::INPUT, flags, payload } => session.controller_report(&payload, flags & 1 != 0),
            Frame::Packet { .. } | Frame::Garbage => {} // heartbeats, anything newer
        }
    }
    session.close();
    log!("[{peer}] session #{} ended", session.id);
}

impl Session {
    fn write_loop(&self, mut out: TcpStream, mut cipher: Cipher, rx: mpsc::Receiver<(Vec<u8>, usize)>) {
        let heartbeat = proto::packet(proto::HEARTBEAT, 0, &[]);
        let mut last = Instant::now();
        let mut write = |body: &[u8], out: &mut TcpStream| -> std::io::Result<()> {
            proto::write_frame(out, &cipher.seal(body))
        };
        loop {
            let item = rx.recv_timeout(Duration::from_millis(20));
            if !self.alive.load(Ordering::Relaxed) {
                break;
            }
            let result = match item {
                Ok((body, video_bytes)) => {
                    let r = write(&body, &mut out);
                    self.queued_video.fetch_sub(video_bytes, Ordering::Relaxed);
                    r
                }
                // Keep the phone's Wi-Fi radio awake while streaming (a heartbeat when nothing
                // went out for 20 ms), and a heartbeat every 5 s otherwise.
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    self.check_rtc_stall();
                    let idle = last.elapsed();
                    if (self.streaming.load(Ordering::Relaxed) && idle >= Duration::from_millis(20)) || idle >= Duration::from_secs(5) {
                        write(&heartbeat, &mut out)
                    } else {
                        continue;
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            if result.is_err() {
                break;
            }
            last = Instant::now();
            let _ = out.flush();
        }
        self.close();
    }

    pub fn close(&self) {
        if self.alive.swap(false, Ordering::SeqCst) {
            let _ = self.stream.shutdown(Shutdown::Both);
            input::release_held();
            *self.gamepad.lock().unwrap() = gamepad::Pad::default(); // unplugs the virtual controller
            self.drop_rtc();
            host().session_ended(self.id);
        }
    }

    pub fn send_json(&self, message: Value) {
        let _ = self.tx.send((proto::control_packet(&message), 0));
    }

    fn control(&self, kind: &str, payload: Option<Value>) {
        let mut m = json!({"type": kind});
        if let Some(p) = payload {
            m["payload"] = p;
        }
        self.send_json(m);
    }

    fn media_peer(&self) -> Option<Arc<rtc::Peer>> {
        if self.rtc_ready.load(Ordering::SeqCst) { self.rtc.lock().unwrap().clone() } else { None }
    }

    pub fn send_parameter_sets(&self, sets: &[u8], codec_flags: u8) {
        {
            let mut st = self.state.lock().unwrap();
            if !st.authenticated {
                return;
            }
            st.has_parameter_sets = true;
            st.needs_keyframe = true; // a decoder built from these starts at a keyframe
        }
        match self.media_peer() {
            Some(p) => {
                let mut payload = vec![codec_flags];
                payload.extend_from_slice(sets);
                p.send(rtc::LANE_RELIABLE, rtc::TAG_PARAMETER_SETS, &payload);
            }
            None => {
                let _ = self.tx.send((proto::packet(proto::PARAMETER_SETS, codec_flags, sets), 0));
            }
        }
    }

    /// Sends one frame, or skips it when the link is behind. A skipped delta breaks the
    /// reference chain, so everything waits for a keyframe, which is asked for at once.
    pub fn send_video(&self, annex_b: &[u8], frame_number: u32, pts: i64, keyframe: bool, hevc: bool, budget: usize) {
        let mut st = self.state.lock().unwrap();
        if !st.authenticated || st.held || !st.has_parameter_sets || (st.needs_keyframe && !keyframe) {
            return;
        }
        if let Some(p) = self.media_peer() {
            st.needs_keyframe = false;
            drop(st);
            if !p.send_video(annex_b, pts, hevc, keyframe) && !keyframe {
                self.state.lock().unwrap().needs_keyframe = true;
                host().request_keyframe();
            }
            return;
        }
        if !keyframe && self.queued_video.load(Ordering::Relaxed) > budget {
            st.needs_keyframe = true;
            log!("Session #{}: link behind ({} KB queued), skipping to a keyframe", self.id, self.queued_video.load(Ordering::Relaxed) >> 10);
            drop(st);
            return host().request_keyframe();
        }
        st.needs_keyframe = false;
        drop(st);
        for p in proto::video_packets(annex_b, frame_number, pts, keyframe) {
            let n = p.len();
            self.queued_video.fetch_add(n, Ordering::Relaxed);
            if self.tx.send((p, n)).is_err() {
                self.queued_video.fetch_sub(n, Ordering::Relaxed);
            }
        }
    }

    /// `payload` is an AudioChunkHeader and the audio. Sent only to a client that negotiated
    /// `codec` on this connection: AAC on a PCM-only wire is white noise in someone's ears.
    pub fn send_audio(&self, codec: u8, payload: &[u8]) {
        {
            let st = self.state.lock().unwrap();
            if !st.authenticated || !st.wants_audio || st.audio_codec != codec {
                return;
            }
        }
        match self.media_peer() {
            Some(p) => {
                let mut message = vec![codec];
                message.extend_from_slice(payload);
                p.send(rtc::LANE_AUDIO, rtc::TAG_AUDIO, &message);
            }
            None => {
                let _ = self.tx.send((proto::packet(proto::AUDIO, codec, payload), 0));
            }
        }
    }

    pub fn controller_report(&self, payload: &[u8], connected: bool) {
        if self.state.lock().unwrap().authenticated {
            self.gamepad.lock().unwrap().handle(payload, connected);
        }
    }

    pub fn handle_json(&self, m: &Value) {
        match m["type"].as_str().unwrap_or("") {
            "hello" => host().begin_pairing(self, m),
            "code_verify" => self.verify_code(m),
            "auth_request" => self.authenticate(m),
            kind if self.state.lock().unwrap().authenticated => self.handle_control(kind, &m["payload"]),
            _ => {} // control before authentication is ignored
        }
    }

    fn verify_code(&self, m: &Value) {
        match host().verify_pairing(self.id, m["code"].as_str().unwrap_or("")) {
            Some(Ok((device_id, name, secret))) => {
                if let Err(e) = store::add(&device_id, &name, &secret) {
                    log!("Could not save the pairing: {e}");
                    return self.send_json(json!({"type": "pair_failed", "error": "This PC could not save the pairing."}));
                }
                let mut reply = host().capabilities("pair_success");
                reply["sharedSecret"] = json!(proto::hex(&secret));
                self.send_json(reply);
                log!("Paired with {name}");
            }
            Some(Err(())) => {
                // Five wrong codes end the attempt: a million codes must not be guessable in the five minutes.
                let wrong = self.wrong_codes.fetch_add(1, Ordering::Relaxed) + 1;
                if wrong >= 5 {
                    host().cancel_pairing();
                }
                self.send_json(json!({"type": "pair_failed", "error": "Incorrect code"}));
            }
            None => {} // expired or never started
        }
    }

    fn authenticate(&self, m: &Value) {
        let fail = |e: &str| {
            log!("Auth refused: {e}");
            self.send_json(json!({"type": "auth_failed", "error": e}));
        };
        let device_id = m["deviceID"].as_str().unwrap_or("").to_string();
        let Some(presented) = m["sharedSecret"].as_str().and_then(proto::unhex32) else { return fail("Invalid auth request") };
        // Keys derived from one device's secret don't vouch for another device.
        if self.secure_device.as_ref().is_some_and(|d| *d != device_id) {
            return fail("Authentication failed");
        }
        let Some(stored) = store::secret(&device_id) else { return fail("Device not paired") };
        if !proto::secrets_match(&stored, &presented) {
            return fail("Authentication failed");
        }
        let list = |k: &str| -> Vec<String> { m[k].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default() };
        let decodes_hevc = list("supportedVideoCodecs").iter().any(|c| c == "hevc");
        // Absent or empty means PCM only (compatibility rule 3).
        let aac = list("supportedAudioCodecs").iter().any(|c| c == "aac_lc");
        {
            let mut st = self.state.lock().unwrap();
            st.authenticated = true;
            st.device_id = Some(device_id.clone());
            st.wants_audio = m["wantsAudio"].as_bool().unwrap_or(true);
            st.audio_codec = aac as u8;
            st.decodes_hevc = decodes_hevc;
            st.maximum_frame_rate = m["maximumFrameRate"].as_f64();
        }
        self.streaming.store(true, Ordering::Relaxed);
        let mut reply = host().capabilities("auth_success");
        reply["selectedAudioCodec"] = json!(if aac { "aac_lc" } else { "pcm_f32le" });
        reply["selectedVideoCodec"] = json!(if decodes_hevc && host().encoders.hevc.is_some() { "hevc" } else { "h264" });
        self.send_json(reply);
        let name = store::name(&device_id).unwrap_or(device_id);
        log!("{name} connected");
        host().session_authenticated(self);
        self.offer_rtc();
    }

    fn handle_control(&self, kind: &str, payload: &Value) {
        match kind {
            "ping" => self.control("pong", None),
            "clock_probe" => {
                let received = host().micros();
                self.control("clock_reply", Some(json!({
                    "id": payload["id"], "sentAt": payload["sentAt"], "receivedAt": received, "repliedAt": host().micros(),
                })));
            }
            "media_key" if payload.is_object() => input::handle(payload),
            "video_pause" => {
                self.state.lock().unwrap().held = true;
                self.streaming.store(false, Ordering::Relaxed);
            }
            "video_resume" => {
                self.state.lock().unwrap().held = false;
                self.streaming.store(true, Ordering::Relaxed);
                // Everything during the hold was dropped: parameter sets again, then a keyframe.
                host().resend_parameter_sets(self);
                host().request_keyframe();
            }
            "stream_stop" => self.close(),
            "quality_request" => {
                if let Some(preset) = payload["preset"].as_str() {
                    host().set_preset(preset);
                }
            }
            "quality_feedback" => {
                if let Some(q) = payload["quality"].as_f64() {
                    host().quality_feedback(q);
                }
            }
            "bitrate_cap_request" => {
                self.state.lock().unwrap().bitrate_cap = payload["bitsPerSecond"].as_u64().map(|b| b.max(500_000));
                host().reconfigure(false);
            }
            "audio_enable_request" => {
                if let Some(enabled) = payload["enabled"].as_bool() {
                    self.state.lock().unwrap().wants_audio = enabled;
                }
            }
            // Window titles stay inside the authenticated session (compatibility rule 6).
            "window_list_request" => self.control("window_list", Some(host().window_list())),
            "window_select_request" => {
                if let Some(id) = payload["windowID"].as_u64() {
                    host().select_window(id as u32);
                }
            }
            "viewport_lock_request" => {
                let f = |k: &str| payload[k].as_f64().unwrap_or(0.0).clamp(0.0, 1.0);
                let lock = (payload["locked"].as_bool() == Some(true) && f("width") > 0.02 && f("height") > 0.02)
                    .then(|| [f("x"), f("y"), f("width"), f("height")]);
                host().set_viewport_lock(lock);
            }
            "transport_answer" if payload["kind"] == "rtc2" => {
                let peer = self.rtc.lock().unwrap().clone();
                if let (Some(p), Some(info), Some(address)) = (peer, payload["info"].as_str(), payload["address"].as_str()) {
                    log!("Session #{}: phone answered the UDP offer from {address}", self.id);
                    p.set_remote(info, address);
                }
            }
            "transport_fallback" => {
                // The phone's side of UDP failed; media is back on TCP and needs a keyframe.
                if self.drop_rtc() {
                    log!("Session #{}: phone gave up on UDP; media back on TCP", self.id);
                    self.state.lock().unwrap().needs_keyframe = true;
                    host().request_keyframe();
                }
            }
            // Informational, host-to-client, or unknown from a newer client: ignored.
            _ => {}
        }
    }

    // MARK: rtc2

    /// Offers a UDP peer on the address the phone reached us on. A phone that doesn't know
    /// the offer ignores it and stays on TCP.
    fn offer_rtc(&self) {
        // shortcut: IPv4 only; an IPv6 link-local address needs its scope id in the offer.
        let Some(ip @ IpAddr::V4(_)) = self.local_ip else { return };
        let bitrate = 6_000_000;
        let Some((peer, info)) = rtc::Peer::offer(self.me.clone(), ip, bitrate) else {
            return log!("Session #{}: could not open a UDP port for media; staying on TCP", self.id);
        };
        let address = peer.address.clone();
        *self.rtc.lock().unwrap() = Some(Arc::new(peer));
        // The host clock rides along so the phone can put RTP's 32-bit timestamps back on the
        // full timeline audio uses.
        self.control("transport_offer", Some(json!({"kind": "rtc2", "address": address, "info": info, "hostMicros": host().micros()})));
    }

    /// Takes the peer away (its Drop destroys it off this thread). True if there was one.
    fn drop_rtc(&self) -> bool {
        self.rtc_ready.store(false, Ordering::SeqCst);
        self.rtc.lock().unwrap().take().is_some()
    }

    /// The UDP link came up or went away. Each switch starts the decoder fresh.
    pub fn rtc_link(&self, up: bool) {
        if self.rtc.lock().unwrap().is_none() || self.rtc_ready.swap(up, Ordering::SeqCst) == up {
            return;
        }
        log!("Session #{}: media {}", self.id, if up { "on UDP" } else { "back on TCP while UDP is down" });
        self.state.lock().unwrap().needs_keyframe = true;
        host().resend_parameter_sets(self);
        host().request_keyframe();
    }

    pub fn rtc_keyframe_request(&self) {
        host().request_keyframe(); // the phone lost packets that repair could not bring back
    }

    /// Ends UDP for this session for good (fallback is one-way) and tells the phone.
    pub fn rtc_failed(&self, reason: &str) {
        if self.drop_rtc() {
            log!("Session #{}: UDP abandoned ({reason}); media back on TCP", self.id);
            self.control("transport_fallback", None);
            self.state.lock().unwrap().needs_keyframe = true;
            host().resend_parameter_sets(self);
            host().request_keyframe();
        }
    }

    fn check_rtc_stall(&self) {
        if self.media_peer().is_some_and(|p| p.is_stalled()) {
            self.rtc_failed("no frame acknowledgements for 700 ms");
        }
    }

    // MARK: Messages

    pub fn send_session_start(&self, preset: &str, audio: Option<(u32, u16)>, capture_mode: Value) {
        self.control("quality_changed", Some(json!({"preset": preset})));
        self.control("capture_mode_changed", Some(capture_mode));
        if let Some((rate, channels)) = audio {
            self.send_audio_format(rate, channels);
        }
    }

    pub fn send_capture_mode(&self, mode: Value) {
        self.control("capture_mode_changed", Some(mode));
    }

    pub fn send_audio_format(&self, rate: u32, channels: u16) {
        self.control("audio_format_changed", Some(json!({"sampleRate": rate as f64, "channels": channels})));
    }

    pub fn send_quality_changed(&self, preset: &str) {
        self.control("quality_changed", Some(json!({"preset": preset})));
    }
}
