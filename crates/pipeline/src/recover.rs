//! Finishing a meeting that never stopped (the app closed or crashed while
//! it ran).
//!
//! Everything a meeting produces is saved as it arrives: transcript lines,
//! keyframes and sections go into the timeline (SQLite, write-ahead log),
//! the audio files are flushed every few seconds. What is missing after a
//! crash is the end: the stop time, the WAV headers' final lengths, and the
//! notes for the part of the meeting after the last written section.
//!
//! [`mark_interrupted`] fixes the first two and needs no provider; the app
//! runs it at startup. [`write_missing_sections`] writes the missing notes
//! from the saved transcript and screens, before the final pass.

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;

use providers::{TextGenerator, Usage};
use storage::Timeline;
use tokio::sync::mpsc;

use crate::notes::{self, NotesEvent, NotesInput, Resume, SectionConfig};
use crate::screen::{KeyframeReason, ScreenReading};

/// Meta key set on a meeting that was finished by recovery.
pub const INTERRUPTED: &str = "interrupted";
/// Meta key set once the notes after the last live section are written.
const TAIL_WRITTEN: &str = "tail_written";

/// Whether the meeting in `dir` never stopped and has not been marked yet.
pub fn needs_marking(dir: &Path) -> bool {
    let Ok(timeline) = Timeline::open(&dir.join("timeline.sqlite")) else {
        return false;
    };
    timeline.meta("stopped_at_unix_ms").ok().flatten().is_none()
}

/// Gives a meeting that never stopped a stop time and playable audio files.
/// Returns how long the meeting ran, from its last saved event.
pub fn mark_interrupted(dir: &Path) -> Result<u64, String> {
    for name in ["mic.wav", "system.wav"] {
        let path = dir.join(name);
        if path.is_file() {
            repair_wav(&path).map_err(|e| format!("{name}: {e}"))?;
        }
    }
    let input = crate::final_pass::load(dir)?;
    let timeline = Timeline::open(&dir.join("timeline.sqlite")).map_err(|e| e.to_string())?;
    let started = input.started_at_unix_ms.unwrap_or(0);
    let set = |k: &str, v: &str| timeline.set_meta(k, v).map_err(|e| e.to_string());
    set(INTERRUPTED, "1")?;
    set(
        "stopped_at_unix_ms",
        &(started + input.duration_ms).to_string(),
    )?;
    Ok(input.duration_ms)
}

/// Whether the meeting was recovered after an interruption.
pub fn was_interrupted(timeline: &Timeline) -> bool {
    timeline.meta(INTERRUPTED).ok().flatten().is_some()
}

/// Writes notes sections for the part of an interrupted meeting after its
/// last live section, from the saved transcript and screens. Does nothing
/// for a meeting that stopped normally or whose tail is already written.
/// Returns the number of sections written and the tokens used.
pub async fn write_missing_sections(
    dir: &Path,
    writer: Arc<dyn TextGenerator>,
) -> Result<(usize, Usage), String> {
    let (resume, inputs) = {
        let timeline = Timeline::open(&dir.join("timeline.sqlite")).map_err(|e| e.to_string())?;
        if !was_interrupted(&timeline) || timeline.meta(TAIL_WRITTEN).ok().flatten().is_some() {
            return Ok((0, Usage::default()));
        }
        let input = crate::final_pass::load(dir)?;
        let start_ms = input.sections.last().map(|s| s.end_ms).unwrap_or(0);
        let resume = Resume {
            start_ms,
            headings: input.sections.iter().map(|s| s.heading.clone()).collect(),
        };
        (resume, tail_inputs(&input, start_ms))
    };

    let (in_tx, in_rx) = mpsc::unbounded_channel();
    for i in inputs {
        let _ = in_tx.send(i);
    }
    drop(in_tx);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let stream = tokio::spawn(notes::notes_stream_from(
        SectionConfig::default(),
        resume,
        in_rx,
        writer,
        tx,
    ));

    let timeline = Timeline::open(&dir.join("timeline.sqlite")).map_err(|e| e.to_string())?;
    let live = dir.join("notes-live.md");
    let mut written = 0;
    let mut usage = Usage::default();
    let mut failure = None;
    while let Some(event) = rx.recv().await {
        notes::record(&timeline, &event).map_err(|e| e.to_string())?;
        match &event {
            NotesEvent::Section {
                section, usage: u, ..
            } => {
                written += 1;
                usage.input_tokens += u.input_tokens;
                usage.output_tokens += u.output_tokens;
                notes::append_markdown(&live, section).map_err(|e| e.to_string())?;
            }
            NotesEvent::Failed { message, .. } => failure = Some(message.clone()),
            _ => {}
        }
    }
    let _ = stream.await;
    match failure {
        // Left unmarked, so the next attempt tries the failed part again.
        Some(message) if written == 0 => Err(message),
        _ => {
            timeline
                .set_meta(TAIL_WRITTEN, "1")
                .map_err(|e| e.to_string())?;
            Ok((written, usage))
        }
    }
}

