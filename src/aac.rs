//! AAC-LC for phones that list `aac_lc`, with Windows' own Media Foundation AAC encoder in
//! process (an ffmpeg pipe held ~150 ms of audio in its buffers). One raw access unit per
//! 1024 frames. Timestamps follow a sample clock anchored to the media clock: WASAPI loopback
//! delivers nothing while nothing plays, so gaps are filled with silence to keep it exact.

use std::sync::Once;
use windows::core::{Error, Result};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};

/// CLSID_AACMFTEncoder, the Microsoft AAC encoder MFT (not exported by the windows crate).
const AAC_ENCODER: windows::core::GUID = windows::core::GUID::from_u128(0x93AF0C51_2275_45D2_A35B_F2BA21CAED00);
const FRAMES_PER_UNIT: i64 = 1024;
/// Encoder priming plus decoder look-ahead, as a decoder that trims nothing (Beam's) sees it.
/// Each unit is stamped this much earlier, as PhorosMedia.AACEncoder does on the Mac (2112 for
/// Apple's encoder). Microsoft's encoder already aligns its output: measured 0 by the test.
const PRIMING: i64 = 0;
/// A gap longer than this restarts the encoder instead of feeding seconds of silence.
const MAX_FILL_MICROS: i64 = 2_000_000;
/// Units over this are skipped: one unit must fit one packet (AudioCodecID.maxAccessUnitBytes).
const MAX_UNIT: usize = 1536;

pub struct Encoder {
    mft: IMFTransform,
    rate: i64,
    channels: usize,
    anchor: i64,
    fed: i64,
    units: i64,
    on_unit: Box<dyn FnMut(Vec<u8>, i64) + Send>,
}

// SAFETY: Media Foundation transforms are free-threaded, used from one thread at a time here.
unsafe impl Send for Encoder {}

