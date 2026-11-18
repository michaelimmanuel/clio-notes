//! Compressing a meeting's recording once the meeting is over.
//!
//! While a meeting runs, its audio is written as WAV, which is flushed every
//! few seconds and so survives a crash. Once the meeting has stopped, each
//! WAV is encoded as Opus in an Ogg file next to it (`mic.wav` becomes
//! `mic.opus`), the new file is decoded again to check it, and only then is
//! the WAV deleted. At [`BITRATE`], an hour of one track takes about 14 MB
//! instead of about 350 MB.
//!
//! An interruption at any point leaves a usable folder: the new file is
//! written under a temporary name and renamed once it is complete and
//! checked, and the WAV is kept until then. A WAV still next to its `.opus`
//! (the app stopped between the rename and the delete) is simply encoded
//! again.

use std::fmt::Display;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};

use opus_pure::{
    Application, MAX_PACKET_BYTES, MAX_PACKET_SAMPLES, OggOpusReader, OggOpusWriter, OpusEncoder,
    OpusHead, RateControl, Trim,
};

use crate::resample::Resampler;

/// The recordings a meeting folder holds, by file stem.
pub const RECORDINGS: [&str; 2] = ["mic", "system"];
/// Average bits per second of the compressed audio: speech stays clear, and
/// a video or music played in the meeting stays recognisable.
pub const BITRATE: i32 = 32_000;
/// Sample rates Opus encodes directly. Others are resampled to 48 kHz.
const OPUS_RATES: [u32; 5] = [8_000, 12_000, 16_000, 24_000, 48_000];
/// Mono samples read from the WAV before they are encoded.
const READ_BLOCK: usize = 4_800;

/// What compressing one recording did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compressed {
    /// The new file's name, such as `mic.opus`.
    pub file: String,
    pub wav_bytes: u64,
    pub opus_bytes: u64,
    pub duration_ms: u64,
}

/// The WAV recordings in `dir` that are not compressed yet.
pub fn uncompressed(dir: &Path) -> Vec<PathBuf> {
    RECORDINGS
        .iter()
        .map(|stem| dir.join(format!("{stem}.wav")))
        .filter(|path| path.is_file())
        .collect()
}

/// Compresses every WAV recording in `dir`, stopping at the first that
/// fails. A recording that fails keeps its WAV.
pub fn compress_meeting(dir: &Path) -> Result<Vec<Compressed>, String> {
    uncompressed(dir)
        .iter()
        .map(|wav| compress(wav).map_err(|e| format!("{}: {e}", file_name(wav))))
        .collect()
}

/// Encodes `wav` as Opus in a file next to it with the extension `.opus`,
/// checks that the new file decodes to as many samples as went in, and
/// deletes the WAV.
pub fn compress(wav: &Path) -> Result<Compressed, String> {
    let opus = wav.with_extension("opus");
    let part = wav.with_extension("opus.part");
    let wav_bytes = fs::metadata(wav).map_err(text)?.len();
    let checked = encode(wav, &part).and_then(|(rate, samples)| {
        let decoded = decode(&part, rate, |_| {})?;
        if decoded == samples {
            Ok((rate, samples))
        } else {
            Err(format!(
                "the compressed file decodes to {decoded} samples instead of {samples}"
            ))
        }
    });
    let (rate, samples) =
        match checked.and_then(|r| fs::rename(&part, &opus).map(|_| r).map_err(text)) {
            Ok(r) => r,
            Err(e) => {
                let _ = fs::remove_file(&part);
                return Err(e);
            }
        };
    fs::remove_file(wav)
        .map_err(|e| format!("compressed, but the WAV could not be deleted: {e}"))?;
    Ok(Compressed {
        file: file_name(&opus),
        wav_bytes,
        opus_bytes: fs::metadata(&opus).map_err(text)?.len(),
        duration_ms: samples * 1000 / rate as u64,
    })
}

