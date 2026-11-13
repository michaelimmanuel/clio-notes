//! Microphone and system-audio capture to mono 16-bit WAV. System audio is
//! WASAPI loopback on Windows, a Core Audio process tap on macOS 14.6+ and a
//! PulseAudio/PipeWire monitor source on Linux.
//!
//! Each recording is aligned to the [`SessionClock`]: sample 0 of the WAV file
//! is session time 0. Silence is inserted wherever the device delivered no
//! data, which matters for loopback capture because Windows sends no packets
//! at all while nothing is playing.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample};
use serde::Serialize;

use crate::clock::SessionClock;

/// A gap shorter than this is treated as normal callback jitter, not missing audio.
const GAP_TOLERANCE: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioSource {
    /// An input device, normally the user's microphone.
    Microphone,
    /// Loopback capture of an output device: what the user hears.
    SystemAudio,
}

#[derive(Debug, Clone, Serialize)]
pub struct AudioDeviceInfo {
    pub name: String,
    pub is_default: bool,
}

/// Lists microphones and output devices (usable for system-audio capture).
pub fn list_devices() -> Result<(Vec<AudioDeviceInfo>, Vec<AudioDeviceInfo>)> {
    let host = cpal::default_host();
    let default_in = host.default_input_device().map(|d| d.to_string());
    let default_out = host.default_output_device().map(|d| d.to_string());
    let describe = |devices: Vec<cpal::Device>, default: &Option<String>| {
        devices
            .into_iter()
            .map(|d| {
                let name = d.to_string();
                AudioDeviceInfo {
                    is_default: default.as_deref() == Some(name.as_str()),
                    name,
                }
            })
            .collect()
    };
    // Linux lists each speaker's monitor among the inputs; it is not a microphone.
    let inputs = describe(
        host.input_devices()?.filter(|d| !is_monitor(d)).collect(),
        &default_in,
    );
    let outputs = describe(host.output_devices()?.collect(), &default_out);
    Ok((inputs, outputs))
}

/// Summary of one finished audio recording.
#[derive(Debug, Clone, Serialize)]
pub struct AudioStats {
    pub source: AudioSource,
    pub device: String,
    pub file: PathBuf,
    pub sample_rate: u32,
    pub device_channels: u16,
    pub device_sample_format: String,
    /// Length of the WAV file.
    pub duration_ms: u64,
    /// Silence inserted where the device delivered nothing.
    pub padded_ms: u64,
    /// Session time of the first real audio packet, if any arrived.
    pub first_packet_ms: Option<u64>,
    /// At the last packet: file position minus session time. Positive means the
    /// device clock ran fast. Gaps longer than 100 ms are padded, so a slow
    /// device shows at most about -100 ms here.
    pub drift_ms: i64,
    /// Loudest sample in dBFS, or `None` if every sample was exactly zero.
    /// A microphone with `None` is muted, disconnected or a virtual device
    /// whose app is not running.
    pub peak_dbfs: Option<f64>,
    /// Times WASAPI flagged a data discontinuity. Loopback raises this each
    /// time sound resumes after silence, so a few per minute are normal there.
    pub discontinuities: u64,
    pub stream_errors: u64,
}

impl AudioStats {
    /// True if the device delivered audio but every sample was zero.
    pub fn is_digital_silence(&self) -> bool {
        self.first_packet_ms.is_some() && self.peak_dbfs.is_none()
    }
}

struct Chunk {
    /// When the callback ran; the chunk ends at roughly this instant.
    received: Instant,
    samples: Vec<f32>,
}

/// Receives every block of mono samples exactly as it is written to the WAV
/// file, gap padding included, so sample index `n` is session time
/// `n / sample_rate`. Called on the writer thread, never in the audio callback.
pub type AudioTap = Box<dyn FnMut(&[f32]) + Send>;

/// A running audio capture. Call [`AudioRecorder::stop`] to finish the file.
pub struct AudioRecorder {
    stream: cpal::Stream,
    writer: JoinHandle<Result<WriterStats>>,
    stop_tx: Sender<Instant>,
    counters: Arc<Counters>,
    info: StreamInfo,
}

