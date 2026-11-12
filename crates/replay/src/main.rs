//! Feeds a recorded meeting fixture through the pipeline.
//!
//! Plays `mic.wav` (as "Me"), `system.wav` (as "Others") and the screen
//! frames from a fixture folder in real time or faster. Audio is transcribed
//! with the speech-to-text profile from Settings; frames go through keyframe
//! selection and screen reading, and both feed the live notes. Prints
//! events as they arrive and stores the result in a timeline database, with
//! keyframe images and `notes-live.md` next to it.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use pipeline::notes::{self, section_markdown};
use pipeline::screen::{self, SkipReason};
use pipeline::{
    Frame, NotesEvent, NotesInput, PipelineEvent, ScreenEvent, ScreenMode, ScreenOptions,
    SectionConfig, StreamOptions, notes_stream, screen_stream, transcribe_stream,
};
use providers::{
    ProviderClient, Role, Settings, SettingsStore, SystemKeyStore, TextGenerator, http_client,
};
use serde::Deserialize;
use storage::{Speaker, Timeline};
use tokio::sync::mpsc;

#[derive(Parser)]
#[command(about = "Run a recorded meeting fixture through the live pipeline")]
struct Cli {
    /// Fixture folder made by `record` (mic.wav, system.wav, frames.jsonl).
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
    #[arg(long, value_parser = ["mic", "system", "screen"])]
    only: Option<String>,
    /// Pause length (ms) that ends an utterance.
    #[arg(long)]
    pause_ms: Option<u64>,
    /// Keyframe selection: "auto" (slides and demos) or "slides".
    #[arg(long, default_value = "auto", value_parser = ["auto", "slides"])]
    screen_mode: String,
    /// Pick keyframes but do not send them to the screen-reading model.
    #[arg(long)]
    no_read: bool,
    /// Most screen readings for the run.
    #[arg(long, default_value_t = 60)]
    read_cap: u32,
    /// Screen-reading profile to use instead of the one assigned in Settings.
    #[arg(long)]
    vision_profile: Option<String>,
    /// Do not write notes.
    #[arg(long)]
    no_notes: bool,
    /// Text-generation profile to use instead of the one assigned in Settings.
    #[arg(long)]
    text_profile: Option<String>,
    /// Longest notes section, in seconds, when the screen does not change.
    #[arg(long)]
    section_max_s: Option<u64>,
    /// Timeline database to write. Default: <fixture>/timeline.sqlite,
    /// replaced on each run, with keyframe images in a `keyframes` folder
    /// next to it.
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
    let wants = |name: &str| cli.only.as_deref().is_none_or(|o| o == name);