/// Encodes the WAV at `wav` into an Ogg Opus file at `out`, mixed down to
/// mono. Returns the rate the audio was encoded at and how many samples of
/// audio, at that rate, the file holds.
fn encode(wav: &Path, out: &Path) -> Result<(u32, u64), String> {
    let mut reader = hound::WavReader::open(wav).map_err(text)?;
    let spec = reader.spec();
    let channels = usize::from(spec.channels.max(1));
    let rate = if OPUS_RATES.contains(&spec.sample_rate) {
        spec.sample_rate
    } else {
        48_000
    };

    let mut encoder = OpusEncoder::new(rate as i32, 1, Application::Audio).map_err(text)?;
    encoder.bitrate_bps = BITRATE;
    // Nothing downstream meters the rate of a file.
    encoder.rate_control = RateControl::Vbr;
    if spec.sample_format == hound::SampleFormat::Int {
        encoder.lsb_depth = i32::from(spec.bits_per_sample).clamp(8, 24);
    }
    let head = OpusHead::for_encoder(&encoder, spec.sample_rate);
    let pre_skip = u64::from(head.pre_skip);
    let writer =
        OggOpusWriter::new(BufWriter::new(File::create(out).map_err(text)?), head).map_err(text)?;
    let mut ogg = OggEncoder {
        encoder,
        writer,
        resampler: (rate != spec.sample_rate).then(|| Resampler::new(spec.sample_rate, rate)),
        frame: rate as usize / 50,
        ticks: u64::from(48_000 / rate),
        pre_skip,
        pending: Vec::new(),
        packet: vec![0; MAX_PACKET_BYTES],
        samples: 0,
    };

    // Mixes the channels of each sample frame into one sample.
    let mut block = Vec::with_capacity(READ_BLOCK);
    let mut sum = 0.0f32;
    let mut count = 0;
    let mut add = |sample: f32, ogg: &mut OggEncoder| -> Result<(), String> {
        sum += sample;
        count += 1;
        if count == channels {
            block.push(sum / channels as f32);
            sum = 0.0;
            count = 0;
            if block.len() == READ_BLOCK {
                ogg.push(&block)?;
                block.clear();
            }
        }
        Ok(())
    };
    match spec.sample_format {
        hound::SampleFormat::Int => {
            let scale = 1.0 / (1u64 << (spec.bits_per_sample.clamp(1, 32) - 1)) as f32;
            for sample in reader.samples::<i32>() {
                add(sample.map_err(text)? as f32 * scale, &mut ogg)?;
            }
        }
        hound::SampleFormat::Float => {
            for sample in reader.samples::<f32>() {
                add(sample.map_err(text)?, &mut ogg)?;
            }
        }
    }
    ogg.push(&block)?;
    let samples = ogg.finish()?;
    Ok((rate, samples))
}

/// Opus encoder writing into an Ogg file, fed mono audio in blocks of any
/// size.
struct OggEncoder {
    encoder: OpusEncoder,
    writer: OggOpusWriter<BufWriter<File>>,
    /// Brings the WAV's rate to one Opus encodes, when it is not one.
    resampler: Option<Resampler>,
    /// Samples per packet: 20 ms.
    frame: usize,
    /// Ogg granule ticks (48 kHz) per sample.
    ticks: u64,
    /// Samples, in ticks, by which the encoder's output lags its input.
    pre_skip: u64,
    /// Samples waiting for a whole frame.
    pending: Vec<f32>,
    packet: Vec<u8>,
    /// Samples of audio taken so far, at the encoder's rate.
    samples: u64,
}

impl OggEncoder {
    fn push(&mut self, input: &[f32]) -> Result<(), String> {
        match self.resampler.as_mut().map(|r| r.push(input)) {
            Some(resampled) => self.take(&resampled),
            None => self.take(input),
        }
    }

