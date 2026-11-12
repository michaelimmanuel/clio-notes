//! Tauri commands that start and stop a meeting: capture, live transcription
//! and the timeline.
//!
//! The UI listens for two events:
//! - `meeting-event`: a [`PipelineEvent`] for each utterance as it moves
//!   from detected to transcribed (or empty, or failed);
//! - `meeting-finished`: a [`MeetingSummary`] once capture has stopped and
//!   every pending request has finished.

use std::sync::Mutex;
use std::sync::mpsc as std_mpsc;

use capture::{AudioRecorder, AudioSource, AudioStats, SessionClock};
use pipeline::{PipelineEvent, StreamOptions, transcribe_stream};
use providers::{ProviderClient, Role};
use serde::Serialize;
use storage::{Speaker, Timeline};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::mpsc;

use crate::settings::{AppState, UiError};

#[derive(Default)]
pub struct MeetingState {
    running: Mutex<Option<Running>>,
}

struct Running {
    stop: std_mpsc::Sender<()>,
    info: MeetingInfo,
    stopping: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct MeetingInfo {
    dir: String,
    started_at_unix_ms: u64,
    mic: String,
    system: String,
    model: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct MeetingStatus {
    meeting: Option<MeetingInfo>,
    stopping: bool,
    elapsed_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct MeetingSummary {
    dir: String,
    lines: usize,
    failed: usize,
    error: Option<String>,
}

/// What the capture thread reports once both recorders are running.
struct Started {
    mic: (String, u32),
    system: (String, u32),
}

#[tauri::command]
pub fn meeting_status(meetings: State<'_, MeetingState>) -> MeetingStatus {
    let running = meetings.running.lock().unwrap();
    match running.as_ref() {
        Some(r) => MeetingStatus {
            meeting: Some(r.info.clone()),
            stopping: r.stopping,
            elapsed_ms: now_unix_ms().saturating_sub(r.info.started_at_unix_ms),
        },
        None => MeetingStatus {
            meeting: None,
            stopping: false,
            elapsed_ms: 0,
        },
    }
}

#[tauri::command]
pub async fn start_meeting(
    app: AppHandle,
    state: State<'_, AppState>,
    meetings: State<'_, MeetingState>,
) -> Result<MeetingInfo, UiError> {
    if meetings.running.lock().unwrap().is_some() {
        return Err(UiError::new("busy", "A meeting is already running."));
    }

    let settings = state.store.load()?;
    let profile = settings
        .assigned(Role::SpeechToText)
        .cloned()
        .ok_or_else(|| {
            UiError::new(
                "not_assigned",
                "Choose a profile for speech to text in Settings first.",
            )
        })?;
    let client = ProviderClient::for_profile(&profile, &state.keys, &state.http)?;
    let stt = client.clone().into_speech_to_text().ok_or_else(|| {
        UiError::new(
            "unsupported",
            format!("\"{}\" cannot transcribe audio.", profile.name),
        )
    })?;
    // Opens the connection now, so the first utterance does not pay for it.
    tauri::async_runtime::spawn(async move {
        let _ = client.check().await;
    });

    let root = storage::meetings_root().map_err(|e| UiError::new("storage", e.to_string()))?;
    let dir =
        storage::create_meeting_dir(&root).map_err(|e| UiError::new("storage", e.to_string()))?;
    let timeline = Timeline::open(&dir.join("timeline.sqlite"))
        .map_err(|e| UiError::new("storage", e.to_string()))?;

    let clock = SessionClock::start();
    let (mic_tx, mic_rx) = mpsc::unbounded_channel::<Vec<f32>>();
    let (sys_tx, sys_rx) = mpsc::unbounded_channel::<Vec<f32>>();
    let (stop_tx, stop_rx) = std_mpsc::channel::<()>();
    let (ready_tx, ready_rx) = std_mpsc::channel::<Result<Started, String>>();
    let (stats_tx, stats_rx) = std_mpsc::channel::<Vec<AudioStats>>();

    // cpal streams are owned by one thread from start to stop.
    let capture_dir = dir.clone();
    std::thread::Builder::new()
        .name("meeting-capture".into())
        .spawn(move || {
            let start = |source, file: &str, tx: mpsc::UnboundedSender<Vec<f32>>| {
                AudioRecorder::start_with_tap(
                    source,
                    None,
                    &capture_dir.join(file),
                    clock,
                    Some(Box::new(move |block: &[f32]| {
                        let _ = tx.send(block.to_vec());
                    })),
                )
            };
            let mic = match start(AudioSource::Microphone, "mic.wav", mic_tx) {
                Ok(r) => r,
                Err(e) => {
                    let _ = ready_tx.send(Err(format!("Cannot record the microphone: {e:#}")));
                    return;
                }
            };
            let system = match start(AudioSource::SystemAudio, "system.wav", sys_tx) {
                Ok(r) => r,
                Err(e) => {
                    let _ = mic.stop();
                    let _ = ready_tx.send(Err(format!("Cannot record system audio: {e:#}")));
                    return;
                }
            };
            let _ = ready_tx.send(Ok(Started {
                mic: (mic.device_name().to_string(), mic.sample_rate()),
                system: (system.device_name().to_string(), system.sample_rate()),
            }));
            // Runs until stop_meeting, or until the app drops the sender.
            let _ = stop_rx.recv();
            // Stopping ends each writer thread, which drops its tap and so
            // closes the pipeline's input once the last samples are in.
            let stats = [mic.stop(), system.stop()]
                .into_iter()
                .filter_map(|r| r.map_err(|e| eprintln!("stopping capture: {e:#}")).ok())
                .collect();
            let _ = stats_tx.send(stats);
        })
        .map_err(|e| UiError::new("capture", e.to_string()))?;

    let started = tauri::async_runtime::spawn_blocking(move || ready_rx.recv())
        .await
        .map_err(|e| UiError::new("capture", e.to_string()))?
        .map_err(|_| UiError::new("capture", "The capture thread stopped unexpectedly."))?
        .map_err(|e| {
            let _ = std::fs::remove_dir_all(&dir);
            UiError::new("capture", e)
        })?;

    let info = MeetingInfo {
        dir: dir.display().to_string(),
        started_at_unix_ms: clock.started_at_unix_ms() as u64,
        mic: started.mic.0.clone(),
        system: started.system.0.clone(),
        model: profile.model.clone(),
    };
    for (key, value) in [
        ("started_at_unix_ms", info.started_at_unix_ms.to_string()),
        ("mic_device", info.mic.clone()),
        ("system_device", info.system.clone()),
        ("speech_model", info.model.clone()),
    ] {
        let _ = timeline.set_meta(key, &value);
    }

    let (events_tx, mut events_rx) = mpsc::unbounded_channel::<PipelineEvent>();
    for (speaker, rate, rx) in [
        (Speaker::Me, started.mic.1, mic_rx),
        (Speaker::Others, started.system.1, sys_rx),
    ] {
        let opts = StreamOptions::new(speaker, rate, profile.model.clone());
        tauri::async_runtime::spawn(transcribe_stream(opts, rx, stt.clone(), events_tx.clone()));
    }
    drop(events_tx);

    // Owns the timeline: stores lines and forwards every event to the UI.
    let finished_app = app.clone();
    let summary_dir = info.dir.clone();
    tauri::async_runtime::spawn(async move {
        let mut summary = MeetingSummary {
            dir: summary_dir,
            lines: 0,
            failed: 0,
            error: None,
        };
        while let Some(event) = events_rx.recv().await {
            match &event {
                PipelineEvent::Transcribed { line, .. } => {
                    summary.lines += 1;
                    if let Err(e) = timeline.add_transcript(line) {
                        summary.error = Some(format!("Could not save the transcript: {e}"));
                    }
                }
                PipelineEvent::Failed { .. } => summary.failed += 1,
                _ => {}
            }
            let _ = app.emit("meeting-event", &event);
        }
        // Every stream has ended, so the capture thread is sending its stats.
        let stats = tauri::async_runtime::spawn_blocking(move || {
            stats_rx.recv_timeout(std::time::Duration::from_secs(10))
        })
        .await;
        if let Ok(Ok(stats)) = stats
            && let Ok(json) = serde_json::to_string(&stats)
        {
            let _ = timeline.set_meta("audio_stats", &json);
        }
        let _ = timeline.set_meta("stopped_at_unix_ms", &now_unix_ms().to_string());
        drop(timeline);
        let meetings = finished_app.state::<MeetingState>();
        meetings.running.lock().unwrap().take();
        let _ = finished_app.emit("meeting-finished", &summary);
    });

    *meetings.running.lock().unwrap() = Some(Running {
        stop: stop_tx,
        info: info.clone(),
        stopping: false,
    });
    Ok(info)
}

/// Stops capture. Transcription of the last utterances continues, and
/// `meeting-finished` is emitted when it is done.
#[tauri::command]
pub fn stop_meeting(meetings: State<'_, MeetingState>) -> Result<(), UiError> {
    let mut running = meetings.running.lock().unwrap();
    let r = running
        .as_mut()
        .ok_or_else(|| UiError::new("not_running", "No meeting is running."))?;
    r.stopping = true;
    let _ = r.stop.send(());
    Ok(())
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