#[derive(Default)]
struct Counters {
    errors: AtomicU64,
    discontinuities: AtomicU64,
}

struct StreamInfo {
    source: AudioSource,
    device: String,
    file: PathBuf,
    sample_rate: u32,
    device_channels: u16,
    device_sample_format: String,
}

impl AudioRecorder {
    /// Starts recording `source` into a WAV file at `path`.
    ///
    /// `device` is a case-insensitive substring of the device name; `None`
    /// picks the system default microphone or speaker.
    pub fn start(
        source: AudioSource,
        device: Option<&str>,
        path: &Path,
        clock: SessionClock,
    ) -> Result<Self> {
        Self::start_with_tap(source, device, path, clock, None)
    }

    /// Like [`AudioRecorder::start`], and also hands the written samples to `tap`.
    pub fn start_with_tap(
        source: AudioSource,
        device: Option<&str>,
        path: &Path,
        clock: SessionClock,
        tap: Option<AudioTap>,
    ) -> Result<Self> {
        let host = cpal::default_host();
        let device = find_device(&host, source, device)?;
        let device_name = device.to_string();
        #[cfg(target_os = "linux")]
        let device = match source {
            AudioSource::Microphone => device,
            AudioSource::SystemAudio => monitor_of(&host, &device)?,
        };

        // On Windows and macOS, loopback uses the output device's mix format;
        // cpal turns an input stream on an output device into a loopback
        // stream (WASAPI loopback, or a Core Audio process tap on macOS 14.6+).
        // On Linux the device is already the sink's monitor source.
        let config = match source {
            AudioSource::Microphone => device.default_input_config(),
            AudioSource::SystemAudio if cfg!(target_os = "linux") => device.default_input_config(),
            AudioSource::SystemAudio => device.default_output_config(),
        }
        .with_context(|| format!("no usable format for {device_name}"))?;

        let sample_rate = config.sample_rate();
        let channels = config.channels();
        let sample_format = config.sample_format();

        let spec = hound::WavSpec {
            channels: 1,
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let wav = hound::WavWriter::create(path, spec)
            .with_context(|| format!("cannot create {}", path.display()))?;

        let (chunk_tx, chunk_rx) = mpsc::channel::<Chunk>();
        let (stop_tx, stop_rx) = mpsc::channel::<Instant>();
        let counters = Arc::new(Counters::default());

        let writer = std::thread::Builder::new()
            .name(format!("{source:?}-writer"))
            .spawn(move || write_loop(wav, sample_rate, clock, chunk_rx, stop_rx, tap))?;

        let stream_config: cpal::StreamConfig = config.into();
        let c = counters.clone();
        let stream = match sample_format {
            SampleFormat::F32 => build::<f32>(&device, stream_config, chunk_tx, c),
            SampleFormat::I16 => build::<i16>(&device, stream_config, chunk_tx, c),
            SampleFormat::I32 => build::<i32>(&device, stream_config, chunk_tx, c),
            SampleFormat::U16 => build::<u16>(&device, stream_config, chunk_tx, c),
            other => Err(anyhow!("unsupported sample format {other}")),
        }
        .with_context(|| format!("cannot open {source:?} stream on {device_name}"))?;
        stream.play().context("cannot start audio stream")?;

        Ok(Self {
            stream,
            writer,
            stop_tx,
            counters,
            info: StreamInfo {
                source,
                device: device_name,
                file: path.to_path_buf(),
                sample_rate,
                device_channels: channels,
                device_sample_format: sample_format.to_string(),
            },
        })
    }

    /// The device's sample rate, which is also the WAV file's and the tap's.
    pub fn sample_rate(&self) -> u32 {
        self.info.sample_rate
    }

    pub fn device_name(&self) -> &str {
        &self.info.device
    }

    /// Stops the stream, pads the file to the stop time and finalizes it.
    pub fn stop(self) -> Result<AudioStats> {
        let stopped = Instant::now();
        // Dropping the stream drops the callback and its sender, which ends the
        // writer's chunk loop once the queued chunks are written.
        drop(self.stream);
        let _ = self.stop_tx.send(stopped);
        let w = self
            .writer
            .join()
            .map_err(|_| anyhow!("audio writer thread panicked"))??;

        let rate = self.info.sample_rate as f64;
        let to_ms = |samples: u64| (samples as f64 * 1000.0 / rate).round() as u64;
        Ok(AudioStats {
            source: self.info.source,
            device: self.info.device,
            file: self.info.file,
            sample_rate: self.info.sample_rate,
            device_channels: self.info.device_channels,
            device_sample_format: self.info.device_sample_format,
            duration_ms: to_ms(w.written),
            padded_ms: to_ms(w.padded),
            first_packet_ms: w.first_packet_ms,
            drift_ms: w.drift_ms,
            peak_dbfs: (w.peak > 0.0)
                .then(|| (20.0 * (w.peak as f64).log10() * 10.0).round() / 10.0),
            discontinuities: self.counters.discontinuities.load(Ordering::Relaxed),
            stream_errors: self.counters.errors.load(Ordering::Relaxed),
        })
    }
}

fn find_device(
    host: &cpal::Host,
    source: AudioSource,
    wanted: Option<&str>,
) -> Result<cpal::Device> {
    let Some(wanted) = wanted else {
        return match source {
            AudioSource::Microphone => host.default_input_device(),
            AudioSource::SystemAudio => host.default_output_device(),
        }
        .ok_or_else(|| anyhow!("no default {} device", kind(source)));
    };
    let devices: Vec<cpal::Device> = match source {
        AudioSource::Microphone => host.input_devices()?.collect(),
        AudioSource::SystemAudio => host.output_devices()?.collect(),
    };
    let needle = wanted.to_lowercase();
    // An exact name wins, so "Mic" can be chosen even when "Mic 2" exists.
    if let Some(i) = devices
        .iter()
        .position(|d| d.to_string().to_lowercase() == needle)
    {
        return Ok(devices.into_iter().nth(i).unwrap());
    }
    let mut matches: Vec<_> = devices
        .into_iter()
        .filter(|d| d.to_string().to_lowercase().contains(&needle))
        .collect();
    match matches.len() {
        0 => bail!("no {} device matches \"{wanted}\"", kind(source)),
        1 => Ok(matches.remove(0)),
        _ => {
            let names: Vec<_> = matches.iter().map(|d| d.to_string()).collect();
            bail!("\"{wanted}\" matches several devices: {}", names.join(", "))
        }
    }
}

/// True for a PulseAudio monitor source ("<sink>.monitor"): the sound going to
/// a speaker, not a microphone. Always false outside Linux.
fn is_monitor(device: &cpal::Device) -> bool {
    cfg!(target_os = "linux")
        && device
            .id()
            .map(|id| id.id().ends_with(".monitor"))
            .unwrap_or(false)
}

/// On Linux the sound sent to a sink is recorded from its monitor source.
/// This needs the PulseAudio host, which PipeWire also serves through
/// pipewire-pulse; plain ALSA has no monitor sources.
#[cfg(target_os = "linux")]
fn monitor_of(host: &cpal::Host, sink: &cpal::Device) -> Result<cpal::Device> {
    if host.id() != cpal::HostId::PulseAudio {
        bail!(
            "recording the computer's sound on Linux needs PulseAudio or PipeWire \
             (with pipewire-pulse); only ALSA was found"
        );
    }
    let wanted = format!("{}.monitor", sink.id()?.id());
    host.input_devices()?
        .find(|d| d.id().map(|id| id.id() == wanted).unwrap_or(false))
        .ok_or_else(|| anyhow!("{sink} has no monitor source to record from"))
}

fn kind(source: AudioSource) -> &'static str {
    match source {
        AudioSource::Microphone => "microphone",
        AudioSource::SystemAudio => "output",
    }
}