    /// Encodes every whole frame of `samples` and what was pending.
    fn take(&mut self, samples: &[f32]) -> Result<(), String> {
        self.samples += samples.len() as u64;
        self.pending.extend_from_slice(samples);
        let whole = self.pending.len() / self.frame * self.frame;
        for start in (0..whole).step_by(self.frame) {
            let n = self
                .encoder
                .encode(
                    &self.pending[start..start + self.frame],
                    self.frame,
                    &mut self.packet,
                )
                .map_err(text)?;
            self.writer.write_packet(&self.packet[..n]).map_err(text)?;
        }
        self.pending.drain(..whole);
        Ok(())
    }

    /// Encodes the rest and closes the file, so that it decodes to exactly
    /// the audio that went in (RFC 7845 §4.4). Returns the samples of audio.
    fn finish(mut self) -> Result<u64, String> {
        if let Some(mut resampler) = self.resampler.take() {
            let rest = resampler.finish();
            self.take(&rest)?;
        }
        // The encoder still holds its last `pre_skip` of audio: silence
        // pushes it out, rounded up to whole frames. The last packet says
        // where the audio ends, so the silence is not played.
        let end = self.pre_skip + self.samples * self.ticks;
        let flush = self.pre_skip.div_ceil(self.ticks) as usize;
        let len = (self.pending.len() + flush).div_ceil(self.frame) * self.frame;
        self.pending.resize(len, 0.0);
        let frames = len / self.frame;
        for i in 0..frames {
            let start = i * self.frame;
            let n = self
                .encoder
                .encode(
                    &self.pending[start..start + self.frame],
                    self.frame,
                    &mut self.packet,
                )
                .map_err(text)?;
            if i + 1 == frames {
                let duration = end.saturating_sub(self.writer.granule().max(0) as u64);
                self.writer
                    .write_packet_with_duration(&self.packet[..n], duration as u32)
                    .map_err(text)?;
            } else {
                self.writer.write_packet(&self.packet[..n]).map_err(text)?;
            }
        }
        let file = self
            .writer
            .finish()
            .map_err(text)?
            .into_inner()
            .map_err(|e| e.error().to_string())?;
        // On disk before the WAV is deleted.
        file.sync_all().map_err(text)?;
        Ok(self.samples)
    }
}

