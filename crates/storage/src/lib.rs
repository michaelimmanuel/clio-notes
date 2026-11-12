//! SQLite timeline of a meeting: transcript lines and screen keyframes.
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

/// One keyframe: a screen state worth keeping, and what was read from it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeyframeRow {
    /// Session time when this screen first appeared.
    pub t_ms: u64,
    /// Image path relative to the meeting folder.
    pub file: String,
    pub width: u32,
    pub height: u32,
    /// first, settled or moving.
    pub reason: String,
    /// Share of the screen that changed from the previous keyframe (0–1).
    pub change: f64,
    /// Set when the screen returned to the keyframe from this time.
    pub repeat_of: Option<u64>,
    /// pending, read, failed, or a skipped_* reason.
    pub read_status: String,
    /// The screen reading as JSON, once read.
    pub reading: Option<String>,
    pub model: Option<String>,
    pub read_error: Option<String>,
}

impl KeyframeRow {
    pub fn new(t_ms: u64, file: &str, width: u32, height: u32, reason: &str, change: f64) -> Self {
        Self {
            t_ms,
            file: file.into(),
            width,
            height,
            reason: reason.into(),
            change,
            repeat_of: None,
            read_status: "pending".into(),
            reading: None,
            model: None,
            read_error: None,
        }
    }
}

const SCHEMA_VERSION: i64 = 2;

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
        if version < 2 {
            conn.execute_batch(
                "CREATE TABLE keyframe (
                     t_ms        INTEGER PRIMARY KEY,
                     file        TEXT    NOT NULL,
                     width       INTEGER NOT NULL,
                     height      INTEGER NOT NULL,
                     reason      TEXT    NOT NULL,
                     change      REAL    NOT NULL,
                     repeat_of   INTEGER,
                     read_status TEXT    NOT NULL,
                     reading     TEXT,
                     model       TEXT,
                     read_error  TEXT,
                     created_at  INTEGER NOT NULL
                 );
                 PRAGMA user_version = 2;",
            )?;
        }
        Ok(Self { conn })
    }

    pub fn add_keyframe(&self, k: &KeyframeRow) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO keyframe (t_ms, file, width, height, reason, change, repeat_of,
                 read_status, reading, model, read_error, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                k.t_ms as i64,
                k.file,
                k.width,
                k.height,
                k.reason,
                k.change,
                k.repeat_of.map(|t| t as i64),
                k.read_status,
                k.reading,
                k.model,
                k.read_error,
                unix_ms() as i64,
            ],
        )?;
        Ok(())
    }

    /// Records the outcome of reading the keyframe at `t_ms`.
    pub fn set_keyframe_read(
        &self,
        t_ms: u64,
        status: &str,
        reading: Option<&str>,
        model: Option<&str>,
        error: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE keyframe SET read_status = ?2, reading = ?3, model = ?4, read_error = ?5
             WHERE t_ms = ?1",
            params![t_ms as i64, status, reading, model, error],
        )?;
        Ok(())
    }

    /// Every keyframe, in time order.
    pub fn keyframes(&self) -> Result<Vec<KeyframeRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT t_ms, file, width, height, reason, change, repeat_of, read_status, reading,
                    model, read_error
             FROM keyframe ORDER BY t_ms",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(KeyframeRow {
                t_ms: r.get::<_, i64>(0)? as u64,
                file: r.get(1)?,
                width: r.get(2)?,
                height: r.get(3)?,
                reason: r.get(4)?,
                change: r.get(5)?,
                repeat_of: r.get::<_, Option<i64>>(6)?.map(|t| t as u64),
                read_status: r.get(7)?,
                reading: r.get(8)?,
                model: r.get(9)?,
                read_error: r.get(10)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
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
    fn keyframes_round_trip_and_v1_files_are_upgraded() {
        let dir = temp_dir("keyframes");
        let path = dir.join("timeline.sqlite");
        {
            // A timeline written by M2 (schema v1).
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 CREATE TABLE transcript (id INTEGER PRIMARY KEY, speaker TEXT NOT NULL,
                     start_ms INTEGER NOT NULL, end_ms INTEGER NOT NULL, text TEXT NOT NULL,
                     model TEXT NOT NULL, created_at INTEGER NOT NULL);
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        }
        let t = Timeline::open(&path).unwrap();
        let mut b = KeyframeRow::new(9000, "keyframes/000009000.png", 1920, 1080, "settled", 0.2);
        let a = KeyframeRow::new(1000, "keyframes/000001000.png", 1920, 1080, "first", 1.0);
        t.add_keyframe(&b).unwrap();
        t.add_keyframe(&a).unwrap();
        t.set_keyframe_read(9000, "read", Some("{}"), Some("m"), None)
            .unwrap();
        b.read_status = "read".into();
        b.reading = Some("{}".into());
        b.model = Some("m".into());
        assert_eq!(t.keyframes().unwrap(), vec![a, b]);
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
