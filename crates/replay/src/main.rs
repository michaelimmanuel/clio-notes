//! Feeds a recorded meeting fixture through the pipeline.
//!
//! Plays `mic.wav` (as "Me") and `system.wav` (as "Others") from a fixture
//! folder in real time or faster, transcribes them with the speech-to-text
//! profile from Settings, prints each line as it arrives and stores the
//! result in a timeline database.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::Parser;
use pipeline::{PipelineEvent, StreamOptions, transcribe_stream};
use providers::{ProviderClient, Role, SettingsStore, SystemKeyStore, http_client};
use storage::{Speaker, Timeline};
use tokio::sync::mpsc;

#[derive(Parser)]
#[command(about = "Run a recorded meeting fixture through live transcription")]
struct Cli {
    /// Fixture folder made by `record` (holds mic.wav and/or system.wav).
    fixture: PathBuf,
    /// Playback speed: 1 is real time, 4 is four times faster, 0 is as fast
    /// as possible. Latency numbers are only meaningful at 1.
    #[arg(long, default_value_t = 1.0)]
    speed: f64,
    /// Speech-to-text profile to use instead of the one assigned in Settings.
    #[arg(long)]
    profile: Option<String>,
    /// Model to use instead of the profile's, for comparing models on one key.
    #[arg(long)]
    model: Option<String>,
    /// Only play one stream.
    #[arg(long, value_parser = ["mic", "system"])]
    only: Option<String>,
    /// Pause length (ms) that ends an utterance.
    #[arg(long)]
    pause_ms: Option<u64>,
    /// Timeline database to write. Default: <fixture>/timeline.sqlite,
    /// replaced on each run.
    #[arg(long)]
    out: Option<PathBuf>,
}

