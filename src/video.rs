//! Screen capture and hardware encoding: ffmpeg's Desktop Duplication source (`ddagrab`)
//! into the GPU's H.264/HEVC encoder (NVENC, AMF, Quick Sync or Media Foundation).
//!
//! ffmpeg writes FLV to a pipe because FLV gives each frame a length up front: a raw Annex B
//! pipe only shows where a frame ends when the next one begins, which costs a frame of latency.
//! FLV also carries the parameter sets once, out of band, the way VideoToolbox does on the Mac.

use std::io::{self, Read};
use std::process::{Child, Command, Stdio};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Codec {
    H264,
    Hevc,
}

impl Codec {
    pub fn flags(self) -> u8 {
        match self {
            Codec::H264 => 0,
            Codec::Hevc => 1,
        }
    }
}

pub struct Encoders {
    pub h264: Option<&'static str>,
    pub hevc: Option<&'static str>,
}

/// Hardware encoders only, as on the Mac: a software encoder cannot keep up with a live
/// screen and would hide a missing encoder behind a slow one.
pub fn probe() -> Encoders {
    let first = |list: &[&'static str]| list.iter().copied().find(|e| works(e));
    Encoders {
        h264: first(&["h264_nvenc", "h264_amf", "h264_qsv", "h264_mf"]),
        hevc: first(&["hevc_nvenc", "hevc_amf", "hevc_qsv", "hevc_mf"]),
    }
}

fn works(encoder: &str) -> bool {
    let mut cmd = ffmpeg();
    cmd.args(["-f", "lavfi", "-i", "color=size=320x240:rate=30", "-frames:v", "3", "-c:v", encoder])
        .args(encoder_args(encoder))
        .args(["-f", "null", "-"]);
    cmd.status().map(|s| s.success()).unwrap_or(false)
}

fn ffmpeg() -> Command {
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"]).stdin(Stdio::null()).stdout(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd
}

/// Low-latency settings per vendor: no lookahead, no B-frames, constant bitrate.
fn encoder_args(encoder: &str) -> Vec<&'static str> {
    let mut args = vec!["-bf", "0"];
    if encoder.ends_with("_nvenc") {
        args.extend(["-preset", "p1", "-tune", "ull", "-zerolatency", "1", "-delay", "0", "-rc", "cbr"]);
    } else if encoder.ends_with("_amf") {
        args.extend(["-usage", "ultralowlatency", "-rc", "cbr"]);
    } else if encoder.ends_with("_qsv") {
        args.extend(["-preset", "veryfast", "-async_depth", "1"]);
    } else if encoder.ends_with("_mf") {
        args.extend(["-rate_control", "cbr", "-scenario", "display_remoting", "-hw_encoding", "1"]);
    }
    if !encoder.ends_with("_mf") {
        args.extend(["-profile:v", if encoder.starts_with("hevc") { "main" } else { "high" }]);
    }
    args
}

#[derive(Clone, PartialEq, Debug)]
pub struct Config {
    pub encoder: &'static str,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u64,
    /// Part of the display to capture (x, y, w, h in pixels), or the whole display.
    pub region: Option<(i32, i32, i32, i32)>,
}

pub enum Event {
    /// Annex B parameter sets (SPS+PPS, or VPS+SPS+PPS).
    ParameterSets(Vec<u8>),
    /// One Annex B access unit.
    Frame { annex_b: Vec<u8>, keyframe: bool },
}

pub struct Pipeline {
    child: Child,
}