/// Builds an input stream that downmixes each callback to mono f32 and hands it
/// to the writer thread. No I/O or locking happens in the audio callback.
fn build<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    tx: Sender<Chunk>,
    counters: Arc<Counters>,
) -> Result<cpal::Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let channels = config.channels.max(1) as usize;
    let stream = device.build_input_stream::<T, _, _>(
        config,
        move |data: &[T], _| {
            let received = Instant::now();
            let samples = data
                .chunks(channels)
                .map(|frame| {
                    frame.iter().map(|&s| f32::from_sample(s)).sum::<f32>() / channels as f32
                })
                .collect();
            // The receiver only goes away after the stream is dropped.
            let _ = tx.send(Chunk { received, samples });
        },
        move |err| {
            if err.kind() == cpal::ErrorKind::Xrun {
                // WASAPI's discontinuity flag; the writer's gap padding keeps
                // the timeline aligned, so only count it.
                counters.discontinuities.fetch_add(1, Ordering::Relaxed);
            } else {
                counters.errors.fetch_add(1, Ordering::Relaxed);
                log::warn!("audio stream error: {err}");
            }
        },
        None,
    )?;
    Ok(stream)
}

/// How often the WAV file is flushed to disk while recording.
const FLUSH_EVERY: std::time::Duration = std::time::Duration::from_secs(5);

