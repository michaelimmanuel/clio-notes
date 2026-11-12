//! Live notes: the transcript and the screens, fused into sections.
//!
//! ```text
//! transcript lines ─┐
//!                   ├─► sectioner ─► term corrections ─► section writer ─► notes
//! keyframes ────────┘
//! ```
//!
//! Sections are written one at a time, in order, each shortly after it
//! closes. Every bullet records whether it was said, shown on screen, or
//! both, so the two are never merged silently.

pub mod correct;
pub mod sectioner;
pub mod write;

use std::collections::VecDeque;
use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use providers::{TextGenerator, Usage};
use serde::{Deserialize, Serialize};
use storage::{Speaker, TranscriptLine};
use tokio::sync::mpsc;

use crate::screen::ScreenEvent;
use crate::transcribe::PipelineEvent;

pub use sectioner::{NotesInput, Screen, SectionConfig, SectionDraft, Sectioner};

/// Where a bullet comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// Said aloud.
    Spoken,
    /// Only shown on screen.
    Screen,
    /// Said and shown.
    Both,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BulletKind {
    Point,
    /// Something the participants agreed on.
    Decision,
    /// A task someone took on.
    Action,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bullet {
    pub text: String,
    pub source: Source,
    pub kind: BulletKind,
}

/// The keyframe shown with a section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SectionImage {
    pub t_ms: u64,
    /// Relative to the meeting folder.
    pub file: String,
}

/// One written section of the notes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Section {
    /// Position in the notes, from 0.
    pub index: u32,
    pub start_ms: u64,
    pub end_ms: u64,
    pub heading: String,
    pub bullets: Vec<Bullet>,
    pub image: Option<SectionImage>,
    pub model: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NotesEvent {
    /// A section closed and is being written.
    Writing {
        index: u32,
        start_ms: u64,
        end_ms: u64,
    },
    Section {
        section: Section,
        usage: Usage,
        latency_ms: u64,
    },
    /// The section had nothing worth noting.
    Empty { index: u32 },
    Failed {
        index: u32,
        start_ms: u64,
        end_ms: u64,
        message: String,
    },
    /// A transcript line with a term's spelling fixed from the screen.
    Corrected {
        speaker: Speaker,
        start_ms: u64,
        text: String,
    },
}

impl NotesInput {
    pub fn from_speech(event: &PipelineEvent) -> Self {
        match event {
            PipelineEvent::Detected {
                speaker, start_ms, ..
            } => Self::Utterance {
                speaker: speaker.clone(),
                start_ms: *start_ms,
            },
            PipelineEvent::Transcribed { line, .. } => Self::Line(line.clone()),
            PipelineEvent::Empty {
                speaker, start_ms, ..
            }
            | PipelineEvent::Failed {
                speaker, start_ms, ..
            } => Self::NoLine {
                speaker: speaker.clone(),
                start_ms: *start_ms,
            },
        }
    }

    pub fn from_screen(event: &ScreenEvent) -> Self {
        match event {
            ScreenEvent::Keyframe {
                t_ms,
                file,
                reason,
                change,
                repeat_of,
                ..
            } => Self::Keyframe {
                t_ms: *t_ms,
                file: file.clone(),
                reason: *reason,
                change: *change,
                repeat_of: *repeat_of,
            },
            ScreenEvent::Read { t_ms, reading, .. } => Self::Reading {
                t_ms: *t_ms,
                reading: reading.clone(),
            },
            ScreenEvent::ReadSkipped { t_ms, .. } | ScreenEvent::ReadFailed { t_ms, .. } => {
                Self::NoReading { t_ms: *t_ms }
            }
        }
    }
}

/// Session time as mm:ss, or h:mm:ss from one hour.
pub fn clock(ms: u64) -> String {
    let s = ms / 1000;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s % 3600 / 60, s % 60)
    } else {
        format!("{:02}:{:02}", s / 60, s % 60)
    }
}

/// What writing one section produced.
struct Outcome {
    index: u32,
    start_ms: u64,
    end_ms: u64,
    corrected: Vec<TranscriptLine>,
    result: Result<Option<Section>, String>,
    usage: Usage,
    latency_ms: u64,
}

