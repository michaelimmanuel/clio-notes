//! Tauri commands that start and stop a meeting: capture, live transcription,
//! the screen pipeline and the timeline.
//!
//! The UI listens for three events:
//! - `meeting-event`: a [`PipelineEvent`] for each utterance as it moves
//!   from detected to transcribed (or empty, or failed);
//! - `meeting-screen`: a [`ScreenEvent`] for each keyframe and its reading;
//! - `meeting-finished`: a [`MeetingSummary`] once capture has stopped and
//!   every pending request has finished.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc as std_mpsc;

use capture::{
    AudioRecorder, AudioSource, AudioStats, ScreenRecorder, ScreenStats, ScreenTarget, SessionClock,
};
use pipeline::screen;
use pipeline::{
    Frame, PipelineEvent, ScreenEvent, ScreenOptions, StreamOptions, screen_stream,
    transcribe_stream,
};
use providers::{ProviderClient, Role, TextGenerator};
use serde::Serialize;
use storage::{Speaker, Timeline};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::mpsc;

use crate::settings::{AppState, UiError};

/// Frames per second taken from the screen. Slides and most demos change
/// far slower than this.
const SCREEN_FPS: f64 = 1.0;

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
    /// The captured display, or `None` when screen capture is off.
    screen: Option<String>,
    /// The screen-reading model, or `None` when no profile is assigned.
    vision_model: Option<String>,
    read_cap: Option<u32>,
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
    keyframes: usize,
    reads: usize,
    error: Option<String>,
}

/// What the capture thread reports once every recorder is running.
struct Started {
    mic: (String, u32),
    system: (String, u32),
    screen: Option<String>,
}

#[derive(Serialize)]
struct CaptureStats {
    audio: Vec<AudioStats>,
    screen: Option<ScreenStats>,
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

    let capture = crate::devices::load(&state)?;
    let (inputs, monitors) = crate::devices::connected().await?;
    if let Some(name) = &capture.microphone
        && !inputs.iter().any(|d| &d.name == name)
    {
        return Err(UiError::new(
            "device_missing",
            format!(
                "The microphone \"{name}\" chosen in Settings is not connected. \
                 Plug it in or choose another one."
            ),
        ));
    }
    let screen_target = if capture.record_screen {
        let index = match &capture.display {
            None => None,
            Some(choice) => Some(choice.resolve(&monitors).ok_or_else(|| {
                UiError::new(
                    "device_missing",
                    format!(
                        "The display \"{}\" chosen in Settings is not connected. \
                         Choose another one in Settings.",
                        choice.name
                    ),
                )
            })?),
        };
        Some(ScreenTarget::Monitor(index))
    } else {
        None
    };
    // Screen reading is optional: without a profile, keyframes are still
    // kept for the notes, just not read.
    let vision: Option<(String, Arc<dyn TextGenerator>)> = match settings.assigned(Role::Vision) {
        Some(p) if screen_target.is_some() => Some((
            p.model.clone(),
            ProviderClient::for_profile(p, &state.keys, &state.http)?.into_text_generator(),
        )),
        _ => None,
    };

    let root = storage::meetings_root().map_err(|e| UiError::new("storage", e.to_string()))?;
    let dir =
        storage::create_meeting_dir(&root).map_err(|e| UiError::new("storage", e.to_string()))?;
    let timeline = Timeline::open(&dir.join("timeline.sqlite"))
        .map_err(|e| UiError::new("storage", e.to_string()))?;

    let clock = SessionClock::start();
    let (mic_tx, mic_rx) = mpsc::unbounded_channel::<Vec<f32>>();
    let (sys_tx, sys_rx) = mpsc::unbounded_channel::<Vec<f32>>();
    // Small on purpose: when the screen pipeline is busy, frames are dropped
    // at capture instead of piling up (a 4K frame is 33 MB).
    let (frame_tx, frame_rx) = mpsc::channel::<Frame>(2);
    let (stop_tx, stop_rx) = std_mpsc::channel::<()>();
    let (ready_tx, ready_rx) = std_mpsc::channel::<Result<Started, String>>();
    let (stats_tx, stats_rx) = std_mpsc::channel::<CaptureStats>();