impl Pipeline {
    /// Starts capture+encode. `on_event` runs on a reader thread; `on_exit` runs once when
    /// ffmpeg stops for any reason, including `Drop`.
    pub fn start(
        cfg: &Config,
        mut on_event: impl FnMut(Event) + Send + 'static,
        on_exit: impl FnOnce(String) + Send + 'static,
    ) -> io::Result<Pipeline> {
        let filter = format!("hwdownload,format=bgra,scale={}:{}:flags=bilinear,format=nv12", cfg.width, cfg.height);
        let (bitrate, maxrate, bufsize) = (cfg.bitrate.to_string(), (cfg.bitrate * 2).to_string(), (cfg.bitrate / 4).to_string());
        // A keyframe every 5 s, as the Mac ships. Keyframes on demand come from Host starting a
        // replacement pipeline, whose first frame is one.
        // shortcut: a replacement takes ~0.7 s to produce it (the Mac: one frame); drive NVENC/MF directly to fix.
        let gop = (cfg.fps * 5).to_string();
        let region = cfg.region.map_or(String::new(), |(x, y, w, h)| format!(":offset_x={x}:offset_y={y}:video_size={w}x{h}"));
        let mut cmd = ffmpeg();
        cmd.args(["-fflags", "nobuffer", "-probesize", "32", "-analyzeduration", "0", "-f", "lavfi", "-i"])
            .arg(format!("ddagrab=output_idx=0:framerate={}:draw_mouse=1{region}", cfg.fps))
            .args(["-vf", &filter, "-c:v", cfg.encoder])
            .args(encoder_args(cfg.encoder))
            .args(["-b:v", &bitrate, "-maxrate", &maxrate, "-bufsize", &bufsize, "-g", &gop])
            .args(["-flush_packets", "1", "-f", "flv", "-"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn()?;
        let stdout = child.stdout.take().expect("piped");
        let mut stderr = child.stderr.take().expect("piped");
        std::thread::spawn(move || {
            let result = read_flv(io::BufReader::with_capacity(1 << 20, stdout), &mut on_event);
            let mut err = String::new();
            let _ = stderr.read_to_string(&mut err);
            on_exit(match result {
                Ok(()) => err.trim().to_string(),
                Err(e) => format!("{e}; {}", err.trim()),
            });
        });
        Ok(Pipeline { child })
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Reads an FLV stream and reports its video as Phoros units. Returns at end of stream.
pub fn read_flv(mut r: impl Read, on_event: &mut impl FnMut(Event)) -> io::Result<()> {
    let mut header = [0u8; 13]; // 9-byte header + PreviousTagSize0
    r.read_exact(&mut header)?;
    if &header[..3] != b"FLV" {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not FLV"));
    }
    let mut length_size = 4;
    loop {
        let mut tag = [0u8; 11];
        match r.read_exact(&mut tag) {
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            other => other?,
        }
        let size = u32::from_be_bytes([0, tag[1], tag[2], tag[3]]) as usize;
        let mut data = vec![0u8; size + 4]; // + PreviousTagSize
        r.read_exact(&mut data)?;
        data.truncate(size);
        if tag[0] == 9 {
            if let Some(event) = video_tag(&data, &mut length_size) {
                on_event(event);
            }
        }
    }
}

fn video_tag(d: &[u8], length_size: &mut usize) -> Option<Event> {
    let first = *d.first()?;
    // Enhanced FLV (HEVC): ExHeader bit, frame type, packet type, then a FourCC.
    let (keyframe, packet_type, body) = if first & 0x80 != 0 {
        let packet_type = first & 0x0F;
        let body = match packet_type {
            0 | 3 => d.get(5..)?, // sequence start, coded frames without composition time
            1 => d.get(8..)?,     // coded frames with a 3-byte composition time
            _ => return None,
        };
        ((first >> 4) & 7 == 1, if packet_type == 0 { 0 } else { 1 }, body)
    } else {
        (first >> 4 == 1, *d.get(1)?, d.get(5..)?)
    };
    if packet_type == 0 {
        let hevc = first & 0x80 != 0;
        let (sets, size) = if hevc { hvcc(body)? } else { avcc(body)? };
        *length_size = size;
        return Some(Event::ParameterSets(sets));
    }
    if packet_type != 1 {
        return None;
    }
    let mut annex_b = Vec::with_capacity(body.len() + 16);
    let mut i = 0;
    while i + *length_size <= body.len() {
        let n = body[i..i + *length_size].iter().fold(0usize, |a, &b| (a << 8) | b as usize);
        i += *length_size;
        annex_b.extend_from_slice(&[0, 0, 0, 1]);
        annex_b.extend_from_slice(body.get(i..i + n)?);
        i += n;
    }
    Some(Event::Frame { annex_b, keyframe })
}

fn push_nal(out: &mut Vec<u8>, d: &[u8], i: &mut usize, keep: bool) -> Option<()> {
    let n = u16::from_be_bytes([*d.get(*i)?, *d.get(*i + 1)?]) as usize;
    let nal = d.get(*i + 2..*i + 2 + n)?;
    if keep {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal);
    }
    *i += 2 + n;
    Some(())
}

/// AVCDecoderConfigurationRecord → Annex B SPS+PPS and the NAL length size.
fn avcc(d: &[u8]) -> Option<(Vec<u8>, usize)> {
    let length_size = (*d.get(4)? & 3) as usize + 1;
    let mut out = Vec::new();
    let mut i = 6;
    for _ in 0..(*d.get(5)? & 0x1F) {
        push_nal(&mut out, d, &mut i, true)?;
    }
    let pps = *d.get(i)?;
    i += 1;
    for _ in 0..pps {
        push_nal(&mut out, d, &mut i, true)?;
    }
    Some((out, length_size))
}

/// HEVCDecoderConfigurationRecord → Annex B VPS+SPS+PPS and the NAL length size. Other
/// arrays (x265 stores an SEI there) stay out: the receiver builds its decoder from these.
fn hvcc(d: &[u8]) -> Option<(Vec<u8>, usize)> {
    let length_size = (*d.get(21)? & 3) as usize + 1;
    let mut out = Vec::new();
    let mut i = 23;
    for _ in 0..*d.get(22)? {
        let keep = matches!(*d.get(i)? & 0x3F, 32..=34);
        let count = u16::from_be_bytes([*d.get(i + 1)?, *d.get(i + 2)?]);
        i += 3;
        for _ in 0..count {
            push_nal(&mut out, d, &mut i, keep)?;
        }
    }
    Some((out, length_size))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encodes a test pattern with ffmpeg's software encoders (always present in the builds
    /// this needs) and checks the FLV reader recovers parameter sets, frames and keyframes.
    fn round_trip(encoder: &str, nal_types: fn(&[u8]) -> Vec<u8>, expected_sets: &[u8]) {
        let out = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i", "testsrc=size=320x240:rate=30"])
            .args(["-frames:v", "10", "-c:v", encoder, "-bf", "0", "-g", "5", "-f", "flv", "-"])
            .output()
            .expect("ffmpeg on PATH");
        let (mut sets, mut frames) = (Vec::new(), Vec::new());
        read_flv(&out.stdout[..], &mut |e| match e {
            Event::ParameterSets(s) => sets.push(s),
            Event::Frame { annex_b, keyframe } => frames.push((annex_b, keyframe)),
        })
        .unwrap();
        assert_eq!(sets.len(), 1);
        assert_eq!(nal_types(&sets[0]), expected_sets);
        assert_eq!(frames.len(), 10);
        let keys: Vec<usize> = frames.iter().enumerate().filter(|f| f.1 .1).map(|f| f.0).collect();
        assert_eq!(keys, [0, 5]);
        assert!(frames.iter().all(|f| f.0.starts_with(&[0, 0, 0, 1])));
    }

    fn types(annex_b: &[u8], shift: u8, mask: u8) -> Vec<u8> {
        annex_b.windows(5).filter(|w| w[..4] == [0, 0, 0, 1]).map(|w| (w[4] >> shift) & mask).collect()
    }

    #[test]
    fn flv_h264() {
        round_trip("libx264", |a| types(a, 0, 0x1F), &[7, 8]);
    }

    #[test]
    fn flv_hevc() {
        round_trip("libx265", |a| types(a, 1, 0x3F), &[32, 33, 34]);
    }
}
