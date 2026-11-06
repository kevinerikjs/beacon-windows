//! System audio: WASAPI loopback of the default output device (cpal opens an input stream on
//! an output device as loopback). Sent as interleaved Float32 PCM, which every Beam decodes.
//! shortcut: PCM only (~3 Mbps at 48 kHz stereo); add AAC-LC via Media Foundation if remote links need it.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;
use std::sync::mpsc;

pub struct Format {
    pub sample_rate: u32,
    pub channels: u16,
}

/// Runs capture on its own thread until the returned sender is dropped. `on_format` runs
/// once before the first chunk; `on_chunk` gets little-endian f32 bytes, at most stereo.
pub fn start(
    on_format: impl FnOnce(Format) + Send + 'static,
    on_chunk: impl FnMut(Vec<u8>) + Send + 'static,
) -> mpsc::Sender<()> {
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        if let Err(e) = run(stop_rx, on_format, on_chunk) {
            log!("audio: {e}");
        }
    });
    stop_tx
}

fn run(
    stop: mpsc::Receiver<()>,
    on_format: impl FnOnce(Format),
    mut on_chunk: impl FnMut(Vec<u8>) + Send + 'static,
) -> Result<(), Box<dyn std::error::Error>> {
    let device = cpal::default_host().default_output_device().ok_or("no output device")?;
    let config = device.default_output_config()?;
    let channels = config.channels() as usize;
    let out_channels = channels.min(2); // a 5.1 device sends its front pair
    let rate = config.sample_rate().0;
    let factor = decimation(rate);
    on_format(Format { sample_rate: rate / factor as u32, channels: out_channels as u16 });
    let mut acc = vec![0f32; out_channels];
    let mut n = 0usize;
    let err = |e| log!("audio stream: {e}");
    let mut emit = move |samples: &mut dyn Iterator<Item = f32>| {
        let mut bytes = Vec::with_capacity(4096);
        for (i, s) in samples.enumerate() {
            let c = i % channels;
            if c < out_channels {
                acc[c] += s;
            }
            if c == channels - 1 {
                n += 1;
                if n == factor {
                    for a in acc.iter_mut() {
                        bytes.extend_from_slice(&(*a / factor as f32).to_le_bytes());
                        *a = 0.0;
                    }
                    n = 0;
                }
            }
        }
        if !bytes.is_empty() {
            on_chunk(bytes);
        }
    };
    let stream = match config.sample_format() {
        SampleFormat::F32 => device.build_input_stream(&config.into(), move |d: &[f32], _: &_| emit(&mut d.iter().copied()), err, None)?,
        SampleFormat::I16 => device.build_input_stream(&config.into(), move |d: &[i16], _: &_| emit(&mut d.iter().map(|&s| s as f32 / 32768.0)), err, None)?,
        SampleFormat::I32 => device.build_input_stream(&config.into(), move |d: &[i32], _: &_| emit(&mut d.iter().map(|&s| s as f32 / 2_147_483_648.0)), err, None)?,
        other => return Err(format!("unsupported sample format {other:?}").into()),
    };
    stream.play()?;
    let _ = stop.recv(); // returns when the sender is dropped
    Ok(())
}

/// Beam plays what the Mac sends, 44.1 or 48 kHz. A 96/192 kHz device is averaged down by
/// a whole factor. shortcut: box filter, no proper low-pass; fine for listening, swap for a real resampler if needed.
fn decimation(rate: u32) -> usize {
    [48_000, 44_100].iter().find(|&&base| rate > base && rate.is_multiple_of(base)).map_or(1, |&base| (rate / base) as usize)
}

#[cfg(test)]
mod tests {
    #[test]
    fn decimation_targets_48k_or_44k() {
        assert_eq!(super::decimation(192_000), 4);
        assert_eq!(super::decimation(96_000), 2);
        assert_eq!(super::decimation(88_200), 2);
        assert_eq!(super::decimation(48_000), 1);
        assert_eq!(super::decimation(44_100), 1);
    }
}
