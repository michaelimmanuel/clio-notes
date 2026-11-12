//! The end-of-meeting pass: the live notes in, one clean document out.
//!
//! ```text
//! timeline (live sections, keyframes, transcript) ─► final pass ─► notes.md
//!                                                                  transcript.md
//!                                                                  images/
//! ```
//!
//! The model merges live sections about the same topic, drops repetition,
//! picks one image per topic and writes a summary, decisions and action
//! items. Without a text model, or when the call fails, `notes.md` is still
//! written from the live sections, so every meeting ends with a document.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use providers::{TextGenerator, TextRequest, Usage};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use storage::{KeyframeRow, Timeline, TranscriptLine};

use crate::notes::{Bullet, BulletKind, Source, clock};
use crate::screen::ScreenReading;

/// One point of the final notes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub text: String,
    pub source: Source,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionItem {
    pub text: String,
    /// Who took it on, as said in the meeting; empty when not said.
    pub owner: String,
    pub source: Source,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TopicImage {
    pub t_ms: u64,
    /// The keyframe it was copied from, relative to the meeting folder.
    pub keyframe: String,
    /// The exported copy, relative to the meeting folder (`images/…`).
    pub file: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Topic {
    pub heading: String,
    pub start_ms: u64,
    pub end_ms: u64,
    pub bullets: Vec<Item>,
    pub image: Option<TopicImage>,
}

/// The final notes of a meeting, as stored in the timeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FinalNotes {
    pub title: String,
    pub summary: String,
    pub decisions: Vec<Item>,
    pub actions: Vec<ActionItem>,
    pub topics: Vec<Topic>,
    /// The model that wrote them; `None` when they are the live notes as is.
    pub model: Option<String>,
    /// Why the final pass did not run, when it did not.
    pub note: Option<String>,
}

/// What the final pass reads from a meeting.
#[derive(Debug, Clone)]
pub struct MeetingInput {
    pub started_at_unix_ms: Option<u64>,
    pub duration_ms: u64,
    pub sections: Vec<LiveSection>,
    pub keyframes: Vec<KeyframeRow>,
    pub transcript: Vec<TranscriptLine>,
}

#[derive(Debug, Clone)]
pub struct LiveSection {
    pub start_ms: u64,
    pub end_ms: u64,
    pub heading: String,
    pub bullets: Vec<Bullet>,
    pub image_t_ms: Option<u64>,
}

/// Reads everything the final pass needs. Opens and closes the timeline, so
/// nothing is held across the model call.
pub fn load(dir: &Path) -> Result<MeetingInput, String> {
    let timeline = Timeline::open(&dir.join("timeline.sqlite")).map_err(|e| e.to_string())?;
    let meta = |key: &str| {
        timeline
            .meta(key)
            .ok()
            .flatten()
            .and_then(|v| v.parse::<u64>().ok())
    };
    let started = meta("started_at_unix_ms");
    let stopped = meta("stopped_at_unix_ms");
    let sections: Vec<LiveSection> = timeline
        .sections()
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|s| LiveSection {
            start_ms: s.start_ms,
            end_ms: s.end_ms,
            heading: s.heading,
            bullets: serde_json::from_str(&s.bullets).unwrap_or_default(),
            image_t_ms: s.image_t_ms,
        })
        .collect();
    let keyframes = timeline.keyframes().map_err(|e| e.to_string())?;
    let transcript = timeline.transcript().map_err(|e| e.to_string())?;
    let last_event = [
        sections.last().map(|s| s.end_ms),
        transcript.iter().map(|l| l.end_ms).max(),
        keyframes.last().map(|k| k.t_ms),
    ]
    .into_iter()
    .flatten()
    .max()
    .unwrap_or(0);
    let duration_ms = match (started, stopped) {
        (Some(a), Some(b)) if b > a => b - a,
        _ => last_event,
    };
    Ok(MeetingInput {
        started_at_unix_ms: started,
        duration_ms,
        sections,
        keyframes,
        transcript,
    })
}

/// A screen the model can choose as a topic's image.
struct Candidate<'a> {
    keyframe: &'a KeyframeRow,
    reading: Option<ScreenReading>,
}