/// Decodes the Ogg Opus file at `path` at `rate`, handing `keep` each block
/// of audio (the encoder's delay and the end's padding left out). Returns
/// the samples per channel decoded.
fn decode(path: &Path, rate: u32, mut keep: impl FnMut(&[f32])) -> Result<u64, String> {
    let mut reader =
        OggOpusReader::new(BufReader::new(File::open(path).map_err(text)?)).map_err(text)?;
    let channels = usize::from(reader.head().channel_count.max(1));
    let mut decoder = reader.head().decoder(rate as i32).map_err(text)?;
    let mut trim = Trim::new(reader.head(), rate as i32, channels).map_err(text)?;
    let mut block = vec![0.0f32; MAX_PACKET_SAMPLES * channels];
    let mut decoded = 0;
    for packet in reader.packets() {
        let packet = packet.map_err(text)?;
        let n = decoder
            .decode(&packet.data, MAX_PACKET_SAMPLES, &mut block)
            .map_err(text)?;
        let audio = trim.keep(&packet, &block[..n * channels]);
        decoded += (audio.len() / channels) as u64;
        keep(audio);
    }
    Ok(decoded)
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn text(e: impl Display) -> String {
    e.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "clio-recording-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A tone that changes pitch and loudness, like a voice more than a
    /// constant tone does.
    fn voice_like(rate: u32, seconds: f32) -> Vec<f32> {
        let n = (rate as f32 * seconds) as usize;
        (0..n)
            .map(|i| {
                let t = i as f32 / rate as f32;
                let pitch = 180.0 + 60.0 * (2.0 * PI * 0.7 * t).sin();
                let loudness = 0.25 + 0.2 * (2.0 * PI * 3.0 * t).sin();
                loudness * ((2.0 * PI * pitch * t).sin() + 0.4 * (2.0 * PI * 2.0 * pitch * t).sin())
                    / 1.4
            })
            .collect()
    }

    fn write_wav(path: &Path, rate: u32, channels: u16, mono: &[f32]) {
        let spec = hound::WavSpec {
            channels,
            sample_rate: rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for &s in mono {
            for _ in 0..channels {
                w.write_sample((s * 32767.0) as i16).unwrap();
            }
        }
        w.finalize().unwrap();
    }

    fn decoded(path: &Path, rate: u32) -> Vec<f32> {
        let mut out = Vec::new();
        decode(path, rate, |audio| out.extend_from_slice(audio)).unwrap();
        out
    }

    /// Signal-to-noise ratio of `got` against `want`, in dB.
    fn snr(want: &[f32], got: &[f32]) -> f32 {
        let signal: f32 = want.iter().map(|s| s * s).sum();
        let noise: f32 = want.iter().zip(got).map(|(a, b)| (a - b) * (a - b)).sum();
        10.0 * (signal / noise).log10()
    }

    #[test]
    fn a_recording_becomes_a_much_smaller_file_with_the_same_audio() {
        let dir = temp_dir("48k");
        let audio = voice_like(48_000, 4.0);
        write_wav(&dir.join("mic.wav"), 48_000, 1, &audio);

        let done = compress_meeting(&dir).unwrap();

        assert_eq!(done.len(), 1);
        let c = &done[0];
        assert_eq!(c.file, "mic.opus");
        assert_eq!(c.duration_ms, 4_000);
        assert!(!dir.join("mic.wav").exists());
        assert!(!dir.join("mic.opus.part").exists());
        assert!(
            c.opus_bytes * 15 < c.wav_bytes,
            "{} bytes from {}",
            c.opus_bytes,
            c.wav_bytes
        );
        let back = decoded(&dir.join("mic.opus"), 48_000);
        assert_eq!(back.len(), audio.len());
        let snr = snr(&audio, &back);
        assert!(snr > 10.0, "SNR {snr:.1} dB");
        assert!(uncompressed(&dir).is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn other_rates_and_channels_are_resampled_and_mixed_down() {
        let dir = temp_dir("44k");
        let audio = voice_like(44_100, 2.0);
        write_wav(&dir.join("system.wav"), 44_100, 2, &audio);

        let c = compress(&dir.join("system.wav")).unwrap();

        assert_eq!(c.file, "system.opus");
        let back = decoded(&dir.join("system.opus"), 48_000);
        let expected = 2 * 48_000;
        assert!(
            back.len().abs_diff(expected) <= 2,
            "{} samples, expected about {expected}",
            back.len()
        );
        assert!(c.duration_ms.abs_diff(2_000) <= 1, "{} ms", c.duration_ms);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_empty_recording_becomes_an_empty_file() {
        let dir = temp_dir("empty");
        write_wav(&dir.join("mic.wav"), 48_000, 1, &[]);

        let c = compress(&dir.join("mic.wav")).unwrap();

        assert_eq!(c.duration_ms, 0);
        assert!(decoded(&dir.join("mic.opus"), 48_000).is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_wav_left_next_to_its_opus_is_compressed_again() {
        let dir = temp_dir("again");
        let audio = voice_like(16_000, 1.0);
        write_wav(&dir.join("mic.wav"), 16_000, 1, &audio);
        fs::write(dir.join("mic.opus"), b"left from before").unwrap();
        fs::write(dir.join("mic.opus.part"), b"half written").unwrap();

        compress_meeting(&dir).unwrap();

        assert_eq!(decoded(&dir.join("mic.opus"), 16_000).len(), audio.len());
        assert!(!dir.join("mic.wav").exists());
        assert!(!dir.join("mic.opus.part").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_wav_that_cannot_be_read_is_kept() {
        let dir = temp_dir("bad");
        fs::write(dir.join("mic.wav"), b"not a wav file").unwrap();

        let e = compress_meeting(&dir).unwrap_err();

        assert!(e.starts_with("mic.wav: "), "{e}");
        assert!(dir.join("mic.wav").exists());
        assert!(!dir.join("mic.opus").exists());
        assert!(!dir.join("mic.opus.part").exists());
        fs::remove_dir_all(&dir).unwrap();
    }
}
