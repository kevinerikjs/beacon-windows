//! Beacon for Windows. Streams this PC's display (or one window) and system audio to Beam on
//! an iPhone or iPad, and replays the phone's taps, keys and controller. Wire-compatible with
//! Beacon for Mac (Phoros over encrypted TCP, media over UDP when the phone takes it), so Beam
//! needs no changes. Lives in the notification area.
#![cfg_attr(not(test), windows_subsystem = "windows")]

/// Logs to the console (when started from one) and to `%APPDATA%\Beacon\beacon.log`.
macro_rules! log {
    ($($t:tt)*) => { $crate::log_line(format!($($t)*)) };
}

mod aac;
mod audio;
mod code_window;
mod controls;
mod desktop;
mod gamepad;
mod input;
mod proto;
mod quality;
mod rtc;
mod session;
mod store;
mod tray;
mod video;

use desktop::Rect;
use serde_json::{json, Value};
use session::Session;
use std::io::Write;
use std::net::TcpListener;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const PORT: u16 = 7979;

/// (wire name, width, height, frame rate, Mbps), from beacon-macos Protocol.swift.
const PRESETS: &[(&str, u32, u32, u32, f64)] = &[
    ("360p30", 640, 360, 30, 1.5),
    ("480p30", 854, 480, 30, 2.5),
    ("720p30", 1280, 720, 30, 4.0),
    ("720p60", 1280, 720, 60, 6.0),
    ("1080p30", 1920, 1080, 30, 6.0),
    ("1080p60", 1920, 1080, 60, 10.0),
    ("1440p30", 2560, 1440, 30, 16.0),
    ("1440p60", 2560, 1440, 60, 24.0),
    ("2160p30", 3840, 2160, 30, 28.0),
    ("2160p60", 3840, 2160, 60, 42.0),
    ("native30", 0, 0, 30, 40.0),
    ("native60", 0, 0, 60, 60.0),
];

pub struct Display {
    pub width: u32,
    pub height: u32,
    pub refresh: u32,
}

struct Pairing {
    code: String,
    device_id: String,
    device_name: String,
    session_id: u64,
    expires: Instant,
}

/// One ffmpeg capture+encode process and what it was started with.
struct Running {
    generation: u64,
    _pipeline: video::Pipeline,
    config: video::Config,
    codec_flags: u8,
    /// The captured region as a fraction of the display, for mapping taps.
    region: [f64; 4],
}

#[derive(Default)]
struct Media {
    current: Option<Running>,
    /// A replacement starting up. It takes over at its first parameter sets, so a change of
    /// quality, window or zoom, or a keyframe request, never leaves a gap in the picture.
    standby: Option<Running>,
    /// `None` is `auto`.
    preset: Option<&'static str>,
    ladder: quality::Ladder,
    parameter_sets: Option<Vec<u8>>,
    /// Video bytes a TCP session may have queued before deltas are skipped.
    budget: usize,
    window: Option<u32>,
    /// Viewport lock (zoom), as a fraction of the source.
    lock: Option<[f64; 4]>,
    audio_stop: Option<mpsc::Sender<()>>,
    audio_format: Option<(u32, u16)>,
    aac: Option<aac::Encoder>,
}

pub struct Host {
    pub name: String,
    pub encoders: video::Encoders,
    pub display: Display,
    start: Instant,
    pub sessions: Mutex<Vec<Arc<Session>>>,
    pairing: Mutex<Option<Pairing>>,
    media: Mutex<Media>,
    /// The pipeline whose output reaches clients.
    generation: AtomicU64,
    next_generation: AtomicU64,
    frame_number: AtomicU32,
    pcm_sequence: AtomicU32,
    aac_sequence: AtomicU32,
}

static HOST: OnceLock<Host> = OnceLock::new();

pub fn host() -> &'static Host {
    HOST.get().expect("host initialised in main")
}


impl Host {
    /// Microseconds on the media clock, shared by video, audio and clock replies.
    pub fn micros(&self) -> i64 {
        self.start.elapsed().as_micros() as i64
    }