fn candidates(input: &MeetingInput) -> Vec<Candidate<'_>> {
    input
        .keyframes
        .iter()
        .filter(|k| k.repeat_of.is_none())
        .map(|k| Candidate {
            keyframe: k,
            reading: k
                .reading
                .as_deref()
                .and_then(|r| serde_json::from_str(r).ok()),
        })
        .collect()
}

/// The reply schema in strict-mode form.
pub fn schema() -> Value {
    let string = json!({ "type": "string" });
    let source = json!({ "type": "string", "enum": ["spoken", "screen", "both"] });
    let object = |props: Value| {
        let required: Vec<&String> = props.as_object().unwrap().keys().collect();
        json!({
            "type": "object",
            "properties": props,
            "required": required,
            "additionalProperties": false,
        })
    };
    let array = |items: Value| json!({ "type": "array", "items": items });
    let item = object(json!({ "text": string, "source": source }));
    object(json!({
        "title": string,
        "summary": string,
        "decisions": array(item.clone()),
        "actions": array(object(json!({ "text": string, "owner": string, "source": source }))),
        "topics": array(object(json!({
            "heading": string,
            "sections": array(json!({ "type": "integer" })),
            "bullets": array(item),
            "image": { "type": "integer" },
        }))),
    }))
}

#[derive(Debug, Deserialize)]
struct Reply {
    title: String,
    summary: String,
    decisions: Vec<Item>,
    actions: Vec<ActionItem>,
    topics: Vec<ReplyTopic>,
}

#[derive(Debug, Deserialize)]
struct ReplyTopic {
    heading: String,
    sections: Vec<u32>,
    bullets: Vec<Item>,
    image: u32,
}

const SYSTEM: &str = "You turn live meeting notes into the final notes of the meeting. The \
live notes were written section by section while the meeting ran, so they repeat \
themselves, split topics, and sometimes record points that were revised later.

