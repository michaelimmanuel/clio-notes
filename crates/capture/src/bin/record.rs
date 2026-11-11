//! Records a meeting fixture: microphone, system audio and screen frames on one clock.
//!
//! Output layout:
//!   <out>/mic.wav        mono 16-bit, sample 0 = session time 0
//!   <out>/system.wav     same, from WASAPI loopback of the speaker device
//!   <out>/frames/*.png   one file per captured frame, named by session ms
//!   <out>/frames.jsonl   one line per frame: t_ms, file, size, grab time
//!   <out>/session.json   devices, settings and per-stream stats

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use capture::SessionClock;
use capture::audio::{self, AudioRecorder, AudioSource, AudioStats};
use capture::screen::{self, ScreenRecorder, ScreenStats, ScreenTarget};
use clap::Parser;
use serde::Serialize;

#[derive(Parser, Debug)]
#[command(about = "Record microphone, system audio and screen frames into a fixture folder")]
struct Args {
    /// List audio devices, monitors and windows, then exit.
    #[arg(long)]
    list: bool,

    /// Output folder. Must not exist or be empty. Default: fixtures/session-<unix time>
    #[arg(long)]
    out: Option<PathBuf>,

    /// Stop after this many seconds. Without it, stop with Ctrl+C.
    #[arg(long)]
    duration: Option<u64>,

    /// Microphone name (substring). Default: system default microphone.
    #[arg(long)]
    mic: Option<String>,

    /// Speaker/headset to capture system audio from (substring). Default: system default output.
    #[arg(long)]
    speaker: Option<String>,

    /// Monitor index from --list. Default: primary monitor.
    #[arg(long, conflicts_with = "window")]
    monitor: Option<usize>,

    /// Capture one window instead of a monitor (substring of its title).
    #[arg(long)]
    window: Option<String>,

    /// Screen frames per second.
    #[arg(long, default_value_t = 1.0)]
    fps: f64,

    #[arg(long)]
    no_mic: bool,
    #[arg(long)]
    no_system: bool,
    #[arg(long)]
    no_screen: bool,
}

#[derive(Serialize)]
struct SessionFile {
    format_version: u32,
    started_at_unix_ms: u128,
    duration_ms: u64,
    stopped_by: &'static str,
    mic: Option<AudioStats>,
    system: Option<AudioStats>,
    screen: Option<ScreenStats>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    if args.list {
        return list();
    }
    if args.no_mic && args.no_system && args.no_screen {
        bail!("nothing to record: --no-mic, --no-system and --no-screen are all set");
    }

