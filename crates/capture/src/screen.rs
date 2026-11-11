//! Periodic capture of one monitor or window to PNG frames.
//!
//! Frames go to `<dir>/frames/<session ms>.png`, and one JSON line per frame is
//! appended to `<dir>/frames.jsonl`. This is raw capture only: change detection
//! and keyframe selection happen later in the pipeline.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use xcap::{Monitor, Window};

use crate::clock::SessionClock;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScreenTarget {
    /// Index into [`list_monitors`]; `None` is the primary monitor.
    Monitor(Option<usize>),
    /// Case-insensitive substring of a window title.
    Window(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct MonitorInfo {
    pub index: usize,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub is_primary: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct WindowInfo {
    pub app: String,
    pub title: String,
    pub width: u32,
    pub height: u32,
}

pub fn list_monitors() -> Result<Vec<MonitorInfo>> {
    Ok(Monitor::all()?
        .iter()
        .enumerate()
        .map(|(index, m)| MonitorInfo {
            index,
            name: m.friendly_name().or_else(|_| m.name()).unwrap_or_default(),
            width: m.width().unwrap_or(0),
            height: m.height().unwrap_or(0),
            is_primary: m.is_primary().unwrap_or(false),
        })
        .collect())
}

/// Visible, titled, non-minimized windows.
pub fn list_windows() -> Result<Vec<WindowInfo>> {
    Ok(Window::all()?
        .iter()
        .filter(|w| !w.is_minimized().unwrap_or(true))
        .filter_map(|w| {
            let title = w.title().ok()?;
            (!title.trim().is_empty()).then(|| WindowInfo {
                app: w.app_name().unwrap_or_default(),
                title,
                width: w.width().unwrap_or(0),
                height: w.height().unwrap_or(0),
            })
        })
        .collect())
}

/// One line of `frames.jsonl`.
#[derive(Debug, Serialize)]
struct FrameRecord<'a> {
    /// Session time when capture of this frame began.
    t_ms: u64,
    file: &'a str,
    width: u32,
    height: u32,
    /// How long the grab itself took; the frame shows the screen somewhere in
    /// `[t_ms, t_ms + grab_ms]`.
    grab_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScreenStats {
    pub target: String,
    pub fps: f64,
    pub frames: u64,
    /// Ticks skipped because capture or encoding fell behind.
    pub skipped_ticks: u64,
    /// Ticks where the window was minimized or the grab failed.
    pub failed_grabs: u64,
    pub avg_grab_ms: u64,
    pub avg_encode_ms: u64,
}

/// A running screen capture. Call [`ScreenRecorder::stop`] to end it.
pub struct ScreenRecorder {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<Result<ScreenStats>>,
    target_name: String,
}

enum Source {
    Monitor(Monitor),
    Window(Window),
}

impl Source {
    fn grab(&self) -> Result<image::RgbaImage> {
        match self {
            Source::Monitor(m) => Ok(m.capture_image()?),
            Source::Window(w) => {
                if w.is_minimized().unwrap_or(false) {
                    bail!("window is minimized");
                }
                Ok(w.capture_image()?)
            }
        }
    }
}

fn resolve(target: &ScreenTarget) -> Result<(Source, String)> {
    match target {
        ScreenTarget::Monitor(index) => {
            let monitors = Monitor::all()?;
            let monitor = match index {
                Some(i) => monitors
                    .into_iter()
                    .nth(*i)
                    .ok_or_else(|| anyhow!("no monitor with index {i}"))?,
                None => monitors
                    .into_iter()
                    .find(|m| m.is_primary().unwrap_or(false))
                    .ok_or_else(|| anyhow!("no primary monitor found"))?,
            };
            let name = format!(
                "monitor {} ({}x{})",
                monitor
                    .friendly_name()
                    .or_else(|_| monitor.name())
                    .unwrap_or_default(),
                monitor.width().unwrap_or(0),
                monitor.height().unwrap_or(0),
            );
            Ok((Source::Monitor(monitor), name))
        }
        ScreenTarget::Window(wanted) => {
            let needle = wanted.to_lowercase();
            let mut matches: Vec<Window> = Window::all()?
                .into_iter()
                .filter(|w| !w.is_minimized().unwrap_or(true))
                .filter(|w| w.title().is_ok_and(|t| t.to_lowercase().contains(&needle)))
                .collect();
            match matches.len() {
                0 => bail!("no visible window title contains \"{wanted}\""),
                1 => {
                    let w = matches.remove(0);
                    let name = format!("window \"{}\"", w.title().unwrap_or_default());
                    Ok((Source::Window(w), name))
                }
                _ => {
                    let titles: Vec<_> = matches.iter().filter_map(|w| w.title().ok()).collect();
                    bail!(
                        "\"{wanted}\" matches several windows: {}",
                        titles.join(" | ")
                    )
                }
            }
        }
    }
}

impl ScreenRecorder {
    pub fn start(target: ScreenTarget, dir: &Path, fps: f64, clock: SessionClock) -> Result<Self> {
        if !(0.1..=10.0).contains(&fps) {
            bail!("fps must be between 0.1 and 10");
        }
        // Resolve once here so a bad target fails before recording starts.
        // The capture thread resolves again because xcap handles are not Send.
        let (_, target_name) = resolve(&target)?;

        let frames_dir = dir.join("frames");
        fs::create_dir_all(&frames_dir)?;
        let index = BufWriter::new(File::create(dir.join("frames.jsonl"))?);

        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            let name = target_name.clone();
            std::thread::Builder::new()
                .name("screen-capture".into())
                .spawn(move || capture_loop(target, name, frames_dir, index, fps, clock, stop))?
        };
        Ok(Self {
            stop,
            thread,
            target_name,
        })
    }

    pub fn target_name(&self) -> &str {
        &self.target_name
    }

    pub fn stop(self) -> Result<ScreenStats> {
        self.stop.store(true, Ordering::Relaxed);
        self.thread
            .join()
            .map_err(|_| anyhow!("screen capture thread panicked"))?
    }
}

fn capture_loop(
    target: ScreenTarget,
    target_name: String,
    frames_dir: PathBuf,
    mut index: BufWriter<File>,
    fps: f64,
    clock: SessionClock,
    stop: Arc<AtomicBool>,
) -> Result<ScreenStats> {
    let (source, _) = resolve(&target)?;
    let interval = Duration::from_secs_f64(1.0 / fps);
    let mut next = Instant::now();
    let (mut frames, mut skipped, mut failed) = (0u64, 0u64, 0u64);
    let (mut grab_total, mut encode_total) = (Duration::ZERO, Duration::ZERO);
    let mut last_error = String::new();

    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        if now < next {
            // Sleep in short steps so stop() returns promptly.
            std::thread::sleep((next - now).min(Duration::from_millis(50)));
            continue;
        }
        // If we fell more than one tick behind, drop the missed ticks rather
        // than bursting to catch up.
        while next + interval <= now {
            next += interval;
            skipped += 1;
        }
        next += interval;

        let t = clock.elapsed();
        let grab_start = Instant::now();
        let image = match source.grab() {
            Ok(img) => img,
            Err(e) => {
                failed += 1;
                let msg = e.to_string();
                if msg != last_error {
                    eprintln!("screen grab failed: {msg}");
                    last_error = msg;
                }
                continue;
            }
        };
        let grab = grab_start.elapsed();
        last_error.clear();

        let t_ms = t.as_millis() as u64;
        let file = format!("frames/{t_ms:09}.png");
        let encode_start = Instant::now();
        image
            .save(frames_dir.join(format!("{t_ms:09}.png")))
            .with_context(|| format!("cannot write {file}"))?;
        encode_total += encode_start.elapsed();
        grab_total += grab;
        frames += 1;

        let record = FrameRecord {
            t_ms,
            file: &file,
            width: image.width(),
            height: image.height(),
            grab_ms: grab.as_millis() as u64,
        };
        serde_json::to_writer(&mut index, &record)?;
        index.write_all(b"\n")?;
        // Flush per frame so an interrupted session keeps its index.
        index.flush()?;
    }

    let avg = |total: Duration| total.as_millis() as u64 / frames.max(1);
    Ok(ScreenStats {
        target: target_name,
        fps,
        frames,
        skipped_ticks: skipped,
        failed_grabs: failed,
        avg_grab_ms: avg(grab_total),
        avg_encode_ms: avg(encode_total),
    })
}