struct WriterStats {
    written: u64,
    padded: u64,
    first_packet_ms: Option<u64>,
    drift_ms: i64,
    peak: f32,
}

fn write_loop(
    mut wav: hound::WavWriter<std::io::BufWriter<std::fs::File>>,
    sample_rate: u32,
    clock: SessionClock,
    chunks: Receiver<Chunk>,
    stop: Receiver<Instant>,
    mut tap: Option<AudioTap>,
) -> Result<WriterStats> {
    let mut timeline = Timeline::new(sample_rate);
    let mut first_packet_ms = None;
    let mut drift_ms = 0;
    let mut peak = 0f32;
    let mut flushed = Instant::now();

    for chunk in chunks {
        let end = clock.offset_of(chunk.received);
        first_packet_ms.get_or_insert(end.as_millis() as u64);
        let pad = timeline.pad_before(end, chunk.samples.len() as u64);
        for _ in 0..pad {
            wav.write_sample(0i16)?;
        }
        let mut block = Vec::new();
        if let Some(tap) = tap.as_mut() {
            block.reserve(pad as usize + chunk.samples.len());
            block.resize(pad as usize, 0.0);
            block.extend(chunk.samples.iter().map(|s| s.clamp(-1.0, 1.0)));
            tap(&block);
        }
        for s in chunk.samples {
            let s = s.clamp(-1.0, 1.0);
            peak = peak.max(s.abs());
            wav.write_sample((s * i16::MAX as f32) as i16)?;
        }
        drift_ms = timeline.offset_ms(end);
        // Writes the buffered samples and the header's lengths, so a crash
        // leaves a playable file missing at most the last few seconds.
        if flushed.elapsed() >= FLUSH_EVERY {
            wav.flush()?;
            flushed = Instant::now();
        }
    }

    // Loopback goes quiet when nothing plays, so pad the tail up to the stop time.
    let stopped = stop.recv().unwrap_or_else(|_| Instant::now());
    let pad = timeline.pad_to(clock.offset_of(stopped));
    for _ in 0..pad {
        wav.write_sample(0i16)?;
    }
    if let Some(tap) = tap.as_mut().filter(|_| pad > 0) {
        tap(&vec![0.0; pad as usize]);
    }
    wav.finalize()?;

    Ok(WriterStats {
        written: timeline.written,
        padded: timeline.padded,
        first_packet_ms,
        drift_ms,
        peak,
    })
}

/// Tracks how many samples are in the file and decides how much silence to
/// insert so that sample index stays equal to session time.
struct Timeline {
    rate: u64,
    written: u64,
    padded: u64,
}

impl Timeline {
    fn new(rate: u32) -> Self {
        Self {
            rate: rate as u64,
            written: 0,
            padded: 0,
        }
    }