impl Encoder {
    /// `rate` 44100 or 48000, `channels` 1 or 2, `bitrate` one of 96/128/160/192 kbps.
    /// `anchor` is the media-clock time of the first sample fed.
    pub fn start(rate: u32, channels: u16, bitrate: u32, anchor: i64, on_unit: impl FnMut(Vec<u8>, i64) + Send + 'static) -> Result<Encoder> {
        static STARTUP: Once = Once::new();
        // SAFETY: COM and MF initialisation, then plain COM calls on the objects created here.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            STARTUP.call_once(|| {
                let _ = MFStartup(MF_VERSION, MFSTARTUP_LITE);
            });
            let mft: IMFTransform = CoCreateInstance(&AAC_ENCODER, None, CLSCTX_INPROC_SERVER)?;
            let audio = |subtype: &windows::core::GUID| -> Result<IMFMediaType> {
                let t = MFCreateMediaType()?;
                t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
                t.SetGUID(&MF_MT_SUBTYPE, subtype)?;
                t.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
                t.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, rate)?;
                t.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels as u32)?;
                Ok(t)
            };
            // The AAC encoder takes its output type first.
            let output = audio(&MFAudioFormat_AAC)?;
            output.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, bitrate / 8)?;
            output.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)?; // raw access units, no ADTS
            mft.SetOutputType(0, &output, 0)?;
            let input = audio(&MFAudioFormat_PCM)?;
            input.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, channels as u32 * 2)?;
            input.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, rate * channels as u32 * 2)?;
            mft.SetInputType(0, &input, 0)?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            Ok(Encoder { mft, rate: rate as i64, channels: channels as usize, anchor, fed: 0, units: 0, on_unit: Box::new(on_unit) })
        }
    }

    /// Feeds one chunk of interleaved little-endian f32 that ended at media-clock `now`.
    /// `Err` means the gap was too long or the encoder failed: drop it and start a fresh one.
    pub fn feed(&mut self, pcm: &[u8], now: i64) -> Result<()> {
        let frames = (pcm.len() / 4 / self.channels) as i64;
        let gap = now - frames * 1_000_000 / self.rate - (self.anchor + self.fed * 1_000_000 / self.rate);
        if gap > MAX_FILL_MICROS {
            return Err(Error::empty());
        }
        let mut samples: Vec<u8> = Vec::with_capacity(pcm.len() / 2 + 64);
        if gap > 50_000 {
            let silence = (gap * self.rate / 1_000_000) as usize * self.channels;
            samples.resize(silence * 2, 0);
        }
        for s in pcm.chunks_exact(4) {
            let v = (f32::from_le_bytes([s[0], s[1], s[2], s[3]]).clamp(-1.0, 1.0) * 32767.0) as i16;
            samples.extend_from_slice(&v.to_le_bytes());
        }
        // SAFETY: plain MF calls; the buffer is locked only while copied into.
        unsafe {
            let buffer = MFCreateMemoryBuffer(samples.len() as u32)?;
            let mut data = std::ptr::null_mut();
            buffer.Lock(&mut data, None, None)?;
            std::ptr::copy_nonoverlapping(samples.as_ptr(), data, samples.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(samples.len() as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(self.fed * 10_000_000 / self.rate)?;
            self.fed += (samples.len() / 2 / self.channels) as i64;
            self.mft.ProcessInput(0, &sample, 0)?;
            self.drain()
        }
    }

    unsafe fn drain(&mut self) -> Result<()> {
        loop {
            let buffer = MFCreateMemoryBuffer(4096)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            let mut out = [MFT_OUTPUT_DATA_BUFFER { dwStreamID: 0, pSample: std::mem::ManuallyDrop::new(Some(sample)), dwStatus: 0, pEvents: std::mem::ManuallyDrop::new(None) }];
            let mut status = 0;
            let result = self.mft.ProcessOutput(0, &mut out, &mut status);
            std::mem::ManuallyDrop::drop(&mut out[0].pSample);
            std::mem::ManuallyDrop::drop(&mut out[0].pEvents);
            match result {
                Ok(()) => {
                    let mut data = std::ptr::null_mut();
                    let mut len = 0u32;
                    buffer.Lock(&mut data, None, Some(&mut len))?;
                    let unit = std::slice::from_raw_parts(data, len as usize).to_vec();
                    buffer.Unlock()?;
                    let pts = self.anchor + (self.units * FRAMES_PER_UNIT - PRIMING) * 1_000_000 / self.rate;
                    self.units += 1;
                    if !unit.is_empty() && unit.len() <= MAX_UNIT {
                        (self.on_unit)(unit, pts);
                    }
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                Err(e) => return Err(e),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn adts(au: &[u8], rate_index: u8, channels: u8) -> Vec<u8> {
        let len = au.len() + 7;
        let mut h = vec![0xFF, 0xF1, (1 << 6) | (rate_index << 2) | (channels >> 2), ((channels & 3) << 6) | (len >> 11) as u8, (len >> 3) as u8, ((len & 7) << 5) as u8 | 0x1F, 0xFC];
        h.extend_from_slice(au);
        h
    }

    /// Encodes a click at a known sample, decodes it with ffmpeg (which, like Beam's decoder,
    /// trims nothing), and checks the click lands PRIMING samples late, so stamping each unit
    /// PRIMING earlier puts it back on time.
    #[test]
    fn priming_is_measured_and_units_are_timed() {
        let units = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = units.clone();
        let mut enc = Encoder::start(48_000, 2, 128_000, 1_000_000, move |au, pts| sink.lock().unwrap().push((au, pts))).unwrap();
        let click_at = 10_000usize;
        let mut pcm = vec![0f32; 48_000 * 2];
        for i in 0..32 {
            pcm[(click_at + i) * 2] = 0.9;
            pcm[(click_at + i) * 2 + 1] = 0.9;
        }
        let bytes: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
        enc.feed(&bytes, 2_000_000).unwrap(); // one second that ended at 2 s: no gap
        let units = units.lock().unwrap().clone();
        assert!(units.len() >= 44, "about 47 units per second, got {}", units.len());
        assert!((21_333..=21_334).contains(&(units[1].1 - units[0].1)));
        assert_eq!(units[0].1, 1_000_000 - PRIMING * 1_000_000 / 48_000);

        let stream: Vec<u8> = units.iter().flat_map(|(au, _)| adts(au, 3, 2)).collect(); // 3 = 48 kHz
        let mut child = std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-f", "aac", "-i", "-", "-f", "f32le", "-ac", "1", "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("ffmpeg on PATH");
        child.stdin.take().unwrap().write_all(&stream).unwrap();
        let decoded: Vec<f32> = child.wait_with_output().unwrap().stdout.chunks_exact(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect();
        // The peak, not the first loud sample: AAC smears a click's pre-echo ahead of it.
        let peak = (0..decoded.len()).max_by(|&a, &b| decoded[a].abs().total_cmp(&decoded[b].abs())).expect("audio decodes");
        let measured = peak as i64 - (click_at as i64 + 16);
        assert!((measured - PRIMING).abs() <= 32, "priming measured {measured}, constant {PRIMING}");
    }
}
