//! Splits the meeting into sections for the notes.
//!
//! A section ends when the screen changes meaningfully (a new slide, a
//! switch to another window) or after a time limit. Lines and keyframes are
//! assigned to sections by their own time, not by when they arrive, so slow
//! transcription or screen reading cannot put them in the wrong section.
//!
//! A closed section is handed out once its material is complete: every
//! utterance that started in it has come back from speech to text, and its
//! keyframes have been read (or a wait limit has passed).

use std::collections::VecDeque;

use storage::{Speaker, TranscriptLine};

use crate::screen::{KeyframeReason, ScreenReading};

#[derive(Debug, Clone)]
pub struct SectionConfig {
    /// A screen change does not end a section younger than this.
    pub min_ms: u64,
    /// A section ends after this long even if the screen never changes.
    pub max_ms: u64,
    /// Share of the screen (0–1) that must change to end a section.
    pub break_change: f32,
    /// How long after a section's end to wait for utterances that started
    /// in it. Longer than the longest utterance plus its closing pause.
    pub grace_ms: u64,
    /// How long after a section's end to wait for its screen readings.
    pub read_wait_ms: u64,
    /// A section with fewer spoken words and no new screen is folded into
    /// the next one.
    pub min_words: usize,
}

impl Default for SectionConfig {
    fn default() -> Self {
        Self {
            min_ms: 30_000,
            max_ms: 180_000,
            break_change: 0.3,
            grace_ms: 15_000,
            read_wait_ms: 60_000,
            min_words: 12,
        }
    }
}

/// What the notes are built from, as it arrives from the audio and screen
/// pipelines.
#[derive(Debug, Clone)]
pub enum NotesInput {
    /// An utterance was cut and sent to speech to text.
    Utterance {
        speaker: Speaker,
        start_ms: u64,
    },
    /// Its text came back.
    Line(TranscriptLine),
    /// It had no words, or failed.
    NoLine {
        speaker: Speaker,
        start_ms: u64,
    },
    Keyframe {
        t_ms: u64,
        file: String,
        reason: KeyframeReason,
        change: f32,
        repeat_of: Option<u64>,
    },
    Reading {
        t_ms: u64,
        reading: ScreenReading,
    },
    /// The keyframe will not be read (skipped or failed).
    NoReading {
        t_ms: u64,
    },
    /// The session clock, so sections are written during silence too.
    Tick {
        now_ms: u64,
    },
}

/// A screen shown during a section.
#[derive(Debug, Clone, PartialEq)]
pub struct Screen {
    /// When it appeared. Before the section's start if `carried`.
    pub t_ms: u64,
    pub file: String,
    /// Already on screen when the section began.
    pub carried: bool,
    pub reading: Option<ScreenReading>,
}

/// Everything known about one section, ready to be written up.
#[derive(Debug, Clone, PartialEq)]
pub struct SectionDraft {
    pub start_ms: u64,
    pub end_ms: u64,
    pub lines: Vec<TranscriptLine>,
    pub screens: Vec<Screen>,
}

struct Keyframe {
    t_ms: u64,
    file: String,
    repeat_of: Option<u64>,
    reading: Option<ScreenReading>,
    resolved: bool,
}

pub struct Sectioner {
    cfg: SectionConfig,
    /// Start of the first section not yet handed out.
    cursor: u64,
    /// Ends of closed sections not yet handed out.
    bounds: VecDeque<u64>,
    /// Start of the section still open.
    open_start: u64,
    lines: Vec<TranscriptLine>,
    waiting: Vec<(Speaker, u64)>,
    keyframes: Vec<Keyframe>,
    /// The session clock, from ticks only.
    now: u64,
    /// The latest time seen in any input.
    latest: u64,
}

impl Sectioner {
    pub fn new(cfg: SectionConfig) -> Self {
        Self {
            cfg,
            cursor: 0,
            bounds: VecDeque::new(),
            open_start: 0,
            lines: Vec::new(),
            waiting: Vec::new(),
            keyframes: Vec::new(),
            now: 0,
            latest: 0,
        }
    }