    let mut streams = Vec::new();
    for (name, speaker) in [("mic", Speaker::Me), ("system", Speaker::Others)] {
        let path = cli.fixture.join(format!("{name}.wav"));
        if wants(name) && path.exists() {
            streams.push((path, speaker));
        }
    }
    let frames = if wants("screen") {
        read_frame_index(&cli.fixture)?
    } else {
        Vec::new()
    };
    if streams.is_empty() && frames.is_empty() {
        return Err(format!(
            "Nothing to play in {}: no mic.wav, system.wav or frames.jsonl",
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
    let out_dir = match db_path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    if !frames.is_empty() {
        let _ = std::fs::remove_dir_all(out_dir.join("keyframes"));
    }
    let notes_path = out_dir.join("notes-live.md");
    let _ = std::fs::remove_file(&notes_path);
    let timeline = Timeline::open(&db_path).map_err(|e| e.to_string())?;
    timeline
        .set_meta("source", &cli.fixture.display().to_string())
        .map_err(|e| e.to_string())?;

    eprintln!(
        "Replaying {} at {}",
        cli.fixture.display(),
        if cli.speed > 0.0 {
            format!("{}x", cli.speed)
        } else {
            "full speed".into()
        },
    );

    let started = Instant::now();
    let mut workers = Vec::new();

    let (speech_tx, mut speech_rx) = mpsc::unbounded_channel();
    if !streams.is_empty() {
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
        eprintln!("  speech: {} ({})", profile.name, profile.model);

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
                speech_tx.clone(),
            )));
            let speed = cli.speed;
            workers.push(tokio::spawn(async move {
                let block = (rate as u64 * BLOCK_MS / 1000) as usize;
                for (i, chunk) in samples.chunks(block).enumerate() {
                    if speed > 0.0 {
                        // Sleep until this block's end would have been captured.
                        let due = Duration::from_secs_f64(
                            (i + 1) as f64 * BLOCK_MS as f64 / 1000.0 / speed,
                        );
                        tokio::time::sleep_until((started + due).into()).await;
                    }
                    if tx.send(chunk.to_vec()).is_err() {
                        break;
                    }
                }
            }));
        }
    }
    drop(speech_tx);

    let (screen_tx, mut screen_rx) = mpsc::unbounded_channel();
    if !frames.is_empty() {
        let reader = if cli.no_read {
            None
        } else {
            generator(&settings, cli.vision_profile.as_deref(), Role::Vision)?
        };
        let mode = if cli.screen_mode == "slides" {
            ScreenMode::Slides
        } else {
            ScreenMode::Auto
        };
        let reading = match (&reader, cli.no_read) {
            (_, true) => "not reading".to_string(),
            (Some(r), _) => format!(
                "reading with {} ({}), cap {}",
                r.name, r.model, cli.read_cap
            ),
            (None, _) => "not reading: no screen-reading profile assigned".to_string(),
        };
        eprintln!(
            "  Screen {} frames, {} mode, {reading}",
            frames.len(),
            cli.screen_mode
        );
        let opts = ScreenOptions::new(out_dir.clone(), mode, Some(cli.read_cap));
        let (tx, rx) = mpsc::channel(2);
        workers.push(tokio::spawn(screen_stream(
            opts,
            rx,
            reader.map(|r| r.generator),
            screen_tx,
        )));
        let (fixture, speed) = (cli.fixture.clone(), cli.speed);
        workers.push(tokio::spawn(async move {
            for f in frames {
                if speed > 0.0 {
                    let due = Duration::from_secs_f64(f.t_ms as f64 / 1000.0 / speed);
                    tokio::time::sleep_until((started + due).into()).await;
                }
                let path = fixture.join(&f.file);
                let image = match tokio::task::spawn_blocking(move || image::open(&path)).await {
                    Ok(Ok(img)) => img.into_rgba8(),
                    Ok(Err(e)) => {
                        eprintln!("cannot read {}: {e}", f.file);
                        continue;
                    }
                    Err(_) => continue,
                };
                // Waits rather than drops, so every replay sees every frame.
                if tx
                    .send(Frame {
                        t_ms: f.t_ms,
                        image,
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }));
    } else {
        drop(screen_tx);
    }

    // Notes are fed by both streams and end after both have ended.
    let (notes_in_tx, notes_in_rx) = mpsc::unbounded_channel();
    let (notes_tx, mut notes_rx) = mpsc::unbounded_channel();
    let writer = if cli.no_notes {
        None
    } else {
        generator(&settings, cli.text_profile.as_deref(), Role::Text)?
    };
    match &writer {
        Some(w) => eprintln!("  Notes  writing with {} ({})", w.name, w.model),
        None if cli.no_notes => eprintln!("  Notes  off"),
        None => eprintln!("  Notes  off: no text-generation profile assigned"),
    }
    let mut notes_in = None;
    if let Some(w) = writer {
        let mut cfg = SectionConfig::default();
        if let Some(s) = cli.section_max_s {
            cfg.max_ms = s * 1000;
            cfg.min_ms = cfg.min_ms.min(cfg.max_ms);
        }
        workers.push(tokio::spawn(notes_stream(
            cfg,
            notes_in_rx,
            w.generator,
            notes_tx,
        )));
        if cli.speed > 0.0 {
            // The session clock, so sections are written while playback runs.
            // It stops by itself once the notes input is closed.
            let (ticks, speed) = (notes_in_tx.downgrade(), cli.speed);
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    let Some(tx) = ticks.upgrade() else { break };
                    let now_ms = (started.elapsed().as_secs_f64() * speed * 1000.0) as u64;
                    if tx.send(NotesInput::Tick { now_ms }).is_err() {
                        break;
                    }
                }
            });
        }
        notes_in = Some(notes_in_tx);
    } else {
        drop(notes_tx);
    }

    let mut stats = Stats::default();
    let (mut speech_open, mut screen_open, mut notes_open) = (true, true, true);
    while speech_open || screen_open || notes_open {
        tokio::select! {
            event = speech_rx.recv(), if speech_open => match event {
                Some(event) => {
                    if let Some(tx) = &notes_in {
                        let _ = tx.send(NotesInput::from_speech(&event));
                    }
                    speech_event(event, &timeline, &mut stats)?;
                }
                None => speech_open = false,
            },
            event = screen_rx.recv(), if screen_open => match event {
                Some(event) => {
                    if let Some(tx) = &notes_in {
                        let _ = tx.send(NotesInput::from_screen(&event));
                    }
                    screen::record(&timeline, &event).map_err(|e| e.to_string())?;
                    screen_event(event, &mut stats, cli.no_read);
                }
                None => screen_open = false,
            },
            event = notes_rx.recv(), if notes_open => match event {
                Some(event) => {
                    notes::record(&timeline, &event).map_err(|e| e.to_string())?;
                    notes_event(event, &mut stats, &notes_path);
                }
                None => notes_open = false,
            },
        }
        if !speech_open && !screen_open {
            // Closes the notes input: the last section is written, then the
            // notes stream ends.
            notes_in = None;
        }
    }
    for w in workers {
        w.await.map_err(|e| e.to_string())?;
    }

    if stats.utterances > 0 {
        eprintln!(
            "\nSpeech: {} utterances: {} transcribed, {} empty, {} failed. {:.1} s of audio sent. \
             Request latency: median {}, 90th percentile {}, max {}.",
            stats.utterances,
            stats.latencies.len(),
            stats.empty,
            stats.failed,
            stats.audio_ms as f64 / 1000.0,
            percentile(&mut stats.latencies, 0.5),
            percentile(&mut stats.latencies, 0.9),
            percentile(&mut stats.latencies, 1.0),
        );
    }
    if stats.keyframes + stats.repeats > 0 {
        eprintln!(
            "Screen: {} keyframes, {} returns to an earlier screen. Read {}, skipped {}, failed {}. \
             Read latency: median {}, max {}. Tokens: {} in, {} out.",
            stats.keyframes,
            stats.repeats,
            stats.read_latencies.len(),
            stats.read_skipped,
            stats.read_failed,
            percentile(&mut stats.read_latencies, 0.5),
            percentile(&mut stats.read_latencies, 1.0),
            stats.tokens_in,
            stats.tokens_out,
        );
    }
    if stats.sections + stats.sections_empty + stats.sections_failed > 0 {
        eprintln!(
            "Notes: {} sections written, {} with nothing to note, {} failed. Lines corrected: {}. \
             Write latency: median {}, max {}. Tokens: {} in, {} out. Notes: {}",
            stats.sections,
            stats.sections_empty,
            stats.sections_failed,
            stats.corrections,
            percentile(&mut stats.write_latencies, 0.5),
            percentile(&mut stats.write_latencies, 1.0),
            stats.notes_tokens_in,
            stats.notes_tokens_out,
            notes_path.display(),
        );
    }
    eprintln!(
        "Took {:.1} s. Timeline: {}",
        started.elapsed().as_secs_f64(),
        db_path.display()
    );
    Ok(())
}