/// Audio is fed in blocks of this length, like a capture callback.
const BLOCK_MS: u64 = 100;

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<(), String> {
    let store = SettingsStore::default_location().map_err(|e| e.to_string())?;
    let settings = store.load().map_err(|e| e.to_string())?;
    let mut profile = match &cli.profile {
        Some(name) => settings
            .profile(name)
            .ok_or_else(|| format!("No profile named \"{name}\"."))?,
        None => settings.assigned(Role::SpeechToText).ok_or(
            "No profile is assigned to speech to text. Assign one in Settings or pass --profile.",
        )?,
    }
    .clone();
    if let Some(model) = &cli.model {
        profile.model = model.clone();
    }
    let stt = ProviderClient::for_profile(&profile, &SystemKeyStore, &http_client())
        .map_err(|e| e.to_string())?
        .into_speech_to_text()
        .ok_or_else(|| format!("\"{}\" cannot transcribe audio.", profile.name))?;

    let mut streams = Vec::new();
    for (name, speaker) in [("mic", Speaker::Me), ("system", Speaker::Others)] {
        if cli.only.as_deref().is_some_and(|o| o != name) {
            continue;
        }
        let path = cli.fixture.join(format!("{name}.wav"));
        if path.exists() {
            streams.push((path, speaker));
        }
    }
    if streams.is_empty() {
        return Err(format!(
            "No mic.wav or system.wav in {}",
            cli.fixture.display()
        ));
    }

    let db_path = cli
        .out
        .clone()
        .unwrap_or_else(|| cli.fixture.join("timeline.sqlite"));
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", db_path.display()));
    }
    let timeline = Timeline::open(&db_path).map_err(|e| e.to_string())?;
    timeline
        .set_meta("source", &cli.fixture.display().to_string())
        .map_err(|e| e.to_string())?;

    eprintln!(
        "Replaying {} at {} with {} ({})",
        cli.fixture.display(),
        if cli.speed > 0.0 {
            format!("{}x", cli.speed)
        } else {
            "full speed".into()
        },
        profile.name,
        profile.model
    );

    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    let started = Instant::now();
    let mut workers = Vec::new();
    for (path, speaker) in streams {
        let (rate, samples) = read_wav(&path)?;
        eprintln!(
            "  {:<6} {} ({:.1} s)",
            speaker.label(),
            path.display(),
            samples.len() as f64 / rate as f64
        );
        let mut opts = StreamOptions::new(speaker, rate, profile.model.clone());
        if let Some(ms) = cli.pause_ms {
            opts.segment.min_pause_ms = ms;
        }
        let (tx, rx) = mpsc::unbounded_channel();
        workers.push(tokio::spawn(transcribe_stream(
            opts,
            rx,
            stt.clone(),
            events_tx.clone(),
        )));
        let speed = cli.speed;
        workers.push(tokio::spawn(async move {
            let block = (rate as u64 * BLOCK_MS / 1000) as usize;
            for (i, chunk) in samples.chunks(block).enumerate() {
                if speed > 0.0 {
                    // Sleep until this block's end would have been captured.
                    let due =
                        Duration::from_secs_f64((i + 1) as f64 * BLOCK_MS as f64 / 1000.0 / speed);
                    tokio::time::sleep_until((started + due).into()).await;
                }
                if tx.send(chunk.to_vec()).is_err() {
                    break;
                }
            }
        }));
    }
    drop(events_tx);

    let mut stats = Stats::default();
    while let Some(event) = events_rx.recv().await {
        match event {
            PipelineEvent::Detected { .. } => stats.utterances += 1,
            PipelineEvent::Transcribed { line, latency_ms } => {
                println!(
                    "[{:>7.2}s] {:<6} {}   ({:.1} s)",
                    line.start_ms as f64 / 1000.0,
                    line.speaker.label(),
                    line.text,
                    latency_ms as f64 / 1000.0
                );
                stats.audio_ms += line.end_ms - line.start_ms;
                stats.latencies.push(latency_ms);
                timeline.add_transcript(&line).map_err(|e| e.to_string())?;
            }
            PipelineEvent::Empty { .. } => stats.empty += 1,
            PipelineEvent::Failed {
                speaker,
                start_ms,
                message,
                ..
            } => {
                stats.failed += 1;
                println!(
                    "[{:>7.2}s] {:<6} !! {message}",
                    start_ms as f64 / 1000.0,
                    speaker.label()
                );
            }
        }
    }
    for w in workers {
        w.await.map_err(|e| e.to_string())?;
    }

    stats.latencies.sort_unstable();
    let pct = |p: f64| {
        stats
            .latencies
            .get(((stats.latencies.len() as f64 - 1.0) * p).round() as usize)
            .map(|ms| format!("{:.1} s", *ms as f64 / 1000.0))
            .unwrap_or_else(|| "-".into())
    };
    eprintln!(
        "\n{} utterances: {} transcribed, {} empty, {} failed. {:.1} s of audio sent. \
         Request latency: median {}, 90th percentile {}, max {}. Took {:.1} s.",
        stats.utterances,
        stats.latencies.len(),
        stats.empty,
        stats.failed,
        stats.audio_ms as f64 / 1000.0,
        pct(0.5),
        pct(0.9),
        pct(1.0),
        started.elapsed().as_secs_f64()
    );
    eprintln!("Timeline: {}", db_path.display());
    Ok(())
}

#[derive(Default)]
struct Stats {
    utterances: usize,
    empty: usize,
    failed: usize,
    audio_ms: u64,
    latencies: Vec<u64>,
}

/// Reads a WAV file as mono f32, averaging channels.
fn read_wav(path: &Path) -> Result<(u32, Vec<f32>), String> {
    let mut r =
        hound::WavReader::open(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let spec = r.spec();
    let channels = spec.channels.max(1) as usize;
    let raw: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => r.samples::<f32>().collect::<Result<_, _>>(),
        hound::SampleFormat::Int => {
            let scale = 1.0 / (1u64 << (spec.bits_per_sample - 1)) as f32;
            r.samples::<i32>()
                .map(|s| s.map(|s| s as f32 * scale))
                .collect::<Result<_, _>>()
        }
    }
    .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let mono = raw
        .chunks(channels)
        .map(|f| f.iter().sum::<f32>() / channels as f32)
        .collect();
    Ok((spec.sample_rate, mono))
}
