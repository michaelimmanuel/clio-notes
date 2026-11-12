//! The section writer: one section's transcript and screens in, a heading,
//! bullets and term corrections out.

use providers::{TextGenerator, TextRequest, Usage};
use serde::Deserialize;
use serde_json::{Value, json};

use super::correct::Correction;
use super::sectioner::{Screen, SectionDraft};
use super::{Bullet, BulletKind, Source, clock};

/// The writer's reply, before it is checked.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Reply {
    pub heading: String,
    pub bullets: Vec<Bullet>,
    /// Number of the screen that best shows the section, from 1; 0 for none.
    pub image: u32,
    pub corrections: Vec<Correction>,
}

/// The reply schema in strict-mode form.
pub fn schema() -> Value {
    let string = json!({ "type": "string" });
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
    object(json!({
        "heading": string,
        "bullets": array(object(json!({
            "text": string,
            "source": { "type": "string", "enum": ["spoken", "screen", "both"] },
            "kind": { "type": "string", "enum": ["point", "decision", "action"] },
        }))),
        "image": { "type": "integer" },
        "corrections": array(object(json!({ "heard": string, "should_be": string }))),
    }))
}

const SYSTEM: &str = "You write live meeting notes, one section at a time. You are given \
the transcript of one stretch of a meeting and readings of what was on screen during it. \
The reader was not in the meeting.

Rules:
- Use only what is in the material. Never add facts, names, numbers or conclusions that \
are not there. If something is unclear, leave it out.
- \"Me\" is the person taking the notes; \"Others\" are the other participants, who cannot \
be told apart. Do not invent names for them. Write \"it was said\" style facts directly, \
without \"Me said\".
- Each bullet is one concrete point: a fact, number, explanation, question, decision or \
next step. Keep bullets short. Skip greetings, filler and talk about the call itself.
- `source` says where a bullet comes from: \"spoken\" if it was said, \"screen\" if it was \
only shown, \"both\" if it was said and shown. Never present something that was only on \
screen as if it had been said.
- `kind` is \"decision\" when the participants agreed on something, \"action\" when someone \
took on a task (name who, if said), otherwise \"point\".
- `heading`: 3 to 8 words naming the topic. No numbering.
- `image`: the number of the screen that best illustrates the section, or 0 if none is \
relevant to what was discussed.
- `corrections`: the transcript comes from speech recognition and may mishear names and \
technical terms that are spelled on screen. For each clear mishearing give `heard` (the \
exact words in the transcript) and `should_be` (the spelling on screen). Only when you are \
confident it is the same term. Never use this to rephrase. Use the corrected spellings in \
your bullets.
- If the section has nothing worth noting, return an empty `bullets` list.";

/// Longest screen text passed to the writer, in characters.
const MAX_SCREEN_TEXT: usize = 2500;

/// The prompt for one section.
pub fn prompt(draft: &SectionDraft, earlier_headings: &[String]) -> String {
    let mut p = format!(
        "Meeting section from {} to {}.\n",
        clock(draft.start_ms),
        clock(draft.end_ms)
    );
    if !earlier_headings.is_empty() {
        let recent: Vec<_> = earlier_headings.iter().rev().take(5).rev().collect();
        p.push_str("Earlier sections, for context only (do not repeat them): ");
        p.push_str(
            &recent
                .iter()
                .map(|h| format!("\"{h}\""))
                .collect::<Vec<_>>()
                .join("; "),
        );
        p.push('\n');
    }

    p.push_str("\n## Screens\n");
    if draft.screens.is_empty() {
        p.push_str("(no screen was captured)\n");
    }
    for (i, s) in draft.screens.iter().enumerate() {
        p.push_str(&screen_block(i + 1, s));
    }

    p.push_str("\n## Transcript\n");
    if draft.lines.is_empty() {
        p.push_str("(nothing was said)\n");
    }
    for l in &draft.lines {
        p.push_str(&format!(
            "[{}] {}: {}\n",
            clock(l.start_ms),
            l.speaker.label(),
            l.text
        ));
    }
    p
}

fn screen_block(number: usize, s: &Screen) -> String {
    let when = if s.carried {
        "already showing when the section began".to_string()
    } else {
        format!("appeared at {}", clock(s.t_ms))
    };
    let Some(r) = &s.reading else {
        return format!("\nScreen {number} ({when}): not read.\n");
    };
    let mut b = format!("\nScreen {number} ({when}), {}", r.kind);
    if !r.title.is_empty() {
        b.push_str(&format!(": \"{}\"", r.title));
    }
    b.push('\n');
    if !r.summary.is_empty() {
        b.push_str(&format!("Summary: {}\n", r.summary));
    }
    if !r.text.is_empty() {
        let text: String = r.text.chars().take(MAX_SCREEN_TEXT).collect();
        b.push_str(&format!("Text:\n{text}\n"));
    }
    for t in &r.tables {
        b.push_str(&format!("Table: {}\n", t.caption));
        for row in t.rows.iter().take(20) {
            b.push_str(&format!("  {}\n", row.join(" | ")));
        }
    }
    for c in &r.charts {
        b.push_str(&format!("Chart: {}: {}\n", c.title, c.reading));
    }
    for c in &r.code {
        let code: String = c.code.lines().take(30).collect::<Vec<_>>().join("\n");
        b.push_str(&format!("Code ({}):\n{code}\n", c.language));
    }
    if !r.terms.is_empty() {
        b.push_str(&format!("Terms: {}\n", r.terms.join(", ")));
    }
    b
}

