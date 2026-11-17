//! Searching every meeting, and questions across meetings, on the Meeting
//! notes list.
//!
//! The search index is brought up to date before each search or question,
//! and by a background worker at startup, when woken and every few minutes;
//! the meeting being recorded is left out. While a question is answered,
//! the UI gets one `library-found` event with the meetings being read, then
//! `meeting-answer` events with id `*` and each new piece of the answer.
//!
//! Search by meaning is optional. When a profile is assigned to it, the
//! worker also embeds every piece of every meeting that has no vector yet,
//! and questions search by meaning as well as by words. Settings follows the
//! worker through `meaning-status` events ([`MeaningStatus`]).
//!
//! Questions and answers are kept in the library file, and only counts and
//! timings are logged.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pipeline::library::{self, SearchResult};
use providers::{Embedder, ProviderClient, Role};
use serde::Serialize;
use storage::{Library, LibraryChatRow, MeetingRef, VectorProgress};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::Notify;

use crate::meetings::{recording_id, writer};
use crate::settings::{AppState, UiError};

/// The id `meeting-answer` events carry for answers across meetings.
const ALL: &str = "*";
/// How often the worker looks for changed meetings without being woken.
const CHECK_EVERY: Duration = Duration::from_secs(5 * 60);
/// How long the worker waits after search by meaning failed.
const RETRY_AFTER: Duration = Duration::from_secs(15 * 60);

/// How search by meaning is doing, as Settings shows it.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct MeaningStatus {
    /// off, indexing, ready or error.
    state: &'static str,
    /// The embedding model, when one is assigned.
    model: Option<String>,
    progress: VectorProgress,
    error: Option<String>,
}

pub struct LibraryState {
    /// A question across meetings is being answered.
    asking: AtomicBool,
    /// Held while the index is brought up to date, so it is done once at a
    /// time.
    syncing: Mutex<()>,
    meaning: Mutex<MeaningStatus>,
    /// Wakes the background worker.
    wake: Notify,
}

impl Default for LibraryState {
    fn default() -> Self {
        Self {
            asking: AtomicBool::new(false),
            syncing: Mutex::new(()),
            meaning: Mutex::new(MeaningStatus {
                state: "off",
                ..MeaningStatus::default()
            }),
            wake: Notify::new(),
        }
    }
}

impl LibraryState {
    /// Asks the background worker to look for work now: after settings
    /// change or a meeting's notes are written.
    pub fn wake(&self) {
        self.wake.notify_one();
    }
}

/// Clears [`LibraryState::asking`] however the command ends.
struct Claim<'a>(&'a AtomicBool);

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

#[derive(Debug, Clone, Serialize)]
struct Found<'a> {
    sources: &'a [MeetingRef],
    /// Meetings read with their notes.
    reading: usize,
}

#[derive(Debug, Clone, Serialize)]
struct AnswerText<'a> {
    id: &'a str,
    text: &'a str,
}

fn storage_error(e: impl ToString) -> UiError {
    UiError::new("storage", e.to_string())
}

fn open() -> Result<Library, UiError> {
    Library::open(&storage::library_path().map_err(storage_error)?).map_err(storage_error)
}

/// Brings the index up to date, leaving out meeting `skip`.
fn sync_blocking(app: &AppHandle, skip: Option<String>) -> Result<(), UiError> {
    let state = app.state::<LibraryState>();
    let _guard = state.syncing.lock().unwrap_or_else(|e| e.into_inner());
    let root = storage::meetings_root().map_err(storage_error)?;
    let mut library = open()?;
    let started = Instant::now();
    let report = library::sync(&mut library, &root, skip.as_deref()).map_err(storage_error)?;
    if report.indexed + report.removed + report.failed > 0 {
        log::info!(
            target: "clio::library",
            "Search index updated in {:.1} s: {} meeting(s) indexed, {} removed, {} failed",
            started.elapsed().as_secs_f64(),
            report.indexed,
            report.removed,
            report.failed
        );
    }
    // New text may need vectors.
    if report.indexed > 0 {
        state.wake();
    }
    Ok(())
}

async fn synced(app: &AppHandle) -> Result<(), UiError> {
    let app = app.clone();
    let skip = recording_id(&app);
    tauri::async_runtime::spawn_blocking(move || sync_blocking(&app, skip))
        .await
        .map_err(storage_error)?
}

// ---- Search by meaning ----

