//! The library: a full-text index of every meeting, and the questions asked
//! across meetings.
//!
//! It lives in `library.sqlite` next to the meetings folder. The index is a
//! copy of what the meetings hold and can be rebuilt from them at any time;
//! the questions cannot.
//!
//! With search by meaning on, each piece of text also has a vector from the
//! embedding model. Vectors are kept by a fingerprint of the text and the
//! model, so indexing a meeting again reuses the vectors of pieces whose text
//! did not change.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{Result, StorageError};

const SCHEMA_VERSION: i64 = 2;

/// A meeting as the library knows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LibraryMeeting {
    /// The meeting's folder name.
    pub id: String,
    /// Changes whenever the meeting changes; set by whoever indexes it.
    pub stamp: String,
    /// Empty when the meeting has no notes yet.
    pub title: String,
    pub summary: String,
    pub started_at_unix_ms: Option<u64>,
    pub duration_ms: u64,
}

/// One searchable piece of a meeting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Chunk {
    /// notes, topic, section, screen or transcript.
    pub kind: String,
    pub start_ms: u64,
    pub end_ms: u64,
    pub body: String,
}

/// A chunk that matched a search.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Hit {
    pub meeting: String,
    pub kind: String,
    pub start_ms: u64,
    pub end_ms: u64,
    /// Higher is a better match.
    pub score: f64,
    /// The matching part of the text, with matches between `\u{1}` and
    /// `\u{2}`; only from [`Library::search_snippets`].
    pub snippet: Option<String>,
}

/// A piece of a meeting with its vector.
#[derive(Debug, Clone, PartialEq)]
pub struct ChunkVector {
    pub meeting: String,
    pub kind: String,
    pub start_ms: u64,
    pub end_ms: u64,
    pub vector: Vec<f32>,
}

/// How far the vectors of one model have come.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct VectorProgress {
    pub meetings: usize,
    /// Meetings whose every piece has a vector.
    pub meetings_done: usize,
    pub pieces: usize,
    pub pieces_done: usize,
}

