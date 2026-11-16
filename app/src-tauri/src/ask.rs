//! Questions about a past meeting, asked on its page under Meeting notes.
//!
//! The answer is written by the model assigned to text generation. While it
//! is written, the UI gets `meeting-answer` events with each new piece of
//! text; the command returns the stored question and answer when done.
//! Questions and answers are kept in the meeting's timeline, and only
//! counts and timings are logged.

use std::collections::HashSet;
use std::sync::Mutex;

use pipeline::ask;
use serde::Serialize;
use storage::{ChatRow, Timeline};
use tauri::{AppHandle, Emitter, State};

use crate::meetings::{add_usage, meeting_dir, recording_id, writer};
use crate::settings::{AppState, UiError};

/// Meetings with a question being answered.
#[derive(Default)]
pub struct Asking(Mutex<HashSet<String>>);

/// A new piece of the answer being written for meeting `id`.
#[derive(Debug, Clone, Serialize)]
struct AnswerText<'a> {
    id: &'a str,
    text: &'a str,
}

/// Releases a meeting from [`Asking`] however the command ends.
struct Claim<'a> {
    asking: &'a Asking,
    id: String,
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        self.asking.0.lock().unwrap().remove(&self.id);
    }
}

/// The questions asked about meeting `id`, oldest first.
#[tauri::command]
pub async fn chat_history(id: String) -> Result<Vec<ChatRow>, UiError> {
    let dir = meeting_dir(&id)?;
    tauri::async_runtime::spawn_blocking(move || {
        Timeline::open(&dir.join("timeline.sqlite"))
            .and_then(|t| t.chat())
            .map_err(|e| UiError::new("storage", e.to_string()))
    })
    .await
    .map_err(|e| UiError::new("storage", e.to_string()))?
}

#[tauri::command]
pub async fn ask_meeting(
    app: AppHandle,
    state: State<'_, AppState>,
    asking: State<'_, Asking>,
    id: String,
    question: String,
) -> Result<ChatRow, UiError> {
    let dir = meeting_dir(&id)?;
    if recording_id(&app).as_deref() == Some(id.as_str()) {
        return Err(UiError::new(
            "busy",
            "Stop the meeting before asking about it.",
        ));
    }
    let Some(generator) = writer(&state)? else {
        return Err(UiError::new(
            "config",
            "No profile is assigned to notes writing. Choose one in Settings > Jobs.",
        ));
    };
    if !asking.0.lock().unwrap().insert(id.clone()) {
        return Err(UiError::new(
            "busy",
            "A question about this meeting is being answered.",
        ));
    }
    let _claim = Claim {
        asking: &asking,
        id: id.clone(),
    };

    let on_text = |text: &str| {
        let _ = app.emit("meeting-answer", AnswerText { id: &id, text });
    };
    let started = std::time::Instant::now();
    let result = ask::ask(&dir, generator.as_ref(), &question, &on_text).await;
    add_usage(&dir, |u| match &result {
        Ok(answer) => {
            u.questions.calls += 1;
            u.questions.input_tokens += answer.usage.input_tokens;
            u.questions.output_tokens += answer.usage.output_tokens;
        }
        Err(_) => u.questions.failed += 1,
    });
    match result {
        Ok(answer) => {
            log::info!(
                target: "clio::ask",
                "Meeting {id}: answered a question in {:.1} s with {} ({} in / {} out tokens){}",
                started.elapsed().as_secs_f64(),
                answer.row.model,
                answer.usage.input_tokens,
                answer.usage.output_tokens,
                if answer.shortened { ", transcript shortened to fit" } else { "" }
            );
            Ok(answer.row)
        }
        Err(message) => {
            log::warn!(target: "clio::ask", "Meeting {id}: question failed: {message}");
            Err(UiError::new("ask", message))
        }
    }
}

/// Deletes every question and answer of meeting `id`.
#[tauri::command]
pub async fn clear_chat(asking: State<'_, Asking>, id: String) -> Result<(), UiError> {
    let dir = meeting_dir(&id)?;
    if asking.0.lock().unwrap().contains(&id) {
        return Err(UiError::new(
            "busy",
            "Wait for the answer before clearing the chat.",
        ));
    }
    tauri::async_runtime::spawn_blocking(move || {
        Timeline::open(&dir.join("timeline.sqlite"))
            .and_then(|t| t.clear_chat())
            .map_err(|e| UiError::new("storage", e.to_string()))
    })
    .await
    .map_err(|e| UiError::new("storage", e.to_string()))?
}