/// The profile assigned to search by meaning.
struct Meaning {
    profile_id: String,
    /// The library keeps vectors under this name.
    model: String,
    embedder: Arc<dyn Embedder>,
}

fn embedder(state: &AppState) -> Result<Option<Meaning>, UiError> {
    let settings = state.store.load()?;
    let Some(profile) = settings.assigned(Role::Embeddings) else {
        return Ok(None);
    };
    let client = ProviderClient::for_profile(profile, &state.keys, &state.http)?;
    let embedder = client.into_embedder().ok_or_else(|| {
        UiError::new(
            "config",
            format!("\"{}\" cannot make embeddings.", profile.name),
        )
    })?;
    Ok(Some(Meaning {
        profile_id: profile.id.clone(),
        model: profile.model.clone(),
        embedder,
    }))
}

fn set_meaning(app: &AppHandle, status: MeaningStatus) {
    let state = app.state::<LibraryState>();
    let mut current = state.meaning.lock().unwrap_or_else(|e| e.into_inner());
    if *current != status {
        *current = status.clone();
        let _ = app.emit("meaning-status", status);
    }
}

fn meaning_status(app: &AppHandle) -> MeaningStatus {
    app.state::<LibraryState>()
        .meaning
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// One round of the worker: brings the index up to date, then embeds what
/// has no vector yet, if search by meaning is on.
async fn work(app: &AppHandle) {
    if let Err(e) = synced(app).await {
        log::warn!(target: "clio::library", "Could not update the search index: {}", e.message());
    }
    let state = app.state::<AppState>();
    let Meaning {
        profile_id,
        model,
        embedder,
    } = match embedder(&state) {
        Ok(Some(found)) => found,
        Ok(None) => {
            return set_meaning(
                app,
                MeaningStatus {
                    state: "off",
                    ..MeaningStatus::default()
                },
            );
        }
        Err(e) => {
            return set_meaning(
                app,
                MeaningStatus {
                    state: "error",
                    error: Some(e.message().to_string()),
                    ..MeaningStatus::default()
                },
            );
        }
    };
    let status =
        |state: &'static str, progress: VectorProgress, error: Option<String>| MeaningStatus {
            state,
            model: Some(model.clone()),
            progress,
            error,
        };

    // Vectors from another model, or of text that is gone, are dropped.
    let m = model.clone();
    let progress = tauri::async_runtime::spawn_blocking(move || {
        let library = open()?;
        library.prune_vectors(&m).map_err(storage_error)?;
        library.vector_progress(&m).map_err(storage_error)
    })
    .await
    .map_err(storage_error)
    .and_then(|r| r);
    let progress = match progress {
        Ok(p) => p,
        Err(e) => {
            return set_meaning(
                app,
                status(
                    "error",
                    VectorProgress::default(),
                    Some(e.message().to_string()),
                ),
            );
        }
    };
    if progress.pieces_done >= progress.pieces {
        return set_meaning(app, status("ready", progress, None));
    }
    set_meaning(app, status("indexing", progress, None));

    let Ok(library_path) = storage::library_path() else {
        return;
    };
    // Stops when the job is given to another profile or model.
    let keep_going = || {
        state.store.load().ok().is_some_and(|s| {
            s.assigned(Role::Embeddings)
                .is_some_and(|p| p.id == profile_id && p.model == model)
        })
    };
    let on_batch = |p: &VectorProgress| set_meaning(app, status("indexing", p.clone(), None));
    let started = Instant::now();
    let result = library::embed_missing(
        &library_path,
        embedder.as_ref(),
        &model,
        &keep_going,
        &on_batch,
    )
    .await;
    let progress = meaning_status(app).progress;
    match result {
        Ok(report) => {
            if report.embedded + report.skipped > 0 {
                log::info!(
                    target: "clio::library",
                    "Embedded {} piece(s) with {model} in {:.1} s ({} skipped, {} tokens)",
                    report.embedded,
                    started.elapsed().as_secs_f64(),
                    report.skipped,
                    report.usage.input_tokens
                );
            }
            let done = progress.pieces_done >= progress.pieces;
            set_meaning(
                app,
                status(if done { "ready" } else { "indexing" }, progress, None),
            );
        }
        Err(message) => {
            log::warn!(target: "clio::library", "Search by meaning stopped: {message}");
            set_meaning(app, status("error", progress, Some(message)));
        }
    }
}