    /// Takes one input and returns the sections that are now complete.
    pub fn push(&mut self, input: NotesInput) -> Vec<SectionDraft> {
        match input {
            NotesInput::Utterance { speaker, start_ms } => {
                self.latest = self.latest.max(start_ms);
                self.waiting.push((speaker, start_ms));
            }
            NotesInput::Line(line) => {
                self.resolve(&line.speaker, line.start_ms);
                self.latest = self.latest.max(line.end_ms);
                self.lines.push(line);
            }
            NotesInput::NoLine { speaker, start_ms } => self.resolve(&speaker, start_ms),
            NotesInput::Keyframe {
                t_ms,
                file,
                reason,
                change,
                repeat_of,
            } => {
                self.latest = self.latest.max(t_ms);
                // A meaningful change of screen ends the open section.
                let meaningful = repeat_of.is_none()
                    && reason != KeyframeReason::Moving
                    && change >= self.cfg.break_change;
                if meaningful && t_ms.saturating_sub(self.open_start) >= self.cfg.min_ms {
                    self.bounds.push_back(t_ms);
                    self.open_start = t_ms;
                }
                self.keyframes.push(Keyframe {
                    t_ms,
                    file,
                    repeat_of,
                    reading: None,
                    // A repeat shows a screen that has its own reading.
                    resolved: repeat_of.is_some(),
                });
            }
            NotesInput::Reading { t_ms, reading } => {
                if let Some(k) = self.keyframes.iter_mut().find(|k| k.t_ms == t_ms) {
                    k.reading = Some(reading);
                    k.resolved = true;
                }
            }
            NotesInput::NoReading { t_ms } => {
                if let Some(k) = self.keyframes.iter_mut().find(|k| k.t_ms == t_ms) {
                    k.resolved = true;
                }
            }
            NotesInput::Tick { now_ms } => {
                self.now = self.now.max(now_ms);
                self.latest = self.latest.max(now_ms);
            }
        }
        // The time limit.
        while self.latest >= self.open_start + self.cfg.max_ms {
            self.open_start += self.cfg.max_ms;
            self.bounds.push_back(self.open_start);
        }
        self.take_ready()
    }

    /// The meeting has ended: returns everything that is left.
    pub fn finish(&mut self) -> Vec<SectionDraft> {
        let mut out = Vec::new();
        while let Some(end) = self.bounds.pop_front() {
            if let Some(draft) = self.draft(end, false) {
                out.push(draft);
            }
        }
        let end = self.latest.max(self.cursor);
        if let Some(draft) = self.draft(end.saturating_add(1), true) {
            out.push(SectionDraft {
                end_ms: end,
                ..draft
            });
        }
        out
    }

    fn resolve(&mut self, speaker: &Speaker, start_ms: u64) {
        self.waiting
            .retain(|(s, t)| !(s == speaker && *t == start_ms));
    }

    fn take_ready(&mut self) -> Vec<SectionDraft> {
        let mut out = Vec::new();
        while let Some(&end) = self.bounds.front() {
            let speech_in =
                self.now >= end + self.cfg.grace_ms && !self.waiting.iter().any(|(_, t)| *t < end);
            let screens_in = self.now >= end + self.cfg.read_wait_ms
                || self
                    .keyframes
                    .iter()
                    .filter(|k| k.t_ms >= self.cursor && k.t_ms < end)
                    .all(|k| k.resolved);
            if !(speech_in && screens_in) {
                break;
            }
            self.bounds.pop_front();
            if let Some(draft) = self.draft(end, false) {
                out.push(draft);
            }
        }
        out
    }

    /// Builds the section from the cursor to `end`. Returns `None`, leaving
    /// the cursor in place, when there is too little in it: its material
    /// then becomes part of the following section.
    fn draft(&mut self, end: u64, last: bool) -> Option<SectionDraft> {
        let start = self.cursor;
        let in_range = |t: u64| t >= start && t < end;
        let words: usize = self
            .lines
            .iter()
            .filter(|l| in_range(l.start_ms))
            .map(|l| l.text.split_whitespace().count())
            .sum();
        let new_screens = self.keyframes.iter().any(|k| in_range(k.t_ms));
        let enough = if last {
            words > 0 || new_screens
        } else {
            words >= self.cfg.min_words || new_screens
        };
        if !enough {
            return None;
        }

        let mut lines: Vec<TranscriptLine> = Vec::new();
        self.lines.retain(|l| {
            let take = in_range(l.start_ms);
            if take {
                lines.push(l.clone());
            }
            !take
        });
        lines.sort_by_key(|l| l.start_ms);

        let mut screens = Vec::new();
        // The screen already showing, unless a new one opened the section.
        if !self.keyframes.iter().any(|k| k.t_ms == start)
            && let Some(k) = self.keyframes.iter().rev().find(|k| k.t_ms < start)
        {
            screens.push(self.screen(k, true));
        }
        for k in self.keyframes.iter().filter(|k| in_range(k.t_ms)) {
            screens.push(self.screen(k, false));
        }
        self.cursor = end;
        Some(SectionDraft {
            start_ms: start,
            end_ms: end,
            lines,
            screens,
        })
    }