    /// Everything this host advertises, for pair_success and auth_success alike.
    pub fn capabilities(&self, kind: &str) -> Value {
        let mut m = json!({
            "type": kind,
            "deviceName": self.name,
            "supportsRemoteAccess": true,
            "supportsVideoHold": true,
            "supportsAudioToggle": true,
            "supportsWindowSelection": true,
            "supportsClockSync": true,
            "supportsPointer": true,
            "supportsEncryption": true,
            "maximumVideoDimension": self.display.width.max(self.display.height),
            "phoneControls": controls::wire(&controls::load()),
        });
        if gamepad::available() {
            m["supportsControllerInput"] = json!(true);
        }
        let tailnet = tailscale_addresses();
        if !tailnet.is_empty() {
            m["tailscaleHosts"] = json!(tailnet);
        }
        m
    }

    // MARK: Pairing

    pub fn begin_pairing(&self, session: &Session, hello: &Value) {
        let Some(device_id) = hello["deviceID"].as_str() else { return };
        let device_name = hello["deviceName"].as_str().unwrap_or(device_id).to_string();
        self.cancel_pairing();
        let code = format!("{:06}", rand::Rng::gen_range(&mut rand::rngs::OsRng, 0..1_000_000));
        log!("{device_name} wants to pair. Type this code on the phone:  {} {}", &code[..3], &code[3..]);
        code_window::show(&device_name, &code);
        *self.pairing.lock().unwrap() = Some(Pairing {
            code,
            device_id: device_id.to_string(),
            device_name,
            session_id: session.id,
            expires: Instant::now() + Duration::from_secs(300),
        });
        // The code goes through the person, never over the wire.
        session.send_json(json!({"type": "challenge", "deviceName": self.name}));
    }