/// Runs until `input` closes and the last section has been written.
pub async fn notes_stream(
    cfg: SectionConfig,
    mut input: mpsc::UnboundedReceiver<NotesInput>,
    writer: Arc<dyn TextGenerator>,
    events: mpsc::UnboundedSender<NotesEvent>,
) {
    let mut sectioner = Sectioner::new(cfg);
    let mut queue: VecDeque<SectionDraft> = VecDeque::new();
    let mut running: Option<tokio::task::JoinHandle<Outcome>> = None;
    let mut headings: Vec<String> = Vec::new();
    // Every term seen on screen so far: a slide's terms stay useful after it.
    let mut glossary: Vec<String> = Vec::new();
    let mut next_index = 0u32;
    let mut input_open = true;

    loop {
        if running.is_none()
            && let Some(draft) = queue.pop_front()
        {
            let index = next_index;
            next_index += 1;
            for term in draft
                .screens
                .iter()
                .filter_map(|s| s.reading.as_ref())
                .flat_map(|r| &r.terms)
            {
                if !glossary.contains(term) {
                    glossary.push(term.clone());
                }
            }
            let _ = events.send(NotesEvent::Writing {
                index,
                start_ms: draft.start_ms,
                end_ms: draft.end_ms,
            });
            running = Some(tokio::spawn(write_one(
                draft,
                index,
                headings.clone(),
                glossary.clone(),
                writer.clone(),
            )));
        }
        if !input_open && running.is_none() {
            break;
        }

        tokio::select! {
            message = input.recv(), if input_open => match message {
                Some(message) => queue.extend(sectioner.push(message)),
                None => {
                    input_open = false;
                    queue.extend(sectioner.finish());
                }
            },
            outcome = async { running.as_mut().unwrap().await }, if running.is_some() => {
                running = None;
                let Ok(outcome) = outcome else { continue };
                for line in outcome.corrected {
                    let _ = events.send(NotesEvent::Corrected {
                        speaker: line.speaker,
                        start_ms: line.start_ms,
                        text: line.text,
                    });
                }
                let _ = events.send(match outcome.result {
                    Ok(Some(section)) => {
                        headings.push(section.heading.clone());
                        NotesEvent::Section {
                            section,
                            usage: outcome.usage,
                            latency_ms: outcome.latency_ms,
                        }
                    }
                    Ok(None) => NotesEvent::Empty { index: outcome.index },
                    Err(message) => NotesEvent::Failed {
                        index: outcome.index,
                        start_ms: outcome.start_ms,
                        end_ms: outcome.end_ms,
                        message,
                    },
                });
            }
        }
    }
}

async fn write_one(
    mut draft: SectionDraft,
    index: u32,
    headings: Vec<String>,
    glossary: Vec<String>,
    writer: Arc<dyn TextGenerator>,
) -> Outcome {
    let started = Instant::now();
    let original: Vec<String> = draft.lines.iter().map(|l| l.text.clone()).collect();

    // Free fixes first, so the writer sees the right spellings.
    for line in &mut draft.lines {
        if let Some(fixed) = correct::apply_terms(&line.text, &glossary) {
            line.text = fixed;
        }
    }

    let written = write::write_section(writer.as_ref(), &draft, &headings).await;
    let (result, usage) = match written {
        Err(message) => (Err(message), Usage::default()),
        Ok(w) => {
            // What is on screen, for checking the writer's corrections.
            let mut on_screen = glossary.join("\n").to_lowercase();
            for r in draft.screens.iter().filter_map(|s| s.reading.as_ref()) {
                on_screen.push('\n');
                on_screen.push_str(&r.title.to_lowercase());
                on_screen.push('\n');
                on_screen.push_str(&r.text.to_lowercase());
            }
            for c in w
                .reply
                .corrections
                .iter()
                .filter(|c| correct::is_allowed(c, &on_screen))
            {
                for line in &mut draft.lines {
                    if let Some(fixed) = correct::replace_phrase(&line.text, &c.heard, &c.should_be)
                    {
                        line.text = fixed;
                    }
                }
            }

            let mut bullets = w.reply.bullets;
            write::fix_sources(&mut bullets, &draft);
            let section = (!bullets.is_empty()).then(|| Section {
                index,
                start_ms: draft.start_ms,
                end_ms: draft.end_ms,
                heading: w.reply.heading,
                bullets,
                image: pick_image(&draft.screens, w.reply.image),
                model: w.model,
            });
            (Ok(section), w.usage)
        }
    };

    let corrected = draft
        .lines
        .iter()
        .zip(&original)
        .filter(|(line, before)| &line.text != *before)
        .map(|(line, _)| line.clone())
        .collect();
    Outcome {
        index,
        start_ms: draft.start_ms,
        end_ms: draft.end_ms,
        corrected,
        result,
        usage,
        latency_ms: started.elapsed().as_millis() as u64,
    }
}