#[derive(Debug, Clone)]
pub struct Written {
    pub reply: Reply,
    pub model: String,
    pub usage: Usage,
}

/// Writes one section. A reply that does not match the schema is retried once.
pub async fn write_section(
    generator: &dyn TextGenerator,
    draft: &SectionDraft,
    earlier_headings: &[String],
) -> Result<Written, String> {
    let request = TextRequest::new(prompt(draft, earlier_headings))
        .system(SYSTEM)
        .max_tokens(4000)
        .json_schema("meeting_section", schema());
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
                return Ok(Written {
                    reply,
                    model: response.model,
                    usage,
                });
            }
            Err(e) => last_error = e,
        }
    }
    Err(format!(
        "The text model did not return the expected format: {last_error}"
    ))
}

/// Parses and tidies a reply. Tolerates a Markdown code fence around the JSON.
pub fn parse(text: &str) -> Result<Reply, String> {
    let body = text.trim();
    let body = body
        .strip_prefix("```json")
        .or_else(|| body.strip_prefix("```"))
        .and_then(|b| b.strip_suffix("```"))
        .unwrap_or(body)
        .trim();
    let mut r: Reply = serde_json::from_str(body).map_err(|e| e.to_string())?;
    r.heading = r.heading.trim().trim_matches('#').trim().to_string();
    r.bullets = r
        .bullets
        .into_iter()
        .map(|b| Bullet {
            text: b
                .text
                .trim()
                .trim_start_matches(['-', '*', '•'])
                .trim()
                .to_string(),
            ..b
        })
        .filter(|b| !b.text.is_empty())
        .collect();
    if r.heading.is_empty() && !r.bullets.is_empty() {
        return Err("the heading is empty".into());
    }
    Ok(r)
}

/// With no screens in a section, nothing can have come from the screen.
pub fn fix_sources(bullets: &mut [Bullet], draft: &SectionDraft) {
    let has_screen = draft.screens.iter().any(|s| s.reading.is_some());
    let has_speech = !draft.lines.is_empty();
    for b in bullets {
        if !has_screen {
            b.source = Source::Spoken;
        } else if !has_speech {
            b.source = Source::Screen;
            // Nothing was said, so nothing was decided or taken on.
            b.kind = BulletKind::Point;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::ScreenReading;
    use storage::{Speaker, TranscriptLine};

    fn draft() -> SectionDraft {
        SectionDraft {
            start_ms: 60_000,
            end_ms: 185_000,
            lines: vec![TranscriptLine {
                speaker: Speaker::Others,
                start_ms: 62_000,
                end_ms: 66_000,
                text: "We deploy on cooper netties next week.".into(),
                model: "m".into(),
            }],
            screens: vec![Screen {
                t_ms: 60_000,
                file: "keyframes/000060000.png".into(),
                carried: false,
                reading: Some(ScreenReading {
                    kind: "slides".into(),
                    title: "Rollout plan".into(),
                    text: "Kubernetes cluster ready by Friday".into(),
                    tables: vec![],
                    charts: vec![],
                    code: vec![],
                    terms: vec!["Kubernetes".into()],
                    summary: "A rollout slide.".into(),
                }),
            }],
        }
    }

    #[test]
    fn prompt_holds_the_transcript_and_the_screen() {
        let p = prompt(&draft(), &["Introductions".to_string()]);
        assert!(p.contains("from 01:00 to 03:05"));
        assert!(p.contains("\"Introductions\""));
        assert!(p.contains("Screen 1 (appeared at 01:00), slides: \"Rollout plan\""));
        assert!(p.contains("Terms: Kubernetes"));
        assert!(p.contains("[01:02] Others: We deploy on cooper netties next week."));
    }

    #[test]
    fn reply_is_parsed_and_tidied() {
        let r = parse(
            r#"{"heading":" ## Rollout plan ","bullets":[
                {"text":"- Deploy on Kubernetes next week","source":"spoken","kind":"decision"},
                {"text":"  ","source":"screen","kind":"point"}],
              "image":1,"corrections":[{"heard":"cooper netties","should_be":"Kubernetes"}]}"#,
        )
        .unwrap();
        assert_eq!(r.heading, "Rollout plan");
        assert_eq!(r.bullets.len(), 1);
        assert_eq!(r.bullets[0].text, "Deploy on Kubernetes next week");
        assert_eq!(r.bullets[0].kind, BulletKind::Decision);
        assert_eq!(r.corrections[0].should_be, "Kubernetes");
        assert!(parse(r#"{"heading":"x"}"#).is_err());
    }

    #[test]
    fn sources_cannot_claim_what_was_not_there() {
        let mut bullets = vec![Bullet {
            text: "x".into(),
            source: Source::Screen,
            kind: BulletKind::Point,
        }];
        let mut d = draft();
        d.screens.clear();
        fix_sources(&mut bullets, &d);
        assert_eq!(bullets[0].source, Source::Spoken);

        let mut d = draft();
        d.lines.clear();
        bullets[0].kind = BulletKind::Decision;
        fix_sources(&mut bullets, &d);
        assert_eq!(bullets[0].source, Source::Screen);
        assert_eq!(bullets[0].kind, BulletKind::Point);
    }
}