    /// `None`: no attempt open for this session. `Err`: wrong code, the attempt stays open.
    #[allow(clippy::type_complexity)]
    pub fn verify_pairing(&self, session_id: u64, code: &str) -> Option<Result<(String, String, [u8; 32]), ()>> {
        let mut slot = self.pairing.lock().unwrap();
        let p = slot.as_ref().filter(|p| p.session_id == session_id)?;
        if Instant::now() > p.expires {
            drop(slot);
            self.cancel_pairing();
            return None;
        }
        if p.code != code {
            return Some(Err(()));
        }
        let mut secret = [0u8; 32];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut secret);
        let p = slot.take().expect("checked above");
        drop(slot);
        code_window::close();
        Some(Ok((p.device_id, p.device_name, secret)))
    }

    pub fn cancel_pairing(&self) {
        if self.pairing.lock().unwrap().take().is_some() {
            code_window::close();
        }
    }

    /// Forgets every phone and tells the connected ones, which then pair again.
    pub fn forget_all(&self) {
        if let Err(e) = store::forget_all() {
            return log!("Could not remove {}: {e}", store::path().display());
        }
        log!("Forgot every paired device");
        for s in self.authenticated() {
            s.send_json(json!({"type": "unpaired"}));
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(300)); // let the message go out first
                s.close();
            });
        }
    }

    // MARK: Sessions

    pub fn authenticated(&self) -> Vec<Arc<Session>> {
        let all = self.sessions.lock().unwrap().clone();
        all.into_iter().filter(|s| s.state.lock().unwrap().authenticated).collect()
    }

    pub fn session_authenticated(&self, session: &Session) {
        let device = session.state.lock().unwrap().device_id.clone();
        // A device reconnecting while its old session lingers: drop the stale one.
        let stale: Vec<_> = self
            .authenticated()
            .into_iter()
            .filter(|s| s.id != session.id && s.state.lock().unwrap().device_id == device)
            .collect();
        stale.iter().for_each(|s| s.close());
        let (preset, audio, mode) = {
            let m = self.media.lock().unwrap();
            (active_preset(&m), m.audio_format, capture_mode(m.window))
        };
        session.send_session_start(preset, audio, mode);
        // Parameter sets and a keyframe for the newcomer; also picks up its codec and frame rate.
        self.reconfigure(true);
        let mut m = self.media.lock().unwrap();
        if m.audio_stop.is_none() {
            m.audio_stop = Some(start_audio());
        }
    }

    pub fn session_ended(&self, id: u64) {
        self.sessions.lock().unwrap().retain(|s| s.id != id);
        if self.pairing.lock().unwrap().as_ref().is_some_and(|p| p.session_id == id) {
            self.cancel_pairing();
        }
        if self.authenticated().is_empty() {
            let mut m = self.media.lock().unwrap();
            if m.current.is_some() {
                log!("No phone streaming; capture stopped");
            }
            self.generation.store(0, Ordering::SeqCst);
            m.current = None;
            m.standby = None;
            m.parameter_sets = None;
            m.audio_stop = None; // dropping the sender stops audio capture
            m.audio_format = None;
            m.aac = None;
        } else {
            self.reconfigure(false); // codec or frame rate may go up now
        }
    }

    // MARK: Quality, window and zoom

    pub fn set_preset(&self, name: &str) {
        let preset = {
            let mut m = self.media.lock().unwrap();
            if name == "auto" {
                m.preset = None;
            } else {
                let Some(p) = PRESETS.iter().find(|p| p.0 == name) else { return };
                m.preset = Some(p.0);
                m.ladder.set(p.0, Instant::now());
            }
            active_preset(&m)
        };
        self.reconfigure(false);
        self.authenticated().iter().for_each(|s| s.send_quality_changed(preset));
    }

    pub fn quality_feedback(&self, quality: f64) {
        self.media.lock().unwrap().ladder.feedback(quality);
    }

    pub fn window_list(&self) -> Value {
        let windows: Vec<Value> = desktop::list().into_iter().map(|w| json!({"id": w.id, "title": w.title, "app": w.app})).collect();
        json!({"windows": windows})
    }

    pub fn select_window(&self, id: u32) {
        {
            let mut m = self.media.lock().unwrap();
            if id != 0 && desktop::rect(id).is_some() {
                desktop::focus(id);
                m.window = Some(id);
            } else {
                m.window = None;
            }
        }
        self.reconfigure(false);
        self.broadcast_capture_mode();
    }

    fn broadcast_capture_mode(&self) {
        let mode = capture_mode(self.media.lock().unwrap().window);
        self.authenticated().iter().for_each(|s| s.send_capture_mode(mode.clone()));
    }

    pub fn set_viewport_lock(&self, lock: Option<[f64; 4]>) {
        self.media.lock().unwrap().lock = lock;
        self.reconfigure(false);
    }

    /// Twice a second while streaming: follows a moving window and runs the `auto` ladder.
    fn tick(&self, now: Instant, evaluate_ladder: bool) {
        if self.authenticated().is_empty() {
            return;
        }
        let moved_to = {
            let mut m = self.media.lock().unwrap();
            if m.window.is_some_and(|w| desktop::rect(w).is_none()) {
                log!("The streamed window closed or was minimised; back to the whole display");
                m.window = None;
                drop(m);
                self.reconfigure(false);
                return self.broadcast_capture_mode();
            }
            if evaluate_ladder && m.preset.is_none() { m.ladder.evaluate(now) } else { None }
        };
        self.reconfigure(false);
        if let Some(p) = moved_to {
            log!("Auto quality → {p}");
            self.authenticated().iter().for_each(|s| s.send_quality_changed(p));
        }
    }

    // MARK: Video

    /// What capture and encoding should be now, from the preset, the window, the zoom and
    /// every connected phone.
    fn desired(&self, m: &Media, sessions: &[Arc<Session>]) -> Option<(video::Config, u8, [f64; 4])> {
        let &(_, w, h, preset_fps, mbps) = PRESETS.iter().find(|p| p.0 == active_preset(m))?;
        let states: Vec<_> = sessions.iter().map(|s| {
            let st = s.state.lock().unwrap();
            (st.decodes_hevc, st.maximum_frame_rate, st.bitrate_cap)
        }).collect();
        // HEVC only if every client decodes it; H.264 is the original codec.
        let hevc = self.encoders.hevc.filter(|_| states.iter().all(|s| s.0));
        let (encoder, codec) = match (hevc, self.encoders.h264) {
            (Some(e), _) => (e, video::Codec::Hevc),
            (None, Some(e)) => (e, video::Codec::H264),
            (None, None) => return None,
        };
        // A 60 fps preset rises toward the display's refresh when every client asked for more.
        let fps = if preset_fps >= 60 {
            states.iter().map(|s| s.1.map_or(preset_fps, |want| (want.min(self.display.refresh as f64) as u32).max(preset_fps))).min().unwrap_or(preset_fps)
        } else {
            preset_fps
        };
        let bitrate = states.iter().filter_map(|s| s.2).fold((mbps * 1e6) as u64, u64::min);
        let (dw, dh) = (self.display.width as i32, self.display.height as i32);
        let full = Rect { x: 0, y: 0, w: dw, h: dh };
        let source = m.window.and_then(desktop::rect).and_then(|r| desktop::clip(r, dw, dh)).unwrap_or(full);
        let region = m
            .lock
            .and_then(|[x, y, lw, lh]| {
                let r = Rect {
                    x: source.x + (x * source.w as f64) as i32,
                    y: source.y + (y * source.h as f64) as i32,
                    w: (lw * source.w as f64) as i32,
                    h: (lh * source.h as f64) as i32,
                };
                desktop::clip(r, dw, dh)
            })
            .unwrap_or(source);
        let bounds = if w == 0 { (region.w as u32, region.h as u32) } else { (w, h) };
        let (width, height) = fit(region.w as u32, region.h as u32, bounds);
        let config = video::Config {
            encoder,
            width,
            height,
            fps,
            bitrate,
            region: (region != full).then_some((region.x, region.y, region.w, region.h)),
        };
        let fraction = [region.x as f64 / dw as f64, region.y as f64 / dh as f64, region.w as f64 / dw as f64, region.h as f64 / dh as f64];
        Some((config, codec.flags(), fraction))
    }

    /// Brings capture in line with `desired`. With `keyframe`, an unchanged configuration is
    /// restarted anyway, which is how a keyframe is made on demand.
    pub fn reconfigure(&self, keyframe: bool) {
        let sessions = self.authenticated();
        if sessions.is_empty() {
            return;
        }
        let mut m = self.media.lock().unwrap();
        let Some((config, codec_flags, region)) = self.desired(&m, &sessions) else {
            return log!("No hardware video encoder found; cannot stream");
        };
        let target = m.standby.as_ref().or(m.current.as_ref()).map(|r| &r.config);
        if target == Some(&config) && (!keyframe || m.standby.is_some()) {
            return; // already running, or already restarting, as wanted
        }
        let generation = self.next_generation.fetch_add(1, Ordering::SeqCst) + 1;
        if target != Some(&config) {
            let what = if config.region.is_some() { "part of the display" } else { "display" };
            log!("Streaming {} ({what}): {}x{} @ {} fps, {:.1} Mbps, {}", active_preset(&m), config.width, config.height, config.fps, config.bitrate as f64 / 1e6, config.encoder);
        }
        let started = video::Pipeline::start(
            &config,
            move |event| host().on_video(generation, event),
            move |why| host().pipeline_exited(generation, why),
        );
        match started {
            Ok(pipeline) => m.standby = Some(Running { generation, _pipeline: pipeline, config, codec_flags, region }),
            Err(e) => log!("Could not start ffmpeg ({e}). Is ffmpeg on PATH?"),
        }
    }

    pub fn request_keyframe(&self) {
        self.reconfigure(true);
    }

    fn pipeline_exited(&self, generation: u64, why: String) {
        let mut m = self.media.lock().unwrap();
        if m.standby.as_ref().is_some_and(|s| s.generation == generation) {
            m.standby = None;
            return log!("A capture restart failed ({why})");
        }
        if m.current.as_ref().is_some_and(|c| c.generation == generation) {
            m.current = None;
            drop(m);
            log!("Capture stopped unexpectedly ({why}); restarting");
            std::thread::sleep(Duration::from_secs(1));
            self.reconfigure(true);
        }
    }

    fn on_video(&self, generation: u64, event: video::Event) {
        let pts = self.micros();
        let mut m = self.media.lock().unwrap();
        if m.standby.as_ref().is_some_and(|s| s.generation == generation) {
            if !matches!(event, video::Event::ParameterSets(_)) {
                return;
            }
            // The replacement is ready: it takes over, the old ffmpeg is stopped.
            let s = m.standby.take().expect("checked");
            input::set_region(s.region);
            m.budget = ((s.config.bitrate / 8) as usize * 15 / 100).max(256 << 10); // ~150 ms of video
            self.generation.store(generation, Ordering::SeqCst);
            m.current = Some(s);
        }
        // Checked under the lock: output of an ffmpeg being replaced never reaches a client.
        if self.generation.load(Ordering::SeqCst) != generation {
            return;
        }
        let flags = m.current.as_ref().map_or(0, |c| c.codec_flags);
        match event {
            video::Event::ParameterSets(sets) => {
                m.parameter_sets = Some(sets.clone());
                drop(m);
                self.authenticated().iter().for_each(|s| s.send_parameter_sets(&sets, flags));
            }
            video::Event::Frame { annex_b, keyframe } => {
                let n = self.frame_number.fetch_add(1, Ordering::Relaxed);
                let budget = m.budget;
                drop(m);
                self.authenticated().iter().for_each(|s| s.send_video(&annex_b, n, pts, keyframe, flags == 1, budget));
            }
        }
    }

    pub fn resend_parameter_sets(&self, session: &Session) {
        let m = self.media.lock().unwrap();
        if let (Some(sets), Some(c)) = (&m.parameter_sets, &m.current) {
            session.send_parameter_sets(sets, c.codec_flags);
        }
    }
}