    /// A keyframe as a screen; a repeat takes the image and reading of the
    /// keyframe it returned to.
    fn screen(&self, k: &Keyframe, carried: bool) -> Screen {
        let source = k
            .repeat_of
            .and_then(|of| self.keyframes.iter().find(|o| o.t_ms == of))
            .unwrap_or(k);
        Screen {
            t_ms: k.t_ms,
            file: source.file.clone(),
            carried,
            reading: source.reading.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(start_s: u64, words: usize) -> NotesInput {
        NotesInput::Line(TranscriptLine {
            speaker: Speaker::Me,
            start_ms: start_s * 1000,
            end_ms: start_s * 1000 + 2000,
            text: vec!["word"; words].join(" "),
            model: "m".into(),
        })
    }

    fn keyframe(t_s: u64, change: f32) -> NotesInput {
        NotesInput::Keyframe {
            t_ms: t_s * 1000,
            file: format!("keyframes/{t_s}.png"),
            reason: KeyframeReason::Settled,
            change,
            repeat_of: None,
        }
    }

    fn reading(title: &str) -> ScreenReading {
        ScreenReading {
            kind: "slides".into(),
            title: title.into(),
            text: String::new(),
            tables: vec![],
            charts: vec![],
            code: vec![],
            terms: vec![],
            summary: String::new(),
        }
    }

    fn tick(s: &mut Sectioner, now_s: u64) -> Vec<SectionDraft> {
        s.push(NotesInput::Tick {
            now_ms: now_s * 1000,
        })
    }

    #[test]
    fn a_new_slide_ends_the_section_once_its_material_is_in() {
        let mut s = Sectioner::new(SectionConfig::default());
        assert!(s.push(keyframe(0, 1.0)).is_empty());
        s.push(NotesInput::Reading {
            t_ms: 0,
            reading: reading("Intro"),
        });
        s.push(line(5, 20));
        // An utterance that starts before the slide change, still at the
        // speech-to-text provider when the change is seen.
        s.push(NotesInput::Utterance {
            speaker: Speaker::Me,
            start_ms: 38_000,
        });
        assert!(s.push(keyframe(40, 0.9)).is_empty());
        // Not yet: the grace period has not passed.
        assert!(tick(&mut s, 50).is_empty());
        // Still not: that utterance is not back.
        assert!(tick(&mut s, 56).is_empty());
        assert!(s.push(line(41, 9)).is_empty());
        // Its text arrives, which completes section 1. The reading of
        // slide 2 is not needed for it.
        let done = s.push(line(38, 6));
        assert_eq!(done.len(), 1, "{done:?}");
        s.push(NotesInput::Reading {
            t_ms: 40_000,
            reading: reading("Roadmap"),
        });
        let d = &done[0];
        assert_eq!((d.start_ms, d.end_ms), (0, 40_000));
        assert_eq!(d.lines.len(), 2);
        assert_eq!(d.screens.len(), 1);
        assert_eq!(d.screens[0].reading.as_ref().unwrap().title, "Intro");

        // The rest comes out at the end, with slide 2 as its screen.
        let rest = s.finish();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].start_ms, 40_000);
        assert_eq!(rest[0].lines.len(), 1);
        assert_eq!(
            rest[0].screens[0].reading.as_ref().unwrap().title,
            "Roadmap"
        );
        assert!(!rest[0].screens[0].carried);
    }

    #[test]
    fn small_and_early_screen_changes_do_not_end_a_section() {
        let mut s = Sectioner::new(SectionConfig::default());
        s.push(keyframe(0, 1.0));
        s.push(keyframe(10, 0.9)); // too soon
        s.push(keyframe(50, 0.1)); // one more bullet
        s.push(line(20, 30));
        let all = s.finish();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].screens.len(), 3);
    }

    #[test]
    fn the_time_limit_splits_long_talk_and_carries_the_screen() {
        let mut s = Sectioner::new(SectionConfig::default());
        s.push(keyframe(0, 1.0));
        s.push(NotesInput::NoReading { t_ms: 0 });
        for t in (0..400).step_by(10) {
            s.push(line(t, 15));
        }
        let mut done = tick(&mut s, 400);
        done.extend(s.finish());
        let ranges: Vec<_> = done.iter().map(|d| (d.start_ms, d.end_ms)).collect();
        assert_eq!(ranges[0], (0, 180_000));
        assert_eq!(ranges[1], (180_000, 360_000));
        assert_eq!(done.len(), 3);
        assert_eq!(done[0].lines.len(), 18);
        // The slide from the start is still what is on screen.
        assert!(done[1].screens[0].carried);
        assert_eq!(done[1].screens[0].t_ms, 0);
    }

    #[test]
    fn quiet_stretches_fold_into_the_next_section() {
        let mut s = Sectioner::new(SectionConfig::default());
        s.push(line(10, 3)); // a few words, then a long silence
        assert!(tick(&mut s, 500).is_empty());
        s.push(line(510, 40));
        let all = s.finish();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].start_ms, 0);
        assert_eq!(all[0].lines.len(), 2);
    }

    #[test]
    fn going_back_to_a_slide_uses_its_reading() {
        let mut s = Sectioner::new(SectionConfig::default());
        s.push(keyframe(0, 1.0));
        s.push(NotesInput::Reading {
            t_ms: 0,
            reading: reading("Intro"),
        });
        s.push(keyframe(40, 0.9));
        s.push(NotesInput::NoReading { t_ms: 40_000 });
        s.push(NotesInput::Keyframe {
            t_ms: 60_000,
            file: "keyframes/0.png".into(),
            reason: KeyframeReason::Settled,
            change: 0.0,
            repeat_of: Some(0),
        });
        s.push(line(61, 20));
        let all = s.finish();
        let last = all.last().unwrap();
        let back = last.screens.iter().find(|x| x.t_ms == 60_000).unwrap();
        assert_eq!(back.reading.as_ref().unwrap().title, "Intro");
        assert_eq!(back.file, "keyframes/0.png");
    }
}