/// Keeps the index, and the vectors when search by meaning is on, up to
/// date for as long as the app runs.
pub fn start_worker(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        loop {
            work(&app).await;
            let wait = if meaning_status(&app).state == "error" {
                RETRY_AFTER
            } else {
                CHECK_EVERY
            };
            let state = app.state::<LibraryState>();
            let _ = tokio::time::timeout(wait, state.wake.notified()).await;
        }
    });
}

/// How search by meaning is doing.
#[tauri::command]
pub fn get_meaning_status(app: AppHandle) -> MeaningStatus {
    meaning_status(&app)
}

/// Tries search by meaning again now, after an error.
#[tauri::command]
pub fn retry_meaning(library_state: State<'_, LibraryState>) {
    library_state.wake();
}

// ---- Search and questions ----

/// The meetings with every word of `query` in them, with their best
/// matching parts.
#[tauri::command]
pub async fn search_meetings(app: AppHandle, query: String) -> Result<Vec<SearchResult>, UiError> {
    if query.trim().is_empty() {
        return Ok(Vec::new());
    }
    synced(&app).await?;
    tauri::async_runtime::spawn_blocking(move || {
        library::search(&open()?, &query).map_err(storage_error)
    })
    .await
    .map_err(storage_error)?
}

/// The questions asked across meetings, oldest first.
#[tauri::command]
pub async fn library_chat() -> Result<Vec<LibraryChatRow>, UiError> {
    tauri::async_runtime::spawn_blocking(|| open()?.chat().map_err(storage_error))
        .await
        .map_err(storage_error)?
}

#[tauri::command]
pub async fn ask_all_meetings(
    app: AppHandle,
    state: State<'_, AppState>,
    library_state: State<'_, LibraryState>,
    question: String,
) -> Result<LibraryChatRow, UiError> {
    let Some(generator) = writer(&state)? else {
        return Err(UiError::new(
            "config",
            "No profile is assigned to notes writing. Choose one in Settings > Jobs.",
        ));
    };
    if library_state.asking.swap(true, Ordering::SeqCst) {
        return Err(UiError::new(
            "busy",
            "A question about your meetings is being answered.",
        ));
    }
    let _claim = Claim(&library_state.asking);
    synced(&app).await?;
    let library_path = storage::library_path().map_err(storage_error)?;
    let root = storage::meetings_root().map_err(storage_error)?;

    // By meaning only once some vectors exist; a problem with the embedding
    // profile leaves the question to its words.
    let has_vectors = meaning_status(&app).progress.pieces_done > 0;
    let meaning = match embedder(&state) {
        Ok(Some(m)) if has_vectors => Some((m.model, m.embedder)),
        Ok(_) => None,
        Err(e) => {
            log::warn!(target: "clio::library", "Search by meaning is unavailable: {}", e.message());
            None
        }
    };
    let by_meaning = meaning
        .as_ref()
        .map(|(model, embedder)| (embedder.as_ref(), model.as_str()));

    let on_found = |sources: &[MeetingRef], reading: usize| {
        let _ = app.emit("library-found", Found { sources, reading });
    };
    let on_text = |text: &str| {
        let _ = app.emit("meeting-answer", AnswerText { id: ALL, text });
    };
    let started = Instant::now();
    match library::ask_all(
        &library_path,
        &root,
        generator.as_ref(),
        by_meaning,
        &question,
        &on_found,
        &on_text,
    )
    .await
    {
        Ok(answer) => {
            log::info!(
                target: "clio::library",
                "Answered a question across meetings in {:.1} s with {}: {} search word(s){}, \
                 {} meeting(s) read, {} named ({} in / {} out tokens)",
                started.elapsed().as_secs_f64(),
                answer.row.model,
                answer.plan.terms.len(),
                if answer.by_meaning { " and by meaning" } else { "" },
                answer.meetings_read,
                answer.row.sources.len(),
                answer.usage.input_tokens,
                answer.usage.output_tokens
            );
            Ok(answer.row)
        }
        Err(message) => {
            log::warn!(target: "clio::library", "Question across meetings failed: {message}");
            Err(UiError::new("ask", message))
        }
    }
}

/// Deletes every question asked across meetings.
#[tauri::command]
pub async fn clear_library_chat(library_state: State<'_, LibraryState>) -> Result<(), UiError> {
    if library_state.asking.load(Ordering::SeqCst) {
        return Err(UiError::new(
            "busy",
            "Wait for the answer before clearing the chat.",
        ));
    }
    tauri::async_runtime::spawn_blocking(|| open()?.clear_chat().map_err(storage_error))
        .await
        .map_err(storage_error)?
}