fn active_preset(m: &Media) -> &'static str {
    m.preset.unwrap_or_else(|| m.ladder.current())
}

fn capture_mode(window: Option<u32>) -> Value {
    match window.and_then(desktop::info) {
        Some(w) => json!({"windowMode": true, "windowID": w.id, "title": w.title, "app": w.app}),
        None => json!({"windowMode": false}),
    }
}

fn start_audio() -> mpsc::Sender<()> {
    audio::start(
        |f| {
            log!("Audio: {} Hz, {} channel(s)", f.sample_rate, f.channels);
            let h = host();
            h.pcm_sequence.store(0, Ordering::Relaxed);
            {
                let mut m = h.media.lock().unwrap();
                m.audio_format = Some((f.sample_rate, f.channels));
                m.aac = None;
            }
            h.authenticated().iter().for_each(|s| s.send_audio_format(f.sample_rate, f.channels));
        },
        |pcm| {
            let h = host();
            let now = h.micros();
            let listeners: Vec<(Arc<Session>, u8)> = h.authenticated().into_iter().filter_map(|s| {
                let st = s.state.lock().unwrap();
                st.wants_audio.then_some(st.audio_codec)
            }.map(|c| (s, c))).collect();
            if listeners.iter().any(|l| l.1 == 0) {
                let seq = h.pcm_sequence.fetch_add(1, Ordering::Relaxed);
                let payload = proto::audio_payload(seq, now, &pcm);
                listeners.iter().filter(|l| l.1 == 0).for_each(|l| l.0.send_audio(0, &payload));
            }
            let mut m = h.media.lock().unwrap();
            if !listeners.iter().any(|l| l.1 == 1) {
                m.aac = None; // nobody decodes AAC right now
                return;
            }
            let Some((rate, channels)) = m.audio_format else { return };
            if m.aac.is_none() {
                let frames = (pcm.len() / 4 / channels as usize) as i64;
                h.aac_sequence.store(0, Ordering::Relaxed); // sequence restarts with the encoder
                let started = aac::Encoder::start(rate, channels, 128_000, now - frames * 1_000_000 / rate as i64, |au, pts| {
                    let h = host();
                    let payload = proto::audio_payload(h.aac_sequence.fetch_add(1, Ordering::Relaxed), pts, &au);
                    for s in h.authenticated() {
                        s.send_audio(1, &payload);
                    }
                });
                match started {
                    Ok(e) => m.aac = Some(e),
                    Err(e) => return log!("Could not start the AAC encoder ({e})"),
                }
            }
            if m.aac.as_mut().is_some_and(|e| e.feed(&pcm, now).is_err()) {
                m.aac = None; // a long silence or a dead encoder: the next chunk starts a fresh one
            }
        },
    )
}