fn speech_event(
    event: PipelineEvent,
    timeline: &Timeline,
    stats: &mut Stats,
) -> Result<(), String> {
    match event {
        PipelineEvent::Detected { .. } => stats.utterances += 1,
        PipelineEvent::Transcribed { line, latency_ms } => {
            println!(
                "{} {:<6} {}   ({:.1} s)",
                at(line.start_ms),
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
            println!("{} {:<6} !! {message}", at(start_ms), speaker.label());
        }
    }
    Ok(())
}

fn screen_event(event: ScreenEvent, stats: &mut Stats, quiet_skips: bool) {
    match event {
        ScreenEvent::Keyframe {
            t_ms,
            repeat_of: Some(of),
            ..
        } => {
            stats.repeats += 1;
            println!(
                "{} Screen back to the screen from {:.2}s",
                at(t_ms),
                of as f64 / 1000.0
            );
        }
        ScreenEvent::Keyframe {
            t_ms,
            file,
            reason,
            change,
            ..
        } => {
            stats.keyframes += 1;
            println!(
                "{} Screen keyframe ({}, {:.1}% changed) {file}",
                at(t_ms),
                reason.as_str(),
                change * 100.0
            );
        }
        ScreenEvent::Read {
            t_ms,
            reading,
            usage,
            latency_ms,
            ..
        } => {
            stats.read_latencies.push(latency_ms);
            stats.tokens_in += usage.input_tokens;
            stats.tokens_out += usage.output_tokens;
            let title = if reading.title.is_empty() {
                String::new()
            } else {
                format!("\"{}\" ", reading.title)
            };
            println!(
                "{} Screen   read: [{}] {title}{}   ({:.1} s)",
                at(t_ms),
                reading.kind,
                reading.summary,
                latency_ms as f64 / 1000.0
            );
            if !reading.terms.is_empty() {
                println!("{} Screen   terms: {}", at(t_ms), reading.terms.join(", "));
            }
        }
        ScreenEvent::ReadSkipped { .. } if quiet_skips => {}
        ScreenEvent::ReadSkipped { t_ms, reason } => {
            stats.read_skipped += 1;
            let why = match reason {
                SkipReason::Busy => "reading fell behind and a newer screen came first",
                SkipReason::Cap => "reading cap reached",
                SkipReason::NoProfile => "no screen-reading profile",
            };
            println!("{} Screen   not read: {why}", at(t_ms));
        }
        ScreenEvent::ReadFailed { t_ms, message } => {
            stats.read_failed += 1;
            println!("{} Screen   !! {message}", at(t_ms));
        }
    }
}

fn notes_event(event: NotesEvent, stats: &mut Stats, notes_path: &Path) {
    match event {
        NotesEvent::Writing { .. } => {}
        NotesEvent::Section {
            section,
            usage,
            latency_ms,
        } => {
            stats.sections += 1;
            stats.write_latencies.push(latency_ms);
            stats.notes_tokens_in += usage.input_tokens;
            stats.notes_tokens_out += usage.output_tokens;
            println!(
                "{} Notes  section {} written ({:.1} s):",
                at(section.start_ms),
                section.index + 1,
                latency_ms as f64 / 1000.0
            );
            for line in section_markdown(&section).lines() {
                println!("    {line}");
            }
            if let Err(e) = notes::append_markdown(notes_path, &section) {
                eprintln!("cannot write {}: {e}", notes_path.display());
            }
        }
        NotesEvent::Empty { index } => {
            stats.sections_empty += 1;
            println!(
                "            Notes  section {} had nothing to note",
                index + 1
            );
        }
        NotesEvent::Failed {
            start_ms, message, ..
        } => {
            stats.sections_failed += 1;
            println!("{} Notes  !! {message}", at(start_ms));
        }
        NotesEvent::Corrected {
            speaker,
            start_ms,
            text,
        } => {
            stats.corrections += 1;
            println!("{} {:<6} corrected: {text}", at(start_ms), speaker.label());
        }
    }
}

fn at(t_ms: u64) -> String {
    format!("[{:>7.2}s]", t_ms as f64 / 1000.0)
}

fn percentile(values: &mut [u64], p: f64) -> String {
    values.sort_unstable();
    values
        .get(((values.len() as f64 - 1.0) * p).round() as usize)
        .map(|ms| format!("{:.1} s", *ms as f64 / 1000.0))
        .unwrap_or_else(|| "-".into())
}

struct Reader {
    name: String,
    model: String,
    generator: Arc<dyn TextGenerator>,
}

/// The generator for `role`, if a profile is named or assigned.
fn generator(
    settings: &Settings,
    name: Option<&str>,
    role: Role,
) -> Result<Option<Reader>, String> {
    let profile = match name {
        Some(name) => Some(
            settings
                .profile(name)
                .ok_or_else(|| format!("No profile named \"{name}\"."))?,
        ),
        None => settings.assigned(role),
    };
    let Some(profile) = profile else {
        return Ok(None);
    };
    let generator = ProviderClient::for_profile(profile, &SystemKeyStore, &http_client())
        .map_err(|e| e.to_string())?
        .into_text_generator();
    Ok(Some(Reader {
        name: profile.name.clone(),
        model: profile.model.clone(),
        generator,
    }))
}

#[derive(Deserialize)]
struct FrameRecord {
    t_ms: u64,
    file: String,
}

/// Reads `frames.jsonl`; empty when the fixture has no screen recording.
fn read_frame_index(fixture: &Path) -> Result<Vec<FrameRecord>, String> {
    let path = fixture.join("frames.jsonl");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(Vec::new());
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).map_err(|e| format!("{}: {e}", path.display())))
        .collect()
}

#[derive(Default)]
struct Stats {
    utterances: usize,
    empty: usize,
    failed: usize,
    audio_ms: u64,
    latencies: Vec<u64>,
    keyframes: usize,
    repeats: usize,
    read_skipped: usize,
    read_failed: usize,
    read_latencies: Vec<u64>,
    tokens_in: u64,
    tokens_out: u64,
    sections: usize,
    sections_empty: usize,
    sections_failed: usize,
    corrections: usize,
    write_latencies: Vec<u64>,
    notes_tokens_in: u64,
    notes_tokens_out: u64,
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