    /// Whole samples in `d`, in integer math so 2.01 s is exactly 2010 samples at 1 kHz.
    fn samples_in(&self, d: Duration) -> u64 {
        (d.as_nanos() * self.rate as u128 / 1_000_000_000) as u64
    }

    /// A chunk of `len` samples ending at session time `end` is about to be
    /// written. Returns how many zero samples to write first, and counts both.
    ///
    /// The first chunk is aligned exactly to session time. After that, gaps
    /// below [`GAP_TOLERANCE`] are callback jitter and are not padded.
    fn pad_before(&mut self, end: Duration, len: u64) -> u64 {
        let start_sample = self.samples_in(end).saturating_sub(len);
        let tolerance = if self.written == 0 {
            0
        } else {
            self.samples_in(GAP_TOLERANCE)
        };
        let pad = if start_sample > self.written + tolerance {
            start_sample - self.written
        } else {
            0
        };
        self.padded += pad;
        self.written += pad + len;
        pad
    }

    /// Pads exactly up to session time `end`. Used once, at stop.
    fn pad_to(&mut self, end: Duration) -> u64 {
        let pad = self.samples_in(end).saturating_sub(self.written);
        self.padded += pad;
        self.written += pad;
        pad
    }

    /// File position minus session time `at`, in ms.
    fn offset_ms(&self, at: Duration) -> i64 {
        let diff = self.written as i128 - self.samples_in(at) as i128;
        (diff * 1000 / self.rate as i128) as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 1000; // 1 sample per ms keeps the arithmetic readable

    #[test]
    fn steady_stream_gets_no_padding() {
        let mut t = Timeline::new(RATE);
        for i in 1..=100 {
            assert_eq!(t.pad_before(Duration::from_millis(i * 10), 10), 0);
        }
        assert_eq!(t.written, 1000);
    }

    #[test]
    fn late_first_packet_is_aligned_to_session_start() {
        let mut t = Timeline::new(RATE);
        // First 10 ms of audio arrives 500 ms into the session.
        assert_eq!(t.pad_before(Duration::from_millis(500), 10), 490);
        assert_eq!(t.written, 500);
    }

    #[test]
    fn small_first_packet_offset_is_still_aligned() {
        let mut t = Timeline::new(RATE);
        // 60 ms in, below the jitter tolerance: must still be padded exactly.
        assert_eq!(t.pad_before(Duration::from_millis(60), 10), 50);
        assert_eq!(t.offset_ms(Duration::from_millis(60)), 0);
    }

    #[test]
    fn drift_is_measured_at_last_packet() {
        let mut t = Timeline::new(RATE);
        t.pad_before(Duration::from_millis(10), 10);
        // Device delivers 40 ms of audio in 30 ms of session time: a fast clock.
        t.pad_before(Duration::from_millis(40), 40);
        assert_eq!(t.offset_ms(Duration::from_millis(40)), 10);
    }

    #[test]
    fn silence_gap_is_filled() {
        let mut t = Timeline::new(RATE);
        t.pad_before(Duration::from_millis(10), 10);
        // Nothing for two seconds, as with loopback while nothing plays.
        assert_eq!(t.pad_before(Duration::from_millis(2010), 10), 1990);
        assert_eq!(t.written, 2010);
    }

    #[test]
    fn jitter_below_tolerance_is_ignored() {
        let mut t = Timeline::new(RATE);
        t.pad_before(Duration::from_millis(10), 10);
        assert_eq!(t.pad_before(Duration::from_millis(80), 10), 0);
    }

    #[test]
    fn tail_is_padded_to_stop_time() {
        let mut t = Timeline::new(RATE);
        t.pad_before(Duration::from_millis(10), 10);
        assert_eq!(t.pad_to(Duration::from_millis(3000)), 2990);
        assert_eq!(t.written, 3000);
        // A short tail gap is padded exactly too.
        assert_eq!(t.pad_to(Duration::from_millis(3050)), 50);
    }
}