/// `width`×`height` scaled into `bounds` keeping its aspect, never upscaled, even dimensions.
fn fit(width: u32, height: u32, bounds: (u32, u32)) -> (u32, u32) {
    let scale = (bounds.0 as f64 / width as f64).min(bounds.1 as f64 / height as f64).min(1.0);
    let even = |v: f64| ((v.round() as u32) & !1).max(2);
    (even(width as f64 * scale), even(height as f64 * scale))
}

/// This PC's Tailscale (CGNAT 100.64.0.0/10) addresses, so Beam can reach it remotely.
fn tailscale_addresses() -> Vec<String> {
    if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|i| match i.ip() {
            std::net::IpAddr::V4(v4) if v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1]) => Some(v4.to_string()),
            _ => None,
        })
        .collect()
}

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn primary_display() -> Display {
    use windows_sys::Win32::Graphics::Gdi::*;
    // SAFETY: DEVMODEW is plain data; dmSize tells the API which version we pass.
    unsafe {
        let mut mode: DEVMODEW = std::mem::zeroed();
        mode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
        if EnumDisplaySettingsW(std::ptr::null(), ENUM_CURRENT_SETTINGS, &mut mode) != 0 {
            return Display { width: mode.dmPelsWidth, height: mode.dmPelsHeight, refresh: mode.dmDisplayFrequency.max(30) };
        }
    }
    Display { width: 1920, height: 1080, refresh: 60 }
}