Rules:
- Use only what is in the live notes and the screen list. Never add facts, names, \
numbers or conclusions.
- Merge sections about the same topic into one topic, even if they are far apart. Keep \
topics in the order they first came up.
- Remove repetition, filler and points that were later revised or contradicted (keep the \
final version).
- Keep every source mark: \"spoken\" if it was said, \"screen\" if it was only shown, \
\"both\" if it was said and shown. When merging points, \"spoken\" and \"screen\" together \
become \"both\". Never present something that was only on screen as said.
- `title`: a short title for the meeting, 3 to 8 words, from its main subject.
- `summary`: 2 to 4 sentences on what the meeting was about and its outcome.
- `decisions`: everything the participants agreed on. `actions`: tasks someone took on, \
with `owner` set to who took it on if that was said (\"Me\" is the note taker; \"Others\" \
cannot be told apart), otherwise \"\". A decision or action also stays in its topic only if \
it adds context there.
- `topics`: for each, `heading` (3 to 8 words), `sections` (the numbers of the live \
sections it covers), `bullets` (short, concrete, merged) and `image` (the number of the \
screen that best shows the topic, preferring the most complete version of a slide; 0 if \
none fits).
- The final notes must be shorter and clearer than the live notes.";

/// The prompt: every live section, then the screens that can be shown.
pub fn prompt(input: &MeetingInput) -> String {
    let mut p = format!(
        "Meeting length: {}.\n\n## Live notes\n",
        clock(input.duration_ms)
    );
    for (i, s) in input.sections.iter().enumerate() {
        p.push_str(&format!(
            "\nSection {} [{}–{}] \"{}\"\n",
            i + 1,
            clock(s.start_ms),
            clock(s.end_ms),
            s.heading
        ));
        for b in &s.bullets {
            let kind = match b.kind {
                BulletKind::Point => "point",
                BulletKind::Decision => "decision",
                BulletKind::Action => "action",
            };
            let source = match b.source {
                Source::Spoken => "spoken",
                Source::Screen => "screen",
                Source::Both => "both",
            };
            p.push_str(&format!("- ({kind}, {source}) {}\n", b.text));
        }
    }
    let screens = candidates(input);
    p.push_str("\n## Screens\n");
    if screens.is_empty() {
        p.push_str("(none)\n");
    }
    for (i, c) in screens.iter().enumerate() {
        let what = match &c.reading {
            Some(r) if !r.title.is_empty() => format!("{}: \"{}\". {}", r.kind, r.title, r.summary),
            Some(r) => format!("{}. {}", r.kind, r.summary),
            None => "not read".to_string(),
        };
        p.push_str(&format!(
            "Screen {} [{}] {what}\n",
            i + 1,
            clock(c.keyframe.t_ms)
        ));
    }
    p
}

/// Runs the final pass and exports the result. Falls back to the live notes
/// when there is no generator or the call fails; the reason is kept in
/// [`FinalNotes::note`].
pub async fn finalize(
    dir: &Path,
    generator: Option<&dyn TextGenerator>,
) -> Result<(FinalNotes, Usage), String> {
    let input = load(dir)?;
    let (notes, usage) = match generator {
        None => (
            from_live(
                &input,
                Some("No profile is assigned to text generation, so these are the live notes."),
            ),
            Usage::default(),
        ),
        Some(_) if input.sections.is_empty() => (from_live(&input, None), Usage::default()),
        Some(g) => match generate(g, &input).await {
            Ok(done) => done,
            Err(e) => (
                from_live(
                    &input,
                    Some(&format!(
                        "The final pass failed, so these are the live notes: {e}"
                    )),
                ),
                Usage::default(),
            ),
        },
    };
    let notes = export(dir, &input, notes)?;
    Ok((notes, usage))
}

async fn generate(
    generator: &dyn TextGenerator,
    input: &MeetingInput,
) -> Result<(FinalNotes, Usage), String> {
    let request = TextRequest::new(prompt(input))
        .system(SYSTEM)
        .max_tokens(12_000)
        .json_schema("final_notes", schema());
    let mut usage = Usage::default();
    let mut last_error = String::new();
    for _ in 0..2 {
        let response = generator
            .generate(&request)
            .await
            .map_err(|e| e.to_string())?;
        usage.input_tokens += response.usage.input_tokens;
        usage.output_tokens += response.usage.output_tokens;
        match parse(&response.text) {
            Ok(reply) => {
                let mut notes = build(input, reply);
                notes.model = Some(response.model);
                return Ok((notes, usage));
            }
            Err(e) => last_error = e,
        }
    }
    Err(format!(
        "the text model did not return the expected format: {last_error}"
    ))
}

fn parse(text: &str) -> Result<Reply, String> {
    let body = text.trim();
    let body = body
        .strip_prefix("```json")
        .or_else(|| body.strip_prefix("```"))
        .and_then(|b| b.strip_suffix("```"))
        .unwrap_or(body)
        .trim();
    let reply: Reply = serde_json::from_str(body).map_err(|e| e.to_string())?;
    if reply.topics.is_empty() {
        return Err("no topics".into());
    }
    Ok(reply)
}

fn tidy(text: &str) -> String {
    text.trim()
        .trim_start_matches(['-', '*', '•'])
        .trim()
        .to_string()
}

fn tidy_items(items: Vec<Item>) -> Vec<Item> {
    items
        .into_iter()
        .map(|i| Item {
            text: tidy(&i.text),
            ..i
        })
        .filter(|i| !i.text.is_empty())
        .collect()
}

/// Checks the reply against the meeting: section numbers and image numbers
/// must exist, and a meeting with no screens cannot have screen points.
fn build(input: &MeetingInput, reply: Reply) -> FinalNotes {
    let screens = candidates(input);
    let any_screen = !screens.is_empty();
    let fix = |mut items: Vec<Item>| {
        if !any_screen {
            for i in &mut items {
                i.source = Source::Spoken;
            }
        }
        tidy_items(items)
    };

    let mut topics: Vec<Topic> = Vec::new();
    for t in reply.topics {
        let covered: Vec<&LiveSection> = t
            .sections
            .iter()
            .filter_map(|n| input.sections.get((*n as usize).checked_sub(1)?))
            .collect();
        let bullets = fix(t.bullets);
        let heading = t.heading.trim().trim_matches('#').trim().to_string();
        if bullets.is_empty() || heading.is_empty() {
            continue;
        }
        let (start_ms, end_ms) = match (
            covered.iter().map(|s| s.start_ms).min(),
            covered.iter().map(|s| s.end_ms).max(),
        ) {
            (Some(a), Some(b)) => (a, b),
            // No valid section numbers: place it after the previous topic.
            _ => {
                let at = topics.last().map(|p| p.end_ms).unwrap_or(0);
                (at, at)
            }
        };
        let image = (t.image as usize)
            .checked_sub(1)
            .and_then(|i| screens.get(i))
            .map(|c| TopicImage {
                t_ms: c.keyframe.t_ms,
                keyframe: c.keyframe.file.clone(),
                file: String::new(),
            });
        topics.push(Topic {
            heading,
            start_ms,
            end_ms,
            bullets,
            image,
        });
    }
    topics.sort_by_key(|t| t.start_ms);

    FinalNotes {
        title: reply.title.trim().to_string(),
        summary: reply.summary.trim().to_string(),
        decisions: fix(reply.decisions),
        actions: reply
            .actions
            .into_iter()
            .map(|a| ActionItem {
                text: tidy(&a.text),
                owner: a.owner.trim().to_string(),
                source: if any_screen { a.source } else { Source::Spoken },
            })
            .filter(|a| !a.text.is_empty())
            .collect(),
        topics,
        model: None,
        note: None,
    }
}

/// The live sections as final notes, unchanged.
pub fn from_live(input: &MeetingInput, note: Option<&str>) -> FinalNotes {
    let keyframe = |t: u64| input.keyframes.iter().find(|k| k.t_ms == t);
    let item = |b: &Bullet| Item {
        text: b.text.clone(),
        source: b.source,
    };
    let all = || input.sections.iter().flat_map(|s| &s.bullets);
    FinalNotes {
        title: input
            .sections
            .first()
            .map(|s| s.heading.clone())
            .unwrap_or_else(|| "Meeting".into()),
        summary: String::new(),
        decisions: all()
            .filter(|b| b.kind == BulletKind::Decision)
            .map(item)
            .collect(),
        actions: all()
            .filter(|b| b.kind == BulletKind::Action)
            .map(|b| ActionItem {
                text: b.text.clone(),
                owner: String::new(),
                source: b.source,
            })
            .collect(),
        topics: input
            .sections
            .iter()
            .map(|s| Topic {
                heading: s.heading.clone(),
                start_ms: s.start_ms,
                end_ms: s.end_ms,
                bullets: s.bullets.iter().map(item).collect(),
                image: s.image_t_ms.and_then(keyframe).map(|k| TopicImage {
                    t_ms: k.t_ms,
                    keyframe: k.file.clone(),
                    file: String::new(),
                }),
            })
            .collect(),
        model: None,
        note: note.map(str::to_string),
    }
}

/// Writes `notes.md`, `transcript.md` and `images/` into the meeting folder,
/// and stores the notes in the timeline. Returns the notes with image paths
/// filled in.
pub fn export(
    dir: &Path,
    input: &MeetingInput,
    mut notes: FinalNotes,
) -> Result<FinalNotes, String> {
    let images = dir.join("images");
    if images.exists() {
        std::fs::remove_dir_all(&images).map_err(|e| format!("cannot clear images/: {e}"))?;
    }
    let mut copied: HashMap<String, String> = HashMap::new();
    for (i, topic) in notes.topics.iter_mut().enumerate() {
        let Some(image) = topic.image.as_mut() else {
            continue;
        };
        if let Some(file) = copied.get(&image.keyframe) {
            image.file = file.clone();
            continue;
        }
        let source = dir.join(&image.keyframe);
        if !source.exists() {
            topic.image = None;
            continue;
        }
        std::fs::create_dir_all(&images).map_err(|e| format!("cannot create images/: {e}"))?;
        let file = format!("images/{:02}-{}.png", i + 1, slug(&topic.heading));
        std::fs::copy(&source, dir.join(&file))
            .map_err(|e| format!("cannot copy {}: {e}", image.keyframe))?;
        copied.insert(image.keyframe.clone(), file.clone());
        image.file = file;
    }

    write(dir.join("notes.md"), &markdown(&notes, input))?;
    write(dir.join("transcript.md"), &transcript_markdown(input))?;

    let timeline = Timeline::open(&dir.join("timeline.sqlite")).map_err(|e| e.to_string())?;
    let json = serde_json::to_string(&notes).map_err(|e| e.to_string())?;
    timeline
        .set_meta("final_notes", &json)
        .and_then(|_| timeline.set_meta("final_at_unix_ms", &now_unix_ms().to_string()))
        .map_err(|e| e.to_string())?;
    Ok(notes)
}

/// The stored final notes of a meeting, if the final pass has run.
pub fn stored(timeline: &Timeline) -> Option<FinalNotes> {
    let json = timeline.meta("final_notes").ok().flatten()?;
    serde_json::from_str(&json).ok()
}

fn write(path: PathBuf, text: &str) -> Result<(), String> {
    std::fs::write(&path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

fn slug(heading: &str) -> String {
    let mut s = String::new();
    for c in heading.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            s.push(c);
        } else if !s.ends_with('-') && !s.is_empty() {
            s.push('-');
        }
        if s.len() >= 40 {
            break;
        }
    }
    let s = s.trim_end_matches('-').to_string();
    if s.is_empty() { "screen".into() } else { s }
}

fn source_mark(source: Source) -> &'static str {
    match source {
        Source::Spoken => "",
        Source::Screen => " *(on screen)*",
        Source::Both => " *(said and shown)*",
    }
}

