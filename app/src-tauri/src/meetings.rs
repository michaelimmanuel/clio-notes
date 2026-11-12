//! Past meetings: list, open, re-run the final pass, open the folder, delete.
//!
//! A meeting is named by its folder (`meeting-<unix seconds>`) under the
//! meetings root. Names from the UI are checked against that pattern and
//! resolved inside the root only.
//!
//! The UI listens for `meeting-final`: a [`FinalEvent`] when the final pass
//! of a meeting starts, finishes or fails.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use pipeline::final_pass::{self, FinalNotes};
use providers::{ProviderClient, Role, TextGenerator};
use serde::Serialize;
use storage::{Timeline, TranscriptLine};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_opener::OpenerExt;

use crate::meeting::MeetingState;
use crate::settings::{AppState, UiError};

/// Meetings whose final pass is running.
#[derive(Default)]
pub struct Finalizing(Mutex<HashSet<String>>);

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FinalEvent {
    Started {
        id: String,
    },
    Done {
        id: String,
        title: String,
        note: Option<String>,
    },
    Failed {
        id: String,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MeetingStatus {
    /// Being recorded now.
    Recording,
    /// The final pass is running.
    Finalizing,
    /// Final notes exist.
    Done,
    /// Stopped, but no final notes yet.
    NoFinal,
    /// Never stopped: the app closed or crashed while it ran.
    Interrupted,
}

#[derive(Debug, Clone, Serialize)]
pub struct MeetingItem {
    id: String,
    dir: String,
    started_at_unix_ms: Option<u64>,
    duration_ms: u64,
    title: Option<String>,
    lines: usize,
    keyframes: usize,
    sections: usize,
    status: MeetingStatus,
}

#[derive(Debug, Clone, Serialize)]
pub struct LiveSectionView {
    start_ms: u64,
    end_ms: u64,
    heading: String,
    bullets: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct KeyframeView {
    t_ms: u64,
    file: String,
    title: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct MeetingDetail {
    item: MeetingItem,
    final_notes: Option<FinalNotes>,
    sections: Vec<LiveSectionView>,
    transcript: Vec<TranscriptLine>,
    keyframes: Vec<KeyframeView>,
}

fn root() -> Result<PathBuf, UiError> {
    storage::meetings_root().map_err(|e| UiError::new("storage", e.to_string()))
}

fn valid_id(id: &str) -> bool {
    let Some(rest) = id.strip_prefix("meeting-") else {
        return false;
    };
    let mut parts = rest.splitn(2, '-');
    let digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    parts.next().is_some_and(digits) && parts.next().is_none_or(digits)
}

/// The folder of meeting `id`, which must exist.
fn meeting_dir(id: &str) -> Result<PathBuf, UiError> {
    if !valid_id(id) {
        return Err(UiError::new("not_found", "There is no such meeting."));
    }
    let dir = root()?.join(id);
    if !dir.join("timeline.sqlite").is_file() {
        return Err(UiError::new("not_found", "This meeting no longer exists."));
    }
    Ok(dir)
}

fn id_of(dir: &Path) -> String {
    dir.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn recording_id(app: &AppHandle) -> Option<String> {
    app.state::<MeetingState>()
        .running_dir()
        .map(|d| id_of(Path::new(&d)))
}

fn status(app: &AppHandle, id: &str, timeline: &Timeline) -> MeetingStatus {
    if recording_id(app).as_deref() == Some(id) {
        return MeetingStatus::Recording;
    }
    if app.state::<Finalizing>().0.lock().unwrap().contains(id) {
        return MeetingStatus::Finalizing;
    }
    if timeline.meta("final_notes").ok().flatten().is_some() {
        MeetingStatus::Done
    } else if timeline.meta("stopped_at_unix_ms").ok().flatten().is_some() {
        MeetingStatus::NoFinal
    } else {
        MeetingStatus::Interrupted
    }
}

fn item(app: &AppHandle, dir: &Path) -> Result<(MeetingItem, Timeline), String> {
    let timeline = Timeline::open(&dir.join("timeline.sqlite")).map_err(|e| e.to_string())?;
    let input = final_pass::load(dir)?;
    let id = id_of(dir);
    let title = final_pass::stored(&timeline)
        .map(|n| n.title)
        .or_else(|| input.sections.first().map(|s| s.heading.clone()));
    let item = MeetingItem {
        status: status(app, &id, &timeline),
        id,
        dir: dir.display().to_string(),
        started_at_unix_ms: input.started_at_unix_ms,
        duration_ms: input.duration_ms,
        title,
        lines: input.transcript.len(),
        keyframes: input
            .keyframes
            .iter()
            .filter(|k| k.repeat_of.is_none())
            .count(),
        sections: input.sections.len(),
    };
    Ok((item, timeline))
}

#[tauri::command]
pub async fn list_meetings(app: AppHandle) -> Result<Vec<MeetingItem>, UiError> {
    let root = root()?;
    tauri::async_runtime::spawn_blocking(move || {
        let Ok(entries) = std::fs::read_dir(&root) else {
            return Ok(Vec::new());
        };
        let mut items: Vec<MeetingItem> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| valid_id(&id_of(p)) && p.join("timeline.sqlite").is_file())
            .filter_map(|p| item(&app, &p).ok().map(|(i, _)| i))
            .collect();
        // Newest first; the folder name is the start time in seconds.
        items.sort_by(|a, b| b.id.cmp(&a.id));
        Ok(items)
    })
    .await
    .map_err(|e| UiError::new("storage", e.to_string()))?
}

#[tauri::command]
pub async fn get_meeting(app: AppHandle, id: String) -> Result<MeetingDetail, UiError> {
    let dir = meeting_dir(&id)?;
    tauri::async_runtime::spawn_blocking(move || {
        let (item, timeline) = item(&app, &dir).map_err(|e| UiError::new("storage", e))?;
        let input = final_pass::load(&dir).map_err(|e| UiError::new("storage", e))?;
        let keyframes = input
            .keyframes
            .iter()
            .filter(|k| k.repeat_of.is_none())
            .map(|k| KeyframeView {
                t_ms: k.t_ms,
                file: k.file.clone(),
                title: k
                    .reading
                    .as_deref()
                    .and_then(|r| serde_json::from_str::<pipeline::ScreenReading>(r).ok())
                    .map(|r| {
                        if r.title.is_empty() {
                            r.summary
                        } else {
                            r.title
                        }
                    })
                    .unwrap_or_default(),
            })
            .collect();
        Ok(MeetingDetail {
            final_notes: final_pass::stored(&timeline),
            sections: input
                .sections
                .iter()
                .map(|s| LiveSectionView {
                    start_ms: s.start_ms,
                    end_ms: s.end_ms,
                    heading: s.heading.clone(),
                    bullets: serde_json::to_value(&s.bullets).unwrap_or_default(),
                })
                .collect(),
            transcript: input.transcript,
            keyframes,
            item,
        })
    })
    .await
    .map_err(|e| UiError::new("storage", e.to_string()))?
}

/// A preview of a keyframe or exported image of meeting `id`, as a data URL.
#[tauri::command]
pub async fn meeting_image(
    id: String,
    file: String,
    width: Option<u32>,
) -> Result<String, UiError> {
    let dir = meeting_dir(&id)?;
    let safe = (file.starts_with("keyframes/") || file.starts_with("images/"))
        && !file.contains("..")
        && !file.contains('\\')
        && file.ends_with(".png");
    if !safe {
        return Err(UiError::new("not_found", "There is no such image."));
    }
    let width = width.unwrap_or(640).clamp(64, 1600);
    tauri::async_runtime::spawn_blocking(move || {
        pipeline::screen::thumbnail_of_file(&dir.join(&file), width)
            .map_err(|e| UiError::new("storage", e))
    })
    .await
    .map_err(|e| UiError::new("storage", e.to_string()))?
}

/// The text generator assigned in Settings, if any.
fn writer(state: &AppState) -> Result<Option<std::sync::Arc<dyn TextGenerator>>, UiError> {
    let settings = state.store.load()?;
    Ok(match settings.assigned(Role::Text) {
        Some(p) => {
            Some(ProviderClient::for_profile(p, &state.keys, &state.http)?.into_text_generator())
        }
        None => None,
    })
}

/// Runs the final pass of the meeting in `dir` and reports it with
/// `meeting-final` events. Refuses to run twice at once.
pub async fn run_final_pass(
    app: &AppHandle,
    dir: &Path,
    writer: Option<std::sync::Arc<dyn TextGenerator>>,
) -> Result<FinalNotes, UiError> {
    let id = id_of(dir);
    if !app
        .state::<Finalizing>()
        .0
        .lock()
        .unwrap()
        .insert(id.clone())
    {
        return Err(UiError::new(
            "busy",
            "The final notes of this meeting are being written.",
        ));
    }
    let _ = app.emit("meeting-final", FinalEvent::Started { id: id.clone() });
    let result = final_pass::finalize(dir, writer.as_deref()).await;
    app.state::<Finalizing>().0.lock().unwrap().remove(&id);
    match result {
        Ok((notes, _usage)) => {
            let _ = app.emit(
                "meeting-final",
                FinalEvent::Done {
                    id,
                    title: notes.title.clone(),
                    note: notes.note.clone(),
                },
            );
            Ok(notes)
        }
        Err(message) => {
            let _ = app.emit(
                "meeting-final",
                FinalEvent::Failed {
                    id,
                    message: message.clone(),
                },
            );
            Err(UiError::new("final", message))
        }
    }
}

#[tauri::command]
pub async fn finalize_meeting(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
) -> Result<FinalNotes, UiError> {
    let dir = meeting_dir(&id)?;
    if recording_id(&app).as_deref() == Some(id.as_str()) {
        return Err(UiError::new(
            "busy",
            "Stop the meeting before writing its final notes.",
        ));
    }
    let writer = writer(&state)?;
    run_final_pass(&app, &dir, writer).await
}

#[tauri::command]
pub fn open_meeting_folder(app: AppHandle, id: String) -> Result<(), UiError> {
    let dir = meeting_dir(&id)?;
    app.opener()
        .open_path(dir.display().to_string(), None::<&str>)
        .map_err(|e| UiError::new("open", e.to_string()))
}

/// Moves the meeting folder to the Recycle Bin.
#[tauri::command]
pub fn delete_meeting(app: AppHandle, id: String) -> Result<(), UiError> {
    let dir = meeting_dir(&id)?;
    if recording_id(&app).as_deref() == Some(id.as_str()) {
        return Err(UiError::new("busy", "This meeting is being recorded."));
    }
    if app.state::<Finalizing>().0.lock().unwrap().contains(&id) {
        return Err(UiError::new(
            "busy",
            "Wait until the final notes are written.",
        ));
    }
    trash::delete(&dir).map_err(|e| {
        UiError::new(
            "delete",
            format!("Could not move the meeting to the Recycle Bin: {e}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_meeting_folder_names_are_accepted() {
        assert!(valid_id("meeting-1794412833"));
        assert!(valid_id("meeting-1794412833-2"));
        for bad in [
            "",
            "meeting-",
            "meeting-12a",
            "meeting-1-",
            "meeting-1-2-3",
            "../meeting-1",
            "meeting-1/..",
            "meeting-1\\..",
            "settings",
        ] {
            assert!(!valid_id(bad), "{bad}");
        }
    }
}