/// A fingerprint of `text`: vectors are kept by it.
pub fn text_hash(text: &str) -> String {
    Sha256::digest(text.as_bytes())[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn to_bytes(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn from_bytes(bytes: &[u8]) -> Vec<f32> {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect()
}

/// A meeting named in an answer as `[M3]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MeetingRef {
    /// `M3`.
    pub tag: String,
    pub id: String,
    pub title: String,
    pub started_at_unix_ms: Option<u64>,
}

/// One question asked across meetings, with its answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LibraryChatRow {
    pub id: i64,
    pub question: String,
    pub answer: String,
    pub model: String,
    /// When the question was asked, in Unix milliseconds.
    pub asked_at: u64,
    /// The meetings the answer names.
    pub sources: Vec<MeetingRef>,
}

pub struct Library {
    conn: Connection,
}

impl Library {
    /// Opens or creates the library at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(Duration::from_secs(10))?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(StorageError::Other(format!(
                "{} was written by a newer version of Clio Notes",
                path.display()
            )));
        }
        if version < 1 {
            conn.execute_batch(
                "CREATE TABLE meeting (
                     id          TEXT PRIMARY KEY,
                     stamp       TEXT    NOT NULL,
                     title       TEXT    NOT NULL,
                     summary     TEXT    NOT NULL,
                     started_at  INTEGER,
                     duration_ms INTEGER NOT NULL
                 );
                 CREATE TABLE chat (
                     id       INTEGER PRIMARY KEY,
                     question TEXT    NOT NULL,
                     answer   TEXT    NOT NULL,
                     model    TEXT    NOT NULL,
                     asked_at INTEGER NOT NULL,
                     sources  TEXT    NOT NULL
                 );
                 PRAGMA user_version = 1;",
            )?;
        }
        if version < 2 {
            // Pieces gained a fingerprint, so everything is indexed again.
            conn.execute_batch(
                "DROP TABLE IF EXISTS chunk;
                 DELETE FROM meeting;
                 CREATE VIRTUAL TABLE chunk USING fts5(
                     body,
                     meeting  UNINDEXED,
                     kind     UNINDEXED,
                     start_ms UNINDEXED,
                     end_ms   UNINDEXED,
                     hash     UNINDEXED,
                     tokenize = 'porter unicode61 remove_diacritics 2'
                 );
                 CREATE TABLE vector (
                     hash  TEXT NOT NULL,
                     model TEXT NOT NULL,
                     data  BLOB NOT NULL,
                     PRIMARY KEY (hash, model)
                 ) WITHOUT ROWID;
                 PRAGMA user_version = 2;",
            )?;
        }
        Ok(Self { conn })
    }

    /// The stamp of every indexed meeting, by id.
    pub fn stamps(&self) -> Result<HashMap<String, String>> {
        let mut stmt = self.conn.prepare("SELECT id, stamp FROM meeting")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Every indexed meeting, newest first.
    pub fn meetings(&self) -> Result<Vec<LibraryMeeting>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, stamp, title, summary, started_at, duration_ms FROM meeting
             ORDER BY started_at DESC, id DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(LibraryMeeting {
                id: r.get(0)?,
                stamp: r.get(1)?,
                title: r.get(2)?,
                summary: r.get(3)?,
                started_at_unix_ms: r.get::<_, Option<i64>>(4)?.map(|t| t as u64),
                duration_ms: r.get::<_, i64>(5)? as u64,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Replaces everything indexed for `meeting.id`.
    pub fn replace_meeting(&mut self, meeting: &LibraryMeeting, chunks: &[Chunk]) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM chunk WHERE meeting = ?1", [&meeting.id])?;
        tx.execute(
            "INSERT OR REPLACE INTO meeting (id, stamp, title, summary, started_at, duration_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                meeting.id,
                meeting.stamp,
                meeting.title,
                meeting.summary,
                meeting.started_at_unix_ms.map(|t| t as i64),
                meeting.duration_ms as i64,
            ],
        )?;
        {
            let mut insert = tx.prepare(
                "INSERT INTO chunk (body, meeting, kind, start_ms, end_ms, hash)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for c in chunks {
                insert.execute(params![
                    c.body,
                    meeting.id,
                    c.kind,
                    c.start_ms as i64,
                    c.end_ms as i64,
                    text_hash(&c.body)
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn remove_meeting(&mut self, id: &str) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM chunk WHERE meeting = ?1", [id])?;
        tx.execute("DELETE FROM meeting WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(())
    }

    /// The best `limit` chunks for an FTS5 `query`, best first.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>> {
        self.hits(
            "SELECT meeting, kind, start_ms, end_ms, bm25(chunk), NULL FROM chunk
             WHERE chunk MATCH ?1 ORDER BY bm25(chunk) LIMIT ?2",
            query,
            limit,
        )
    }

    /// [`Library::search`] with the matching part of each chunk.
    pub fn search_snippets(&self, query: &str, limit: usize) -> Result<Vec<Hit>> {
        self.hits(
            "SELECT meeting, kind, start_ms, end_ms, bm25(chunk),
                    snippet(chunk, 0, char(1), char(2), '…', 14)
             FROM chunk WHERE chunk MATCH ?1 ORDER BY bm25(chunk) LIMIT ?2",
            query,
            limit,
        )
    }

    fn hits(&self, sql: &str, query: &str, limit: usize) -> Result<Vec<Hit>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(params![query, limit as i64], |r| {
            Ok(Hit {
                meeting: r.get(0)?,
                kind: r.get(1)?,
                start_ms: r.get::<_, i64>(2)? as u64,
                end_ms: r.get::<_, i64>(3)? as u64,
                // bm25 is lower for better matches.
                score: -r.get::<_, f64>(4)?,
                snippet: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Up to `limit` pieces with no vector from `model`, newest meetings
    /// first, as (fingerprint, text).
    pub fn missing_vectors(&self, model: &str, limit: usize) -> Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT c.hash, MIN(c.body) FROM chunk c JOIN meeting m ON m.id = c.meeting
             WHERE NOT EXISTS (SELECT 1 FROM vector v WHERE v.hash = c.hash AND v.model = ?1)
             GROUP BY c.hash ORDER BY MAX(m.started_at) DESC, MIN(c.start_ms) LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![model, limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Stores vectors from `model` by fingerprint. An empty vector marks a
    /// piece the model could not take, so it is not tried again.
    pub fn add_vectors(&mut self, model: &str, vectors: &[(String, Vec<f32>)]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut insert = tx
                .prepare("INSERT OR REPLACE INTO vector (hash, model, data) VALUES (?1, ?2, ?3)")?;
            for (hash, vector) in vectors {
                insert.execute(params![hash, model, to_bytes(vector)])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Whether `model` has made at least one vector here.
    pub fn has_vectors(&self, model: &str) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM vector WHERE model = ?1 AND length(data) > 0)",
            [model],
            |r| r.get(0),
        )?)
    }

    /// Every piece with a vector from `model`.
    pub fn vectors(&self, model: &str) -> Result<Vec<ChunkVector>> {
        let mut stmt = self.conn.prepare(
            "SELECT c.meeting, c.kind, c.start_ms, c.end_ms, v.data
             FROM chunk c JOIN vector v ON v.hash = c.hash AND v.model = ?1
             WHERE length(v.data) > 0",
        )?;
        let rows = stmt.query_map([model], |r| {
            Ok(ChunkVector {
                meeting: r.get(0)?,
                kind: r.get(1)?,
                start_ms: r.get::<_, i64>(2)? as u64,
                end_ms: r.get::<_, i64>(3)? as u64,
                vector: from_bytes(&r.get::<_, Vec<u8>>(4)?),
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// How many meetings and pieces have a vector from `model`.
    pub fn vector_progress(&self, model: &str) -> Result<VectorProgress> {
        let meetings: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM meeting", [], |r| r.get(0))?;
        let mut stmt = self.conn.prepare(
            "SELECT c.meeting, COUNT(*),
                    SUM(EXISTS (SELECT 1 FROM vector v WHERE v.hash = c.hash AND v.model = ?1))
             FROM chunk c GROUP BY c.meeting",
        )?;
        let mut progress = VectorProgress {
            meetings: meetings as usize,
            meetings_done: meetings as usize,
            ..VectorProgress::default()
        };
        let rows = stmt.query_map([model], |r| Ok((r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)))?;
        for row in rows {
            let (pieces, done) = row?;
            progress.pieces += pieces as usize;
            progress.pieces_done += done as usize;
            if done < pieces {
                progress.meetings_done = progress.meetings_done.saturating_sub(1);
            }
        }
        Ok(progress)
    }

    /// Deletes vectors from other models and of text no meeting has any
    /// more. Returns how many were deleted.
    pub fn prune_vectors(&self, model: &str) -> Result<usize> {
        Ok(self.conn.execute(
            "DELETE FROM vector WHERE model <> ?1 OR hash NOT IN (SELECT hash FROM chunk)",
            [model],
        )?)
    }

    /// Stores an answered question and returns it with its id.
    pub fn add_chat(
        &self,
        question: &str,
        answer: &str,
        model: &str,
        asked_at: u64,
        sources: &[MeetingRef],
    ) -> Result<LibraryChatRow> {
        let json =
            serde_json::to_string(sources).map_err(|e| StorageError::Other(e.to_string()))?;
        self.conn.execute(
            "INSERT INTO chat (question, answer, model, asked_at, sources)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![question, answer, model, asked_at as i64, json],
        )?;
        Ok(LibraryChatRow {
            id: self.conn.last_insert_rowid(),
            question: question.into(),
            answer: answer.into(),
            model: model.into(),
            asked_at,
            sources: sources.to_vec(),
        })
    }

    /// Every question asked across meetings, oldest first.
    pub fn chat(&self) -> Result<Vec<LibraryChatRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, question, answer, model, asked_at, sources FROM chat ORDER BY id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(LibraryChatRow {
                id: r.get(0)?,
                question: r.get(1)?,
                answer: r.get(2)?,
                model: r.get(3)?,
                asked_at: r.get::<_, i64>(4)? as u64,
                sources: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or_default(),
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn clear_chat(&self) -> Result<()> {
        self.conn.execute("DELETE FROM chat", [])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(test: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "clio-library-{test}-{}-{}",
            std::process::id(),
            crate::unix_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("library.sqlite")
    }

    fn meeting(id: &str, started: u64) -> LibraryMeeting {
        LibraryMeeting {
            id: id.into(),
            stamp: "s1".into(),
            title: format!("Title {id}"),
            summary: String::new(),
            started_at_unix_ms: Some(started),
            duration_ms: 60_000,
        }
    }

    fn chunk(kind: &str, start_ms: u64, body: &str) -> Chunk {
        Chunk {
            kind: kind.into(),
            start_ms,
            end_ms: start_ms + 1000,
            body: body.into(),
        }
    }

    #[test]
    fn index_replaces_searches_and_removes() {
        let path = temp_path("index");
        let mut lib = Library::open(&path).unwrap();
        lib.replace_meeting(
            &meeting("meeting-1", 1),
            &[
                chunk(
                    "transcript",
                    5000,
                    "We talked about pricing for the launch.",
                ),
                chunk("screen", 9000, "Roadmap slide"),
            ],
        )
        .unwrap();
        lib.replace_meeting(
            &meeting("meeting-2", 2),
            &[chunk(
                "transcript",
                1000,
                "The price goes up in March. Harga naik.",
            )],
        )
        .unwrap();

        // Porter stemming: "prices" finds "pricing" and "price".
        let hits = lib.search("\"prices\"", 10).unwrap();
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert!(hits.iter().all(|h| h.score > 0.0));
        let snippets = lib.search_snippets("\"roadmap\"", 10).unwrap();
        assert_eq!(snippets[0].meeting, "meeting-1");
        assert_eq!(snippets[0].start_ms, 9000);
        assert_eq!(
            snippets[0].snippet.as_deref(),
            Some("\u{1}Roadmap\u{2} slide")
        );

        // Re-indexing a meeting drops its old chunks.
        let mut m1 = meeting("meeting-1", 1);
        m1.stamp = "s2".into();
        lib.replace_meeting(&m1, &[chunk("notes", 0, "Budget review")])
            .unwrap();
        assert!(lib.search("\"roadmap\"", 10).unwrap().is_empty());
        assert_eq!(lib.stamps().unwrap()["meeting-1"], "s2");
        assert_eq!(
            lib.meetings()
                .unwrap()
                .iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>(),
            ["meeting-2", "meeting-1"]
        );

        lib.remove_meeting("meeting-2").unwrap();
        assert!(lib.search("\"price\"", 10).unwrap().is_empty());
        assert_eq!(lib.meetings().unwrap().len(), 1);
        drop(lib);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn vectors_are_kept_by_text_and_model() {
        let path = temp_path("vectors");
        let mut lib = Library::open(&path).unwrap();
        lib.replace_meeting(
            &meeting("meeting-1", 1),
            &[
                chunk("notes", 0, "Budget"),
                chunk("transcript", 5000, "Launch in March"),
            ],
        )
        .unwrap();
        lib.replace_meeting(
            &meeting("meeting-2", 2),
            &[chunk("transcript", 0, "Hiring")],
        )
        .unwrap();
        assert_eq!(
            lib.vector_progress("e").unwrap(),
            VectorProgress {
                meetings: 2,
                meetings_done: 0,
                pieces: 3,
                pieces_done: 0
            }
        );

        // Newest meeting first.
        let missing = lib.missing_vectors("e", 10).unwrap();
        assert_eq!(missing[0], (text_hash("Hiring"), "Hiring".to_string()));
        assert_eq!(missing.len(), 3);
        let vectors: Vec<(String, Vec<f32>)> = missing
            .iter()
            .map(|(hash, body)| {
                (
                    hash.clone(),
                    if body == "Budget" {
                        vec![]
                    } else {
                        vec![0.5, -1.0]
                    },
                )
            })
            .collect();
        lib.add_vectors("e", &vectors).unwrap();
        assert!(lib.missing_vectors("e", 10).unwrap().is_empty());
        // The piece the model could not take is not searched.
        let found = lib.vectors("e").unwrap();
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|v| v.vector == [0.5, -1.0]));
        assert_eq!(lib.vector_progress("e").unwrap().meetings_done, 2);
        assert_eq!(lib.missing_vectors("other", 10).unwrap().len(), 3);

        // Indexing again keeps the vectors of unchanged text.
        let mut m1 = meeting("meeting-1", 1);
        m1.stamp = "s2".into();
        lib.replace_meeting(
            &m1,
            &[
                chunk("notes", 0, "Budget"),
                chunk("transcript", 5000, "Launch in April"),
            ],
        )
        .unwrap();
        let missing = lib.missing_vectors("e", 10).unwrap();
        assert_eq!(
            missing,
            [(text_hash("Launch in April"), "Launch in April".to_string())]
        );

        // Pruning drops other models and text that is gone.
        lib.add_vectors("other", &[(text_hash("Hiring"), vec![1.0])])
            .unwrap();
        assert_eq!(lib.prune_vectors("e").unwrap(), 2);
        drop(lib);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn version_1_libraries_are_upgraded() {
        let path = temp_path("upgrade");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE meeting (id TEXT PRIMARY KEY, stamp TEXT NOT NULL, title TEXT NOT NULL,
                     summary TEXT NOT NULL, started_at INTEGER, duration_ms INTEGER NOT NULL);
                 CREATE VIRTUAL TABLE chunk USING fts5(body, meeting UNINDEXED, kind UNINDEXED,
                     start_ms UNINDEXED, end_ms UNINDEXED);
                 CREATE TABLE chat (id INTEGER PRIMARY KEY, question TEXT NOT NULL,
                     answer TEXT NOT NULL, model TEXT NOT NULL, asked_at INTEGER NOT NULL,
                     sources TEXT NOT NULL);
                 INSERT INTO meeting VALUES ('meeting-1', 's', 't', '', 1, 1);
                 INSERT INTO chat VALUES (1, 'q', 'a', 'm', 1, '[]');
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        }
        let lib = Library::open(&path).unwrap();
        assert!(
            lib.stamps().unwrap().is_empty(),
            "meetings are indexed again"
        );
        assert_eq!(lib.chat().unwrap().len(), 1, "questions are kept");
        drop(lib);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn chat_keeps_sources() {
        let path = temp_path("chat");
        let lib = Library::open(&path).unwrap();
        let source = MeetingRef {
            tag: "M1".into(),
            id: "meeting-1".into(),
            title: "Plan".into(),
            started_at_unix_ms: Some(5),
        };
        let row = lib
            .add_chat("What changed?", "The date [M1 01:00].", "m", 7, &[source])
            .unwrap();
        assert_eq!(lib.chat().unwrap(), vec![row]);
        lib.clear_chat().unwrap();
        assert!(lib.chat().unwrap().is_empty());
        drop(lib);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
