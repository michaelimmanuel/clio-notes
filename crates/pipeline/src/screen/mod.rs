//! The screen pipeline: frames in, keyframes and screen readings out.
//!
//! ```text
//! frames ─► fingerprint ─► keyframe selector ─► keyframes/<t_ms>.png ─► screen reading
//! ```
//!
//! Frames are dropped upstream when this task is busy (the capture tap uses a
//! small bounded channel), so a slow disk or model never holds up capture.
//! Readings run up to `max_in_flight` at a time; when that is full, only the
//! newest waiting keyframe is kept and older waiting ones are skipped.

pub mod fingerprint;
pub mod keyframe;
pub mod read;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use base64::Engine;
use image::RgbaImage;
use image::imageops::FilterType;
use providers::{TextGenerator, Usage};
use serde::Serialize;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

pub use fingerprint::Fingerprint;
pub use keyframe::{Decision, KeyframeConfig, KeyframeReason, KeyframeSelector, ScreenMode};
pub use read::{ReadResult, ScreenReading, read_screen};

/// One captured frame.
pub struct Frame {
    pub t_ms: u64,
    pub image: RgbaImage,
}

#[derive(Debug, Clone)]
pub struct ScreenOptions {
    pub keyframes: KeyframeConfig,
    /// The meeting folder. Keyframes go to `<dir>/keyframes/<t_ms>.png`.
    pub dir: PathBuf,
    /// Most screen readings per meeting; `None` is no limit.
    pub read_cap: Option<u32>,
    pub max_in_flight: usize,
}

impl ScreenOptions {
    pub fn new(dir: PathBuf, mode: ScreenMode, read_cap: Option<u32>) -> Self {
        Self {
            keyframes: KeyframeConfig::for_mode(mode),
            dir,
            read_cap,
            // A reading takes 10–25 s, so three in flight keep up with a
            // new screen every 5–8 s.
            max_in_flight: 3,
        }
    }
}

/// Why a keyframe was not read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// A newer keyframe arrived while reading was behind.
    Busy,
    /// The per-meeting limit on screen readings was reached.
    Cap,
    /// No profile is assigned to screen reading.
    NoProfile,
}

impl SkipReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Busy => "skipped_busy",
            Self::Cap => "skipped_cap",
            Self::NoProfile => "skipped_no_profile",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ScreenEvent {
    /// A new keyframe, saved to disk, or a return to an earlier one.
    Keyframe {
        t_ms: u64,
        /// Relative to the meeting folder.
        file: String,
        width: u32,
        height: u32,
        reason: KeyframeReason,
        change: f32,
        /// Set when the screen went back to the keyframe from this time;
        /// `file` is then that keyframe's image.
        repeat_of: Option<u64>,
        /// A small JPEG preview as a data URL, for the UI.
        thumbnail: String,
    },
    Read {
        t_ms: u64,
        reading: ScreenReading,
        model: String,
        usage: Usage,
        latency_ms: u64,
        /// Readings started so far in this meeting.
        reads_used: u32,
    },
    ReadSkipped {
        t_ms: u64,
        reason: SkipReason,
    },
    ReadFailed {
        t_ms: u64,
        message: String,
    },
}

/// Stores a screen event in the meeting's timeline.
pub fn record(timeline: &storage::Timeline, event: &ScreenEvent) -> storage::Result<()> {
    match event {
        ScreenEvent::Keyframe {
            t_ms,
            file,
            width,
            height,
            reason,
            change,
            repeat_of,
            ..
        } => {
            let mut row = storage::KeyframeRow::new(
                *t_ms,
                file,
                *width,
                *height,
                reason.as_str(),
                *change as f64,
            );
            if repeat_of.is_some() {
                row.repeat_of = *repeat_of;
                row.read_status = "repeat".into();
            }
            timeline.add_keyframe(&row)
        }
        ScreenEvent::Read {
            t_ms,
            reading,
            model,
            ..
        } => {
            let json = serde_json::to_string(reading).unwrap_or_default();
            timeline.set_keyframe_read(*t_ms, "read", Some(&json), Some(model), None)
        }
        ScreenEvent::ReadSkipped { t_ms, reason } => {
            timeline.set_keyframe_read(*t_ms, reason.as_str(), None, None, None)
        }
        ScreenEvent::ReadFailed { t_ms, message } => {
            timeline.set_keyframe_read(*t_ms, "failed", None, None, Some(message))
        }
    }
}

struct Saved {
    file: String,
    width: u32,
    height: u32,
    jpeg: Vec<u8>,
    thumbnail: String,
}