/// The saved lines and screens from `start_ms` on, as the notes stream
/// would have received them live.
fn tail_inputs(input: &crate::final_pass::MeetingInput, start_ms: u64) -> Vec<NotesInput> {
    let mut timed: Vec<(u64, Vec<NotesInput>)> = Vec::new();
    for line in input.transcript.iter().filter(|l| l.start_ms >= start_ms) {
        timed.push((
            line.start_ms,
            vec![
                NotesInput::Utterance {
                    speaker: line.speaker.clone(),
                    start_ms: line.start_ms,
                },
                NotesInput::Line(line.clone()),
            ],
        ));
    }
    // The screen showing when the tail starts is carried into its first
    // section, so earlier keyframes are passed too.
    for k in &input.keyframes {
        let reason = match k.reason.as_str() {
            "first" => KeyframeReason::First,
            "moving" => KeyframeReason::Moving,
            _ => KeyframeReason::Settled,
        };
        let reading = k
            .reading
            .as_deref()
            .and_then(|r| serde_json::from_str::<ScreenReading>(r).ok());
        let mut items = vec![NotesInput::Keyframe {
            t_ms: k.t_ms,
            file: k.file.clone(),
            reason,
            change: k.change as f32,
            repeat_of: k.repeat_of,
        }];
        items.push(match reading {
            Some(reading) => NotesInput::Reading {
                t_ms: k.t_ms,
                reading,
            },
            None => NotesInput::NoReading { t_ms: k.t_ms },
        });
        timed.push((k.t_ms, items));
    }
    timed.sort_by_key(|(t, _)| *t);
    timed.into_iter().flat_map(|(_, items)| items).collect()
}