/// The screen the writer chose (numbered from 1). 0 means none; a number
/// that does not exist falls back to the first screen new in this section.
fn pick_image(screens: &[Screen], choice: u32) -> Option<SectionImage> {
    let screen = match choice {
        0 => return None,
        n => screens
            .get(n as usize - 1)
            .or_else(|| screens.iter().find(|s| !s.carried))
            .or(screens.first())?,
    };
    Some(SectionImage {
        t_ms: screen.t_ms,
        file: screen.file.clone(),
    })
}

/// Stores a notes event in the meeting's timeline.
pub fn record(timeline: &storage::Timeline, event: &NotesEvent) -> storage::Result<()> {
    match event {
        NotesEvent::Section { section, .. } => timeline.add_section(&storage::SectionRow {
            index: section.index,
            start_ms: section.start_ms,
            end_ms: section.end_ms,
            heading: section.heading.clone(),
            bullets: serde_json::to_string(&section.bullets).unwrap_or_default(),
            image_t_ms: section.image.as_ref().map(|i| i.t_ms),
            model: section.model.clone(),
        }),
        NotesEvent::Corrected {
            speaker,
            start_ms,
            text,
        } => timeline.set_corrected(speaker, *start_ms, text),
        NotesEvent::Writing { .. } | NotesEvent::Empty { .. } | NotesEvent::Failed { .. } => Ok(()),
    }
}

/// One section as Markdown. Bullets that were only on screen say so.
pub fn section_markdown(section: &Section) -> String {
    let mut md = format!("## {} – {}\n\n", clock(section.start_ms), section.heading);
    if let Some(image) = &section.image {
        md.push_str(&format!(
            "![Screen at {}]({})\n\n",
            clock(image.t_ms),
            image.file
        ));
    }
    for b in &section.bullets {
        let label = match b.kind {
            BulletKind::Point => "",
            BulletKind::Decision => "**Decision:** ",
            BulletKind::Action => "**Action:** ",
        };
        let source = match b.source {
            Source::Spoken => "",
            Source::Screen => " *(on screen)*",
            Source::Both => " *(said and shown)*",
        };
        md.push_str(&format!("- {label}{}{source}\n", b.text));
    }
    md.push('\n');
    md
}