/// The local date and time a meeting started, for headers.
pub fn local_time(unix_ms: u64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_millis_opt(unix_ms as i64) {
        chrono::LocalResult::Single(t) => t.format("%A %-d %B %Y, %H:%M").to_string(),
        _ => String::new(),
    }
}

/// The final notes as Markdown, with image links relative to the folder.
pub fn markdown(notes: &FinalNotes, input: &MeetingInput) -> String {
    let mut md = format!("# {}\n\n", notes.title);
    let mut line = Vec::new();
    if let Some(start) = input.started_at_unix_ms {
        line.push(local_time(start));
    }
    line.push(format!("{} long", duration(input.duration_ms)));
    md.push_str(&format!("*{}*\n\n", line.join(" · ")));
    if let Some(note) = &notes.note {
        md.push_str(&format!("> {note}\n\n"));
    }
    if !notes.summary.is_empty() {
        md.push_str(&format!("{}\n\n", notes.summary));
    }
    if !notes.decisions.is_empty() {
        md.push_str("## Decisions\n\n");
        for d in &notes.decisions {
            md.push_str(&format!("- {}{}\n", d.text, source_mark(d.source)));
        }
        md.push('\n');
    }
    if !notes.actions.is_empty() {
        md.push_str("## Action items\n\n");
        for a in &notes.actions {
            let owner = if a.owner.is_empty() {
                String::new()
            } else {
                format!(" — {}", a.owner)
            };
            md.push_str(&format!(
                "- [ ] {}{owner}{}\n",
                a.text,
                source_mark(a.source)
            ));
        }
        md.push('\n');
    }
    for t in &notes.topics {
        md.push_str(&format!(
            "## {}\n\n*{}–{}*\n\n",
            t.heading,
            clock(t.start_ms),
            clock(t.end_ms)
        ));
        if let Some(image) = t.image.as_ref().filter(|i| !i.file.is_empty()) {
            md.push_str(&format!(
                "![Screen at {}]({})\n\n",
                clock(image.t_ms),
                image.file
            ));
        }
        for b in &t.bullets {
            md.push_str(&format!("- {}{}\n", b.text, source_mark(b.source)));
        }
        md.push('\n');
    }
    md.push_str(
        "---\n\n*Written by Clio Notes. Points marked \"on screen\" were shown but not said. \
         Full [transcript](transcript.md) · [live notes](notes-live.md).*\n",
    );
    md
}