    // cpal streams are owned by one thread from start to stop.
    let capture_dir = dir.clone();
    let microphone = capture.microphone.clone();
    let target = screen_target.clone();
    std::thread::Builder::new()
        .name("meeting-capture".into())
        .spawn(move || {
            let start = |source, file: &str, tx: mpsc::UnboundedSender<Vec<f32>>| {
                let device = match source {
                    AudioSource::Microphone => microphone.as_deref(),
                    AudioSource::SystemAudio => None,
                };
                AudioRecorder::start_with_tap(
                    source,
                    device,
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
            let screen = match target {
                None => None,
                Some(target) => {
                    let tap = Box::new(move |t_ms, image| {
                        // Full means the pipeline is busy: skip this frame.
                        let _ = frame_tx.try_send(Frame { t_ms, image });
                    });
                    match ScreenRecorder::start_with_tap(target, None, SCREEN_FPS, clock, Some(tap))
                    {
                        Ok(r) => Some(r),
                        Err(e) => {
                            let _ = mic.stop();
                            let _ = system.stop();
                            let _ = ready_tx.send(Err(format!("Cannot capture the screen: {e:#}")));
                            return;
                        }
                    }
                }
            };
            let _ = ready_tx.send(Ok(Started {
                mic: (mic.device_name().to_string(), mic.sample_rate()),
                system: (system.device_name().to_string(), system.sample_rate()),
                screen: screen.as_ref().map(|s| s.target_name().to_string()),
            }));
            // Runs until stop_meeting, or until the app drops the sender.
            let _ = stop_rx.recv();
            // Stopping ends each writer thread, which drops its tap and so
            // closes the pipeline's input once the last samples are in.
            let audio = [mic.stop(), system.stop()]
                .into_iter()
                .filter_map(|r| r.map_err(|e| eprintln!("stopping capture: {e:#}")).ok())
                .collect();
            let screen = screen.and_then(|s| {
                s.stop()
                    .map_err(|e| eprintln!("stopping screen capture: {e:#}"))
                    .ok()
            });
            let _ = stats_tx.send(CaptureStats { audio, screen });
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
        screen: started.screen.clone(),
        vision_model: vision.as_ref().map(|(model, _)| model.clone()),
        read_cap: capture.screen_read_cap,
    };
    for (key, value) in [
        ("started_at_unix_ms", info.started_at_unix_ms.to_string()),
        ("mic_device", info.mic.clone()),
        ("system_device", info.system.clone()),
        ("speech_model", info.model.clone()),
        ("screen", info.screen.clone().unwrap_or_default()),
        (
            "vision_model",
            info.vision_model.clone().unwrap_or_default(),
        ),
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

    let (screen_tx, mut screen_rx) = mpsc::unbounded_channel::<ScreenEvent>();
    if started.screen.is_some() {
        let opts = ScreenOptions::new(dir.clone(), capture.screen_mode, capture.screen_read_cap);
        tauri::async_runtime::spawn(screen_stream(
            opts,
            frame_rx,
            vision.map(|(_, reader)| reader),
            screen_tx,
        ));
        // Keeps this window out of its own screenshots, so live transcript
        // lines do not count as screen changes.
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.set_content_protected(true);
        }
    } else {
        drop(screen_tx);
    }

    // Owns the timeline: stores lines and keyframes and forwards every event
    // to the UI.
    let finished_app = app.clone();
    let summary_dir = info.dir.clone();
    tauri::async_runtime::spawn(async move {
        let mut summary = MeetingSummary {
            dir: summary_dir,
            lines: 0,
            failed: 0,
            keyframes: 0,
            reads: 0,
            error: None,
        };
        let (mut speech_open, mut screen_open) = (true, true);
        while speech_open || screen_open {
            tokio::select! {
                event = events_rx.recv(), if speech_open => {
                    let Some(event) = event else {
                        speech_open = false;
                        continue;
                    };
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
                event = screen_rx.recv(), if screen_open => {
                    let Some(event) = event else {
                        screen_open = false;
                        continue;
                    };
                    match &event {
                        ScreenEvent::Keyframe { repeat_of: None, .. } => summary.keyframes += 1,
                        ScreenEvent::Read { .. } => summary.reads += 1,
                        _ => {}
                    }
                    if let Err(e) = screen::record(&timeline, &event) {
                        summary.error = Some(format!("Could not save a keyframe: {e}"));
                    }
                    let _ = app.emit("meeting-screen", &event);
                }
            }
        }
        // Every stream has ended, so the capture thread is sending its stats.
        let stats = tauri::async_runtime::spawn_blocking(move || {
            stats_rx.recv_timeout(std::time::Duration::from_secs(10))
        })
        .await;
        if let Ok(Ok(stats)) = stats {
            if let Ok(json) = serde_json::to_string(&stats.audio) {
                let _ = timeline.set_meta("audio_stats", &json);
            }
            if let Some(screen) = stats.screen
                && let Ok(json) = serde_json::to_string(&screen)
            {
                let _ = timeline.set_meta("screen_stats", &json);
            }
        }
        let _ = timeline.set_meta("stopped_at_unix_ms", &now_unix_ms().to_string());
        drop(timeline);
        if let Some(window) = finished_app.get_webview_window("main") {
            let _ = window.set_content_protected(false);
        }
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

/// Stops capture. Transcription of the last utterances and reading of the
/// last keyframes continue, and `meeting-finished` is emitted when they are
/// done.
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