/// Adds a section to the live notes file, creating it with a title first.
pub fn append_markdown(path: &Path, section: &Section) -> std::io::Result<()> {
    let new = !path.exists();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    if new {
        file.write_all(b"# Meeting notes (live)\n\n")?;
    }
    file.write_all(section_markdown(section).as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::{KeyframeReason, ScreenReading};
    use async_trait::async_trait;
    use providers::{ProviderError, TextRequest, TextResponse};
    use std::sync::Mutex;

    /// Replies with a fixed section and records the prompts it was sent.
    struct FakeWriter {
        prompts: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl TextGenerator for FakeWriter {
        async fn generate(&self, request: &TextRequest) -> Result<TextResponse, ProviderError> {
            let n = {
                let mut prompts = self.prompts.lock().unwrap();
                prompts.push(request.prompt.clone());
                prompts.len()
            };
            Ok(TextResponse {
                text: format!(
                    r#"{{"heading":"Topic {n}","bullets":[
                        {{"text":"Deploy on Kubernetes","source":"both","kind":"decision"}}],
                      "image":1,"corrections":[
                        {{"heard":"cooper netties","should_be":"Kubernetes"}},
                        {{"heard":"deploy","should_be":"Destroy"}}]}}"#
                ),
                model: "fake".into(),
                usage: Usage {
                    input_tokens: 100,
                    output_tokens: 20,
                },
            })
        }
    }

    fn line(start_s: u64, text: &str) -> NotesInput {
        NotesInput::Line(TranscriptLine {
            speaker: Speaker::Others,
            start_ms: start_s * 1000,
            end_ms: start_s * 1000 + 3000,
            text: text.into(),
            model: "m".into(),
        })
    }

    fn slide(t_s: u64, title: &str, terms: &[&str]) -> [NotesInput; 2] {
        [
            NotesInput::Keyframe {
                t_ms: t_s * 1000,
                file: format!("keyframes/{t_s}.png"),
                reason: KeyframeReason::Settled,
                change: 0.9,
                repeat_of: None,
            },
            NotesInput::Reading {
                t_ms: t_s * 1000,
                reading: ScreenReading {
                    kind: "slides".into(),
                    title: title.into(),
                    text: String::new(),
                    tables: vec![],
                    charts: vec![],
                    code: vec![],
                    terms: terms.iter().map(|t| t.to_string()).collect(),
                    summary: String::new(),
                },
            },
        ]
    }

    #[tokio::test]
    async fn sections_are_written_in_order_with_checked_corrections() {
        let writer = Arc::new(FakeWriter {
            prompts: Mutex::new(Vec::new()),
        });
        let (tx, rx) = mpsc::unbounded_channel();
        let (etx, mut erx) = mpsc::unbounded_channel();
        let task = tokio::spawn(notes_stream(
            SectionConfig::default(),
            rx,
            writer.clone(),
            etx,
        ));
        for m in slide(0, "Plan", &["Kubernetes", "PowerShell"]) {
            tx.send(m).unwrap();
        }
        tx.send(line(
            5,
            "We will deploy on cooper netties from power shell, all agreed.",
        ))
        .unwrap();
        for m in slide(40, "Budget", &[]) {
            tx.send(m).unwrap();
        }
        tx.send(line(
            45,
            "The budget is fine as it stands for this quarter, no changes.",
        ))
        .unwrap();
        drop(tx);
        task.await.unwrap();

        let mut events = Vec::new();
        while let Ok(e) = erx.try_recv() {
            events.push(e);
        }
        let sections: Vec<&Section> = events
            .iter()
            .filter_map(|e| match e {
                NotesEvent::Section { section, .. } => Some(section),
                _ => None,
            })
            .collect();
        assert_eq!(sections.len(), 2, "{events:?}");
        assert_eq!((sections[0].index, sections[0].end_ms), (0, 40_000));
        assert_eq!(sections[0].image.as_ref().unwrap().file, "keyframes/0.png");
        assert_eq!(sections[1].image.as_ref().unwrap().file, "keyframes/40.png");

        // The local fix and the on-screen correction are applied; the one
        // whose replacement is not on screen ("Destroy") is refused.
        let corrected: Vec<&String> = events
            .iter()
            .filter_map(|e| match e {
                NotesEvent::Corrected { text, .. } => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(
            corrected,
            vec!["We will deploy on Kubernetes from PowerShell, all agreed."]
        );

        let prompts = writer.prompts.lock().unwrap();
        // The writer saw the locally fixed spelling, and the second section
        // was told about the first.
        assert!(prompts[0].contains("from PowerShell"));
        assert!(prompts[1].contains("\"Topic 1\""));
    }

    #[test]
    fn markdown_marks_decisions_and_screen_only_bullets() {
        let section = Section {
            index: 0,
            start_ms: 65_000,
            end_ms: 120_000,
            heading: "Rollout plan".into(),
            bullets: vec![
                Bullet {
                    text: "Ship on Friday".into(),
                    source: Source::Spoken,
                    kind: BulletKind::Decision,
                },
                Bullet {
                    text: "Revenue up 12%".into(),
                    source: Source::Screen,
                    kind: BulletKind::Point,
                },
            ],
            image: Some(SectionImage {
                t_ms: 65_000,
                file: "keyframes/000065000.png".into(),
            }),
            model: "m".into(),
        };
        assert_eq!(
            section_markdown(&section),
            "## 01:05 – Rollout plan\n\n![Screen at 01:05](keyframes/000065000.png)\n\n\
             - **Decision:** Ship on Friday\n- Revenue up 12% *(on screen)*\n\n"
        );
    }
}