static LOG: Mutex<Option<std::fs::File>> = Mutex::new(None);

pub fn log_path() -> std::path::PathBuf {
    store::path().with_file_name("beacon.log")
}

pub fn log_line(line: String) {
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    // SAFETY: fills a plain struct.
    let t = unsafe {
        let mut t = std::mem::zeroed();
        GetLocalTime(&mut t);
        t
    };
    let stamped = format!("{:02}:{:02}:{:02} {line}", t.wHour, t.wMinute, t.wSecond);
    println!("{stamped}");
    if let Some(f) = LOG.lock().unwrap().as_mut() {
        let _ = writeln!(f, "{stamped}");
    }
}

fn message_box(text: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::*;
    // SAFETY: valid NUL-terminated strings.
    unsafe { MessageBoxW(std::ptr::null_mut(), wide(text).as_ptr(), wide("Beacon").as_ptr(), MB_OK | MB_ICONWARNING) };
}

fn main() {
    // Started from a terminal: print there too. Started from Explorer or at login: tray only.
    // SAFETY: plain call; fails harmlessly without a parent console.
    unsafe { windows_sys::Win32::System::Console::AttachConsole(windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS) };
    if std::env::args().any(|a| a == "--forget-all") {
        match store::forget_all() {
            Ok(()) => println!("Forgot every paired device. Each phone pairs again on its next connection."),
            Err(e) => println!("Could not remove {}: {e}", store::path().display()),
        }
        return;
    }
    if std::env::args().any(|a| a == "--uninstall") {
        // Run by the MSI as it uninstalls: undoes what Beacon created for this Windows user.
        tray::set_login(false);
        let _ = std::fs::remove_dir_all(store::path().parent().expect("has a parent"));
        return;
    }
    desktop::dpi_aware();
    let _ = std::fs::create_dir_all(store::path().parent().expect("has a parent"));
    *LOG.lock().unwrap() = std::fs::File::create(log_path()).ok();

    let name = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "Windows PC".into());
    let display = primary_display();
    log!("Beacon for Windows. Display {}x{} @ {} Hz", display.width, display.height, display.refresh);
    let encoders = video::probe();
    log!("Hardware encoders: H.264 {}, HEVC {}", encoders.h264.unwrap_or("none"), encoders.hevc.unwrap_or("none"));
    if encoders.h264.is_none() {
        message_box("Beacon found no hardware H.264 encoder (NVIDIA, AMD, Intel or Media Foundation), or ffmpeg is not on PATH. Beam cannot be served.");
    }
    log!("Controller passthrough: {}", if gamepad::available() { "on (ViGEmBus)" } else { "off, ViGEmBus is not installed" });
    let _ = HOST.set(Host {
        name: name.clone(),
        encoders,
        display,
        start: Instant::now(),
        sessions: Mutex::default(),
        pairing: Mutex::default(),
        media: Mutex::default(),
        generation: AtomicU64::new(0),
        next_generation: AtomicU64::new(0),
        frame_number: AtomicU32::new(0),
        pcm_sequence: AtomicU32::new(0),
        aac_sequence: AtomicU32::new(0),
    });

    // IPv4 and IPv6 each get a listener: a Windows IPv6 socket is not dual-stack by default,
    // and Bonjour may hand the phone either address.
    let mut listening = false;
    for addr in [format!("0.0.0.0:{PORT}"), format!("[::]:{PORT}")] {
        match TcpListener::bind(&addr) {
            Ok(listener) => {
                listening = true;
                std::thread::spawn(move || {
                    for stream in listener.incoming().flatten() {
                        std::thread::spawn(move || session::run(stream));
                    }
                });
            }
            Err(e) => log!("Could not listen on {addr}: {e}"),
        }
    }
    if !listening {
        log!("Port {PORT} is taken");
        return message_box("Beacon is already running (port 7979 is in use).");
    }

    // "enc=1" tells Beam 3.6+ to open with an encrypted hello, which this host requires.
    let hostname: String = name.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let mdns = mdns_sd::ServiceDaemon::new().and_then(|d| {
        let info = mdns_sd::ServiceInfo::new("_beam._tcp.local.", &name, &format!("{hostname}.local."), "", PORT, &[("enc", "1")][..])?
            .enable_addr_auto();
        d.register(info)?;
        Ok(d)
    });
    match &mdns {
        Ok(_) => log!("Ready. Open Beam on your iPhone or iPad on the same network and pick \"{name}\"."),
        Err(e) => log!("Bonjour advertising failed ({e}); Beam won't find this PC automatically."),
    }
    log!("Paired devices: {}", store::load().len());

    std::thread::spawn(|| {
        let mut ticks = 0u64;
        loop {
            std::thread::sleep(Duration::from_millis(500));
            ticks += 1;
            host().tick(Instant::now(), ticks.is_multiple_of(4)); // the ladder every 2 s, as on the Mac
        }
    });
    tray::run(&name);
}

#[cfg(test)]
mod tests {
    use super::fit;

    #[test]
    fn fit_keeps_aspect_and_never_upscales() {
        assert_eq!(fit(1920, 1080, (1280, 720)), (1280, 720));
        assert_eq!(fit(2560, 1600, (1920, 1080)), (1728, 1080));
        assert_eq!(fit(1920, 1080, (3840, 2160)), (1920, 1080));
        assert_eq!(fit(3440, 1440, (854, 480)), (854, 356));
    }
}