/// The transcript with screen changes marked, as Markdown.
pub fn transcript_markdown(input: &MeetingInput) -> String {
    enum Entry<'a> {
        Line(&'a TranscriptLine),
        Screen(&'a KeyframeRow),
    }
    let mut entries: Vec<(u64, Entry)> = input
        .transcript
        .iter()
        .map(|l| (l.start_ms, Entry::Line(l)))
        .chain(
            input
                .keyframes
                .iter()
                .filter(|k| k.repeat_of.is_none())
                .map(|k| (k.t_ms, Entry::Screen(k))),
        )
        .collect();
    entries.sort_by_key(|(t, _)| *t);

    let mut md = String::from("# Transcript\n\n");
    for (t, entry) in entries {
        match entry {
            Entry::Line(l) => md.push_str(&format!(
                "**[{}] {}:** {}\n\n",
                clock(t),
                l.speaker.label(),
                l.text
            )),
            Entry::Screen(k) => {
                let title = k
                    .reading
                    .as_deref()
                    .and_then(|r| serde_json::from_str::<ScreenReading>(r).ok())
                    .map(|r| {
                        if r.title.is_empty() {
                            r.summary
                        } else {
                            r.title
                        }
                    })
                    .unwrap_or_default();
                md.push_str(&format!("*[{}] Screen changed: {title}*\n\n", clock(t)));
            }
        }
    }
    md
}

fn duration(ms: u64) -> String {
    let minutes = (ms + 30_000) / 60_000;
    match minutes {
        0 => format!("{} s", ms / 1000),
        m if m < 60 => format!("{m} min"),
        m => format!("{} h {} min", m / 60, m % 60),
    }
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use providers::{ProviderError, TextResponse};
    use storage::{KeyframeRow, SectionRow, Speaker};

    fn bullet(text: &str, source: Source, kind: BulletKind) -> Bullet {
        Bullet {
            text: text.into(),
            source,
            kind,
        }
    }

    /// A meeting folder with two live sections about one topic and a slide.
    fn meeting() -> PathBuf {
        static RUN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "clio-final-{}-{}",
            std::process::id(),
            RUN.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(dir.join("keyframes")).unwrap();
        std::fs::write(dir.join("keyframes/000001000.png"), b"png").unwrap();
        let t = Timeline::open(&dir.join("timeline.sqlite")).unwrap();
        t.set_meta("started_at_unix_ms", "1794412833741").unwrap();
        t.set_meta("stopped_at_unix_ms", "1794413133741").unwrap();
        let mut k = KeyframeRow::new(1000, "keyframes/000001000.png", 1920, 1080, "first", 1.0);
        k.read_status = "read".into();
        k.reading = Some(
            r#"{"kind":"slides","title":"Q3 budget","text":"","tables":[],"charts":[],
                "code":[],"terms":[],"summary":"Budget slide"}"#
                .into(),
        );
        t.add_keyframe(&k).unwrap();
        for (i, (start, heading, bullets)) in [
            (
                0,
                "Budget",
                vec![bullet("Budget is 40k", Source::Screen, BulletKind::Point)],
            ),
            (
                120_000,
                "Budget continued",
                vec![bullet(
                    "Keep the budget",
                    Source::Spoken,
                    BulletKind::Decision,
                )],
            ),
        ]
        .into_iter()
        .enumerate()
        {
            t.add_section(&SectionRow {
                index: i as u32,
                start_ms: start,
                end_ms: start + 120_000,
                heading: heading.into(),
                bullets: serde_json::to_string(&bullets).unwrap(),
                image_t_ms: Some(1000),
                model: "m".into(),
            })
            .unwrap();
        }
        t.add_transcript(&TranscriptLine {
            speaker: Speaker::Others,
            start_ms: 2000,
            end_ms: 4000,
            text: "Let's keep the budget.".into(),
            model: "m".into(),
        })
        .unwrap();
        dir
    }

    struct FakeWriter(String);

    #[async_trait]
    impl TextGenerator for FakeWriter {
        async fn generate(&self, _: &TextRequest) -> Result<TextResponse, ProviderError> {
            Ok(TextResponse {
                text: self.0.clone(),
                model: "fake".into(),
                usage: Usage {
                    input_tokens: 500,
                    output_tokens: 100,
                },
            })
        }
    }

    #[tokio::test]
    async fn final_pass_merges_and_exports() {
        let dir = meeting();
        let writer = FakeWriter(
            r#"{"title":"Q3 budget review","summary":"The team reviewed the budget.",
               "decisions":[{"text":"Keep the 40k budget","source":"both"}],
               "actions":[{"text":"Send the figures","owner":"Me","source":"spoken"}],
               "topics":[{"heading":"Budget","sections":[1,2,9],
                          "bullets":[{"text":"- The budget is 40k","source":"both"}],
                          "image":1},
                         {"heading":"Invented","sections":[],"bullets":[],"image":5}]}"#
                .into(),
        );
        let (notes, usage) = finalize(&dir, Some(&writer)).await.unwrap();
        assert_eq!(usage.input_tokens, 500);
        assert_eq!(notes.model.as_deref(), Some("fake"));
        // One topic: the empty one is dropped, section 9 does not exist.
        assert_eq!(notes.topics.len(), 1);
        let topic = &notes.topics[0];
        assert_eq!((topic.start_ms, topic.end_ms), (0, 240_000));
        assert_eq!(topic.bullets[0].text, "The budget is 40k");
        assert_eq!(topic.image.as_ref().unwrap().file, "images/01-budget.png");
        assert!(dir.join("images/01-budget.png").exists());

        let md = std::fs::read_to_string(dir.join("notes.md")).unwrap();
        assert!(md.starts_with("# Q3 budget review\n"));
        assert!(md.contains("5 min long"));
        assert!(md.contains("- Keep the 40k budget *(said and shown)*"));
        assert!(md.contains("- [ ] Send the figures — Me"));
        assert!(md.contains("![Screen at 00:01](images/01-budget.png)"));
        let transcript = std::fs::read_to_string(dir.join("transcript.md")).unwrap();
        assert!(transcript.contains("*[00:01] Screen changed: Q3 budget*"));
        assert!(transcript.contains("**[00:02] Others:** Let's keep the budget."));

        // Stored, so the app can show it again.
        let t = Timeline::open(&dir.join("timeline.sqlite")).unwrap();
        assert_eq!(stored(&t).unwrap(), notes);
        drop(t);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn without_a_model_the_live_notes_are_exported() {
        let dir = meeting();
        let (notes, _) = finalize(&dir, None).await.unwrap();
        assert_eq!(notes.topics.len(), 2);
        assert_eq!(notes.decisions[0].text, "Keep the budget");
        assert!(notes.note.as_deref().unwrap().contains("No profile"));
        // The same slide is copied once and used by both topics.
        assert_eq!(
            notes.topics[1].image.as_ref().unwrap().file,
            "images/01-budget.png"
        );
        let md = std::fs::read_to_string(dir.join("notes.md")).unwrap();
        assert!(md.contains("> No profile is assigned"));

        // A broken reply falls back the same way.
        let broken = FakeWriter("not json".into());
        let (notes, _) = finalize(&dir, Some(&broken)).await.unwrap();
        assert!(notes.note.as_deref().unwrap().contains("final pass failed"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn prompt_lists_sections_and_screens() {
        let dir = meeting();
        let p = prompt(&load(&dir).unwrap());
        assert!(p.contains("Meeting length: 05:00."));
        assert!(p.contains("Section 2 [02:00–04:00] \"Budget continued\""));
        assert!(p.contains("- (decision, spoken) Keep the budget"));
        assert!(p.contains("Screen 1 [00:01] slides: \"Q3 budget\". Budget slide"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn slugs_are_file_safe() {
        assert_eq!(
            slug("Q3 Budget: Review & Next steps!"),
            "q3-budget-review-next-steps"
        );
        assert_eq!(slug("¿?"), "screen");
    }
}