/// Runs until `frames` closes and every reading has finished.
pub async fn screen_stream(
    opts: ScreenOptions,
    mut frames: mpsc::Receiver<Frame>,
    reader: Option<Arc<dyn TextGenerator>>,
    events: mpsc::UnboundedSender<ScreenEvent>,
) {
    let mut selector = KeyframeSelector::new(opts.keyframes.clone());
    let mut files: Vec<(u64, String, u32, u32, String)> = Vec::new();
    let mut reads = Reads {
        reader,
        cap: opts.read_cap,
        max_in_flight: opts.max_in_flight.max(1),
        used: 0,
        running: JoinSet::new(),
        waiting: None,
        events: events.clone(),
    };

    loop {
        tokio::select! {
            frame = frames.recv() => {
                let Some(frame) = frame else { break };
                let Frame { t_ms, image } = frame;
                let Ok((fp, image)) =
                    tokio::task::spawn_blocking(move || (Fingerprint::of(&image), image)).await
                else {
                    continue;
                };
                match selector.push(t_ms, fp) {
                    None => {}
                    Some(Decision::Repeat { t_ms, of_t_ms }) => {
                        if let Some((_, file, width, height, thumbnail)) =
                            files.iter().find(|f| f.0 == of_t_ms)
                        {
                            let _ = events.send(ScreenEvent::Keyframe {
                                t_ms,
                                file: file.clone(),
                                width: *width,
                                height: *height,
                                reason: KeyframeReason::Settled,
                                change: 0.0,
                                repeat_of: Some(of_t_ms),
                                thumbnail: thumbnail.clone(),
                            });
                        }
                    }
                    Some(Decision::New { t_ms, reason, change }) => {
                        let dir = opts.dir.clone();
                        let saved = tokio::task::spawn_blocking(move || save(&dir, t_ms, &image)).await;
                        let saved = match saved {
                            Ok(Ok(s)) => s,
                            Ok(Err(e)) => {
                                let _ = events.send(ScreenEvent::ReadFailed {
                                    t_ms,
                                    message: format!("Could not save the keyframe: {e}"),
                                });
                                continue;
                            }
                            Err(_) => continue,
                        };
                        files.push((t_ms, saved.file.clone(), saved.width, saved.height, saved.thumbnail.clone()));
                        let _ = events.send(ScreenEvent::Keyframe {
                            t_ms,
                            file: saved.file,
                            width: saved.width,
                            height: saved.height,
                            reason,
                            change,
                            repeat_of: None,
                            thumbnail: saved.thumbnail,
                        });
                        reads.submit(t_ms, saved.jpeg);
                    }
                }
            }
            Some(done) = reads.running.join_next(), if !reads.running.is_empty() => {
                if let Ok(event) = done {
                    let _ = events.send(event);
                }
                reads.start_waiting();
            }
        }
    }

    // Capture has ended: finish what is running and what is waiting.
    while let Some(done) = reads.running.join_next().await {
        if let Ok(event) = done {
            let _ = events.send(event);
        }
        reads.start_waiting();
    }
}

struct Reads {
    reader: Option<Arc<dyn TextGenerator>>,
    cap: Option<u32>,
    max_in_flight: usize,
    used: u32,
    running: JoinSet<ScreenEvent>,
    waiting: Option<(u64, Vec<u8>)>,
    events: mpsc::UnboundedSender<ScreenEvent>,
}

impl Reads {
    fn submit(&mut self, t_ms: u64, jpeg: Vec<u8>) {
        if self.reader.is_none() {
            self.skip(t_ms, SkipReason::NoProfile);
            return;
        }
        if let Some((old, _)) = self.waiting.replace((t_ms, jpeg)) {
            self.skip(old, SkipReason::Busy);
        }
        self.start_waiting();
    }

    fn start_waiting(&mut self) {
        if self.running.len() >= self.max_in_flight {
            return;
        }
        let (Some((t_ms, jpeg)), Some(reader)) = (self.waiting.take(), self.reader.clone()) else {
            return;
        };
        if self.cap.is_some_and(|cap| self.used >= cap) {
            self.skip(t_ms, SkipReason::Cap);
            return;
        }
        self.used += 1;
        let reads_used = self.used;
        self.running.spawn(async move {
            let started = Instant::now();
            match read_screen(reader.as_ref(), jpeg).await {
                Ok(r) => ScreenEvent::Read {
                    t_ms,
                    reading: r.reading,
                    model: r.model,
                    usage: r.usage,
                    latency_ms: started.elapsed().as_millis() as u64,
                    reads_used,
                },
                Err(message) => ScreenEvent::ReadFailed { t_ms, message },
            }
        });
    }

    fn skip(&self, t_ms: u64, reason: SkipReason) {
        let _ = self.events.send(ScreenEvent::ReadSkipped { t_ms, reason });
    }
}

