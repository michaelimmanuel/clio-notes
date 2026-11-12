//! SQLite timeline, keyframe image files and notes export.
//!
//! Each meeting lives in its own folder (`meetings/meeting-<unix seconds>/`)
//! holding `timeline.sqlite`, the recorded audio and, later, images and notes.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("timeline database error: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, StorageError>;

/// Who said a line.
///
/// v1 knows only which stream the audio came from. Stored as text so named
/// speakers (see PLAN.md, "Feature: speaker identification") can be added
/// without a schema change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Speaker {
    /// The local microphone.
    Me,
    /// Everyone heard through system audio.
    Others,
}

impl Speaker {
    pub fn as_str(&self) -> &'static str {
        match self {
            Speaker::Me => "me",
            Speaker::Others => "others",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Speaker::Me => "Me",
            Speaker::Others => "Others",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "me" => Speaker::Me,
            _ => Speaker::Others,
        }
    }
}

/// One transcribed utterance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptLine {
    pub speaker: Speaker,
    /// Session time, from the shared capture clock.
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
    /// The model that produced the text.
    pub model: String,
}

const SCHEMA_VERSION: i64 = 1;

/// The event log of one meeting.
pub struct Timeline {
    conn: Connection,
}

impl Timeline {
    /// Opens or creates the timeline at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        // WAL keeps already-written lines safe if the app is killed mid-meeting.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(StorageError::Other(format!(
                "{} was written by a newer version of Clio Notes",
                path.display()
            )));
        }
        if version < 1 {
            conn.execute_batch(
                "CREATE TABLE meta (
                     key   TEXT PRIMARY KEY,
                     value TEXT NOT NULL
                 );
                 CREATE TABLE transcript (
                     id         INTEGER PRIMARY KEY,
                     speaker    TEXT    NOT NULL,
                     start_ms   INTEGER NOT NULL,
                     end_ms     INTEGER NOT NULL,
                     text       TEXT    NOT NULL,
                     model      TEXT    NOT NULL,
                     created_at INTEGER NOT NULL
                 );
                 CREATE INDEX transcript_start ON transcript (start_ms);
                 PRAGMA user_version = 1;",
            )?;
        }
        Ok(Self { conn })
    }

    pub fn add_transcript(&self, line: &TranscriptLine) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO transcript (speaker, start_ms, end_ms, text, model, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                line.speaker.as_str(),
                line.start_ms as i64,
                line.end_ms as i64,
                line.text,
                line.model,
                unix_ms() as i64,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Every line, in the order it was spoken.
    pub fn transcript(&self) -> Result<Vec<TranscriptLine>> {
        let mut stmt = self.conn.prepare(
            "SELECT speaker, start_ms, end_ms, text, model FROM transcript
             ORDER BY start_ms, id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(TranscriptLine {
                speaker: Speaker::parse(&r.get::<_, String>(0)?),
                start_ms: r.get::<_, i64>(1)? as u64,
                end_ms: r.get::<_, i64>(2)? as u64,
                text: r.get(3)?,
                model: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }
}

/// The folder holding every meeting: `<data dir>/com.clionotes.app/meetings`,
/// or `$CLIO_NOTES_DATA_DIR/meetings` when that is set.
pub fn meetings_root() -> Result<PathBuf> {
    let base = match std::env::var_os("CLIO_NOTES_DATA_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => dirs::data_dir()
            .ok_or_else(|| StorageError::Other("cannot find the app data folder".into()))?
            .join("com.clionotes.app"),
    };
    Ok(base.join("meetings"))
}

/// Creates a new, empty meeting folder under `root`.
pub fn create_meeting_dir(root: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(root)?;
    let secs = unix_ms() / 1000;
    for n in 0..100 {
        let name = if n == 0 {
            format!("meeting-{secs}")
        } else {
            format!("meeting-{secs}-{n}")
        };
        let dir = root.join(name);
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(StorageError::Other(
        "cannot pick a meeting folder name".into(),
    ))
}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "clio-storage-{test}-{}-{}",
            std::process::id(),
            unix_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn transcript_round_trips_in_spoken_order() {
        let dir = temp_dir("transcript");
        let path = dir.join("timeline.sqlite");
        let line = |speaker, start_ms, text: &str| TranscriptLine {
            speaker,
            start_ms,
            end_ms: start_ms + 900,
            text: text.into(),
            model: "m".into(),
        };
        {
            let t = Timeline::open(&path).unwrap();
            // Results arrive out of order when requests run in parallel.
            t.add_transcript(&line(Speaker::Others, 5000, "second"))
                .unwrap();
            t.add_transcript(&line(Speaker::Me, 1000, "first")).unwrap();
            t.set_meta("started_at_unix_ms", "1").unwrap();
            t.set_meta("started_at_unix_ms", "2").unwrap();
        }
        let t = Timeline::open(&path).unwrap();
        let lines = t.transcript().unwrap();
        assert_eq!(
            lines,
            vec![
                line(Speaker::Me, 1000, "first"),
                line(Speaker::Others, 5000, "second")
            ]
        );
        assert_eq!(t.meta("started_at_unix_ms").unwrap().as_deref(), Some("2"));
        assert_eq!(t.meta("missing").unwrap(), None);
        drop(t);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn meeting_dirs_are_unique() {
        let root = temp_dir("dirs");
        let a = create_meeting_dir(&root).unwrap();
        let b = create_meeting_dir(&root).unwrap();
        assert_ne!(a, b);
        std::fs::remove_dir_all(root).unwrap();
    }
}