    let out = args.out.clone().unwrap_or_else(default_out_dir);
    prepare_out_dir(&out)?;

    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        ctrlc::set_handler(move || stop.store(true, Ordering::Relaxed))
            .context("cannot install Ctrl+C handler")?;
    }

    let clock = SessionClock::start();

    // Start every stream before reporting, so a bad device fails fast and
    // nothing is left half-started.
    let mic = (!args.no_mic)
        .then(|| {
            AudioRecorder::start(
                AudioSource::Microphone,
                args.mic.as_deref(),
                &out.join("mic.wav"),
                clock,
            )
        })
        .transpose()
        .context("microphone")?;
    let system = (!args.no_system)
        .then(|| {
            AudioRecorder::start(
                AudioSource::SystemAudio,
                args.speaker.as_deref(),
                &out.join("system.wav"),
                clock,
            )
        })
        .transpose()
        .context("system audio")?;
    let target = match &args.window {
        Some(title) => ScreenTarget::Window(title.clone()),
        None => ScreenTarget::Monitor(args.monitor),
    };
    let screen = (!args.no_screen)
        .then(|| ScreenRecorder::start(target, &out, args.fps, clock))
        .transpose()
        .context("screen")?;

    eprintln!("Recording to {}", out.display());
    if let Some(m) = &mic {
        eprintln!("  mic:    {}", m.device_name());
    }
    if let Some(s) = &system {
        eprintln!("  system: {} (loopback)", s.device_name());
    }
    if let Some(s) = &screen {
        eprintln!("  screen: {} at {} fps", s.target_name(), args.fps);
    }
    match args.duration {
        Some(secs) => eprintln!("Stopping after {secs} s, or press Ctrl+C."),
        None => eprintln!("Press Ctrl+C to stop."),
    }

    let limit = args.duration.map(Duration::from_secs);
    let mut last_report = 0;
    let stopped_by = loop {
        if stop.load(Ordering::Relaxed) {
            break "ctrl_c";
        }
        let elapsed = clock.elapsed();
        if limit.is_some_and(|l| elapsed >= l) {
            break "duration";
        }
        let secs = elapsed.as_secs();
        if secs >= last_report + 30 {
            last_report = secs;
            eprintln!("  {:02}:{:02} recorded", secs / 60, secs % 60);
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let duration_ms = clock.elapsed().as_millis() as u64;

    eprintln!("Stopping...");
    // Stop everything even if one stream fails, then report the first error.
    let mic = mic.map(AudioRecorder::stop).transpose();
    let system = system.map(AudioRecorder::stop).transpose();
    let screen = screen.map(ScreenRecorder::stop).transpose();

    let session = SessionFile {
        format_version: 1,
        started_at_unix_ms: clock.started_at_unix_ms(),
        duration_ms,
        stopped_by,
        mic: mic.context("microphone")?,
        system: system.context("system audio")?,
        screen: screen.context("screen")?,
    };
    fs::write(
        out.join("session.json"),
        serde_json::to_string_pretty(&session)?,
    )?;
    print_summary(&session, &out);
    Ok(())
}

fn default_out_dir() -> PathBuf {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    PathBuf::from("fixtures").join(format!("session-{secs}"))
}

fn prepare_out_dir(out: &Path) -> Result<()> {
    if out.exists() && fs::read_dir(out)?.next().is_some() {
        bail!("{} already exists and is not empty", out.display());
    }
    fs::create_dir_all(out).with_context(|| format!("cannot create {}", out.display()))
}

fn print_summary(s: &SessionFile, out: &Path) {
    eprintln!(
        "Done: {:.1} s in {}",
        s.duration_ms as f64 / 1000.0,
        out.display()
    );
    for a in [&s.mic, &s.system].into_iter().flatten() {
        let first = a
            .first_packet_ms
            .map_or("never".to_string(), |ms| format!("{ms} ms"));
        let peak = a
            .peak_dbfs
            .map_or("silent".to_string(), |db| format!("{db} dBFS"));
        eprintln!(
            "  {:?}: {:.1} s, {} Hz, peak {peak}, padded {} ms, first packet {first}, \
             drift {} ms, discontinuities {}, errors {}",
            a.source,
            a.duration_ms as f64 / 1000.0,
            a.sample_rate,
            a.padded_ms,
            a.drift_ms,
            a.discontinuities,
            a.stream_errors,
        );
    }
    if let Some(m) = &s.mic {
        if m.is_digital_silence() {
            eprintln!(
                "WARNING: \"{}\" delivered only digital silence (every sample is zero). \
                 It is muted, or a virtual device whose app is not running. \
                 Pick another with --mic (see --list).",
                m.device
            );
        } else if m.first_packet_ms.is_none() {
            eprintln!("WARNING: \"{}\" delivered no audio at all.", m.device);
        }
    }
    if let Some(sys) = &s.system
        && sys.peak_dbfs.is_none()
    {
        eprintln!(
            "Note: no sound was played through \"{}\" during the recording. \
             If meeting audio goes to another device, pick it with --speaker.",
            sys.device
        );
    }
    if let Some(sc) = &s.screen {
        eprintln!(
            "  screen: {} frames, {} skipped ticks, {} failed grabs, grab {} ms, encode {} ms avg",
            sc.frames, sc.skipped_ticks, sc.failed_grabs, sc.avg_grab_ms, sc.avg_encode_ms
        );
    }
}

fn list() -> Result<()> {
    let (inputs, outputs) = audio::list_devices()?;
    let mark = |d: bool| if d { " (default)" } else { "" };
    println!("Microphones (--mic):");
    for d in &inputs {
        println!("  {}{}", d.name, mark(d.is_default));
    }
    println!("Speakers for system audio (--speaker):");
    for d in &outputs {
        println!("  {}{}", d.name, mark(d.is_default));
    }
    println!("Monitors (--monitor):");
    for m in screen::list_monitors()? {
        println!(
            "  {}: {} {}x{}{}",
            m.index,
            m.name,
            m.width,
            m.height,
            if m.is_primary { " (primary)" } else { "" }
        );
    }
    println!("Windows (--window):");
    for w in screen::list_windows()? {
        println!("  {} — {} ({}x{})", w.title, w.app, w.width, w.height);
    }
    Ok(())
}