/// Writes the full-size PNG and prepares the model and UI copies.
fn save(dir: &std::path::Path, t_ms: u64, image: &RgbaImage) -> Result<Saved, String> {
    let rel = format!("keyframes/{t_ms:09}.png");
    let path = dir.join(&rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    image.save(&path).map_err(|e| e.to_string())?;

    let (w, h) = image.dimensions();
    let tw = 320u32;
    let th = ((h as f32 * tw as f32 / w.max(1) as f32).round() as u32).max(1);
    let small = image::DynamicImage::ImageRgba8(image::imageops::resize(
        image,
        tw,
        th,
        FilterType::Triangle,
    ))
    .into_rgb8();
    let mut thumb = std::io::Cursor::new(Vec::new());
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut thumb, 75)
        .encode_image(&small)
        .map_err(|e| e.to_string())?;
    let thumbnail = format!(
        "data:image/jpeg;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(thumb.into_inner())
    );
    Ok(Saved {
        file: rel,
        width: w,
        height: h,
        jpeg: read::encode_for_reading(image),
        thumbnail,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use image::Rgba;
    use providers::{ProviderError, TextRequest, TextResponse};
    use std::time::Duration;

    struct SlowReader;

    #[async_trait]
    impl TextGenerator for SlowReader {
        async fn generate(&self, _: &TextRequest) -> Result<TextResponse, ProviderError> {
            tokio::time::sleep(Duration::from_millis(300)).await;
            Ok(TextResponse {
                text: r#"{"kind":"slides","title":"T","text":"","tables":[],"charts":[],
                    "code":[],"terms":[],"summary":"s"}"#
                    .into(),
                model: "fake".into(),
                usage: Usage::default(),
            })
        }
    }

    fn slide(n: u32) -> RgbaImage {
        let mut img = RgbaImage::from_pixel(320, 180, Rgba([255, 255, 255, 255]));
        let shade = (n * 40 % 200) as u8;
        for y in 40..140 {
            for x in 20..300 {
                img.put_pixel(x, y, Rgba([shade, shade, shade, 255]));
            }
        }
        img
    }

    async fn run(
        slides: &[u32],
        reader: Option<Arc<dyn TextGenerator>>,
        cap: Option<u32>,
    ) -> (Vec<ScreenEvent>, PathBuf) {
        static RUN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "clio-screen-{}-{}",
            std::process::id(),
            RUN.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let mut opts = ScreenOptions::new(dir.clone(), ScreenMode::Slides, cap);
        opts.max_in_flight = 1;
        let (tx, rx) = mpsc::channel(4);
        let (etx, mut erx) = mpsc::unbounded_channel();
        let task = tokio::spawn(screen_stream(opts, rx, reader, etx));
        for (i, n) in slides.iter().enumerate() {
            tx.send(Frame {
                t_ms: i as u64 * 1000,
                image: slide(*n),
            })
            .await
            .unwrap();
        }
        drop(tx);
        task.await.unwrap();
        let mut out = Vec::new();
        while let Ok(e) = erx.try_recv() {
            out.push(e);
        }
        (out, dir)
    }

    fn count(events: &[ScreenEvent], pred: impl Fn(&ScreenEvent) -> bool) -> usize {
        events.iter().filter(|e| pred(e)).count()
    }

    #[tokio::test]
    async fn keyframes_are_saved_and_read() {
        let reader: Arc<dyn TextGenerator> = Arc::new(SlowReader);
        let (events, dir) = run(&[0, 0, 1, 1, 0, 0], Some(reader), None).await;
        let keyframes: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                ScreenEvent::Keyframe {
                    t_ms,
                    repeat_of,
                    file,
                    ..
                } => Some((*t_ms, *repeat_of, file.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(keyframes.len(), 3, "{events:?}");
        assert_eq!(keyframes[2].1, Some(0));
        assert!(dir.join(&keyframes[0].2).exists());
        assert!(dir.join(&keyframes[1].2).exists());
        // Two new screens, two readings; the repeat is not read again.
        assert_eq!(count(&events, |e| matches!(e, ScreenEvent::Read { .. })), 2);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn cap_and_missing_profile_skip_reading() {
        let reader: Arc<dyn TextGenerator> = Arc::new(SlowReader);
        let (events, dir) = run(&[0, 0, 1, 1, 2, 2], Some(reader), Some(1)).await;
        let read = count(&events, |e| matches!(e, ScreenEvent::Read { .. }));
        let skipped = count(&events, |e| matches!(e, ScreenEvent::ReadSkipped { .. }));
        assert_eq!(read, 1, "{events:?}");
        assert_eq!(read + skipped, 3, "{events:?}");
        std::fs::remove_dir_all(dir).unwrap();

        let (events, dir) = run(&[0, 0, 1, 1], None, None).await;
        assert_eq!(
            count(&events, |e| matches!(
                e,
                ScreenEvent::ReadSkipped {
                    reason: SkipReason::NoProfile,
                    ..
                }
            )),
            2
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
