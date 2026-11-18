//! Compressing the recordings of meetings that have stopped.
//!
//! A meeting's audio is recorded as WAV, about 700 MB per hour for its two
//! tracks. A background worker turns the WAVs of every stopped meeting into
//! Opus files of about 30 MB per hour with [`pipeline::recording`], one
//! meeting at a time, newest first. It runs at startup, when a meeting
//! stops and every half hour, and waits while a meeting is being recorded.
//! A meeting that never stopped is left alone until recovery has given it
//! a stop time and repaired its WAVs.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use pipeline::recording;
use storage::Timeline;
use tauri::{AppHandle, Manager};
use tokio::sync::Notify;

use crate::meetings::{id_of, recording_id};

/// How often the worker looks for recordings to compress without being
/// woken. Only a failed one is still there by then.
const CHECK_EVERY: Duration = Duration::from_secs(30 * 60);

#[derive(Default)]
pub struct RecordingsState {
    /// Wakes the worker.
    wake: Notify,
    /// The meeting whose recording is being compressed.
    busy: Mutex<Option<String>>,
}

impl RecordingsState {
    /// Asks the worker to look for recordings now: after a meeting stops.
    pub fn wake(&self) {
        self.wake.notify_one();
    }

    /// Whether meeting `id`'s recording is being compressed.
    pub fn is_busy(&self, id: &str) -> bool {
        self.busy.lock().unwrap().as_deref() == Some(id)
    }
}

pub fn start_worker(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        loop {
            work(&app).await;
            let state = app.state::<RecordingsState>();
            let _ = tokio::time::timeout(CHECK_EVERY, state.wake.notified()).await;
        }
    });
}

/// Compresses the recordings of every stopped meeting, newest first.
async fn work(app: &AppHandle) {
    let Ok(root) = storage::meetings_root() else {
        return;
    };
    for dir in waiting(&root) {
        // Recording needs the computer more; stopping wakes the worker.
        if recording_id(app).is_some() {
            return;
        }
        if !stopped(&dir) {
            continue;
        }
        let id = id_of(&dir);
        let state = app.state::<RecordingsState>();
        *state.busy.lock().unwrap() = Some(id.clone());
        let started = Instant::now();
        let result =
            tauri::async_runtime::spawn_blocking(move || recording::compress_meeting(&dir))
                .await
                .map_err(|e| e.to_string())
                .and_then(|r| r);
        *state.busy.lock().unwrap() = None;
        match result {
            Ok(done) if !done.is_empty() => {
                let mb = |bytes: u64| bytes as f64 / 1_000_000.0;
                log::info!(
                    target: "clio::recordings",
                    "Meeting {id}: {} compressed from {:.1} MB to {:.1} MB in {:.1} s",
                    done.iter().map(|c| c.file.as_str()).collect::<Vec<_>>().join(" and "),
                    mb(done.iter().map(|c| c.wav_bytes).sum()),
                    mb(done.iter().map(|c| c.opus_bytes).sum()),
                    started.elapsed().as_secs_f64()
                );
            }
            Ok(_) => {}
            Err(e) => log::warn!(
                target: "clio::recordings",
                "Meeting {id}: could not compress the recording, so it is kept as it is: {e}"
            ),
        }
    }
}

/// The meeting folders that still hold WAV recordings, newest first.
fn waiting(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|dir| {
            storage::is_meeting_id(&id_of(dir)) && !recording::uncompressed(dir).is_empty()
        })
        .collect();
    // Folder names hold the start time.
    dirs.sort_by_key(|dir| std::cmp::Reverse(id_of(dir)));
    dirs
}

/// Whether the meeting in `dir` has a stop time: it stopped, or recovery
/// finished it after the app closed mid-meeting.
fn stopped(dir: &Path) -> bool {
    Timeline::open(&dir.join("timeline.sqlite"))
        .ok()
        .and_then(|t| t.meta("stopped_at_unix_ms").ok().flatten())
        .is_some()
}