/// Fixes the RIFF and data lengths of a WAV file whose writer never
/// finished, so players read all of it. Returns whether anything changed.
pub fn repair_wav(path: &Path) -> std::io::Result<bool> {
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    let len = file.metadata()?.len();
    let mut header = [0u8; 12];
    if len < 44 || file.read_exact(&mut header).is_err() || &header[0..4] != b"RIFF" {
        return Ok(false);
    }
    // Walks the chunks to the data chunk.
    let mut pos = 12u64;
    let data_pos = loop {
        if pos + 8 > len {
            return Ok(false);
        }
        file.seek(SeekFrom::Start(pos))?;
        let mut chunk = [0u8; 8];
        file.read_exact(&mut chunk)?;
        if &chunk[0..4] == b"data" {
            break pos;
        }
        let size = u32::from_le_bytes(chunk[4..8].try_into().unwrap()) as u64;
        pos += 8 + size + (size & 1);
    };
    let riff_size = (len - 8).min(u32::MAX as u64) as u32;
    // Whole 16-bit samples only.
    let data_size = ((len - data_pos - 8) & !1).min(u32::MAX as u64) as u32;

    let mut current = [0u8; 4];
    file.seek(SeekFrom::Start(data_pos + 4))?;
    file.read_exact(&mut current)?;
    let stale_data = u32::from_le_bytes(current) != data_size;
    let stale_riff = u32::from_le_bytes(header[4..8].try_into().unwrap()) != riff_size;
    if !stale_data && !stale_riff {
        return Ok(false);
    }
    file.seek(SeekFrom::Start(4))?;
    file.write_all(&riff_size.to_le_bytes())?;
    file.seek(SeekFrom::Start(data_pos + 4))?;
    file.write_all(&data_size.to_le_bytes())?;
    file.flush()?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notes::{Bullet, BulletKind, Source};
    use async_trait::async_trait;
    use providers::{TextRequest, TextResponse};
    use storage::{SectionRow, Speaker, TranscriptLine};

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "clio-recover-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn an_unfinished_wav_is_repaired() {
        let dir = temp_dir("wav");
        let path = dir.join("mic.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&path, spec).unwrap();
        for i in 0..16_000 {
            w.write_sample((i % 100) as i16).unwrap();
        }
        w.finalize().unwrap();
        // What a crash leaves: data on disk, lengths in the header still 0.
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[4..8].copy_from_slice(&0u32.to_le_bytes());
        let data = bytes.windows(4).position(|w| w == b"data").unwrap();
        bytes[data + 4..data + 8].copy_from_slice(&0u32.to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(hound::WavReader::open(&path).unwrap().len(), 0);

        assert!(repair_wav(&path).unwrap());
        assert_eq!(hound::WavReader::open(&path).unwrap().len(), 16_000);
        assert!(!repair_wav(&path).unwrap(), "a good file is left alone");
        let _ = std::fs::remove_dir_all(&dir);
    }

    struct Writer;

    #[async_trait]
    impl TextGenerator for Writer {
        async fn generate(&self, r: &TextRequest) -> providers::Result<TextResponse> {
            assert!(r.prompt.contains("budget"), "{}", r.prompt);
            Ok(TextResponse {
                text: r#"{"heading":"Budget","bullets":[{"text":"The budget is fixed.","source":"spoken","kind":"decision"}],"image":0,"corrections":[]}"#.into(),
                model: "m".into(),
                usage: providers::Usage {
                    input_tokens: 5,
                    output_tokens: 2,
                },
            })
        }
    }

    #[tokio::test]
    async fn the_rest_of_an_interrupted_meeting_gets_notes() {
        let dir = temp_dir("tail");
        {
            let t = Timeline::open(&dir.join("timeline.sqlite")).unwrap();
            t.set_meta("started_at_unix_ms", "1000000").unwrap();
            let line = |start_ms: u64, text: &str| TranscriptLine {
                speaker: Speaker::Me,
                start_ms,
                end_ms: start_ms + 3000,
                text: text.into(),
                model: "m".into(),
            };
            t.add_transcript(&line(
                1000,
                "We start with the agenda for today and the plan.",
            ))
            .unwrap();
            t.add_section(&SectionRow {
                index: 0,
                start_ms: 0,
                end_ms: 40_000,
                heading: "Agenda".into(),
                bullets: serde_json::to_string(&vec![Bullet {
                    text: "Agenda".into(),
                    source: Source::Spoken,
                    kind: BulletKind::Point,
                }])
                .unwrap(),
                image_t_ms: None,
                model: "m".into(),
            })
            .unwrap();
            t.add_transcript(&line(
                45_000,
                "The budget stays fixed for the next quarter, we all agree on that one.",
            ))
            .unwrap();
        }
        assert!(needs_marking(&dir));
        assert_eq!(mark_interrupted(&dir).unwrap(), 48_000);
        assert!(!needs_marking(&dir));

        let (written, usage) = write_missing_sections(&dir, Arc::new(Writer))
            .await
            .unwrap();
        assert_eq!(written, 1);
        assert_eq!(usage.input_tokens, 5);
        let t = Timeline::open(&dir.join("timeline.sqlite")).unwrap();
        let sections = t.sections().unwrap();
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[1].index, 1);
        assert_eq!(sections[1].start_ms, 40_000);
        assert_eq!(sections[1].heading, "Budget");
        assert!(
            std::fs::read_to_string(dir.join("notes-live.md"))
                .unwrap()
                .contains("Budget")
        );
        // Only once.
        assert_eq!(
            write_missing_sections(&dir, Arc::new(Writer))
                .await
                .unwrap()
                .0,
            0
        );
        drop(t);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
