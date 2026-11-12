//! Decides which frames are keyframes: the screen states worth keeping.
//!
//! Rules, in order:
//! - Cells that keep changing (a webcam thumbnail, a clock, a playing video)
//!   are "volatile" and ignored when judging settled changes.
//! - A change is taken once the screen has settled on it, so slide
//!   transitions and half-loaded pages are skipped. Its time is when the new
//!   state first appeared.
//! - Small changes (a typed line, one more bullet) wait until some time has
//!   passed since the last keyframe, so they are taken in batches.
//! - Returning to an earlier screen is reported as a repeat, not a new image.
//! - In [`ScreenMode::Auto`], a screen that never settles (a demo, scrolling,
//!   a video) is sampled now and then, if it differs enough from the last
//!   keyframe.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use super::fingerprint::{CELLS, Fingerprint, fraction};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScreenMode {
    /// Slides and demos: settled changes, plus samples of a moving screen.
    #[default]
    Auto,
    /// Only settled changes. For slide decks.
    Slides,
}

#[derive(Debug, Clone)]
pub struct KeyframeConfig {
    /// Share of the screen (0–1) that must differ for a change to count.
    pub change_threshold: f32,
    /// How long the screen must stay the same before a change is taken.
    pub settle_ms: u64,
    /// Changes smaller than this share of the screen ...
    pub small_change: f32,
    /// ... are only taken this long after the previous keyframe.
    pub small_change_gap_ms: u64,
    /// Auto mode: sample a screen that has been moving for this long.
    pub moving_after_ms: Option<u64>,
    /// Auto mode: at most one sample of a moving screen per this many ms.
    pub moving_gap_ms: u64,
    /// Auto mode: a sample must differ from the last keyframe by this much.
    pub moving_threshold: f32,
    /// How many earlier keyframes are checked for repeats.
    pub repeat_window: usize,
}

impl KeyframeConfig {
    pub fn for_mode(mode: ScreenMode) -> Self {
        let auto = Self::default();
        match mode {
            ScreenMode::Auto => auto,
            ScreenMode::Slides => Self {
                moving_after_ms: None,
                ..auto
            },
        }
    }
}

impl Default for KeyframeConfig {
    fn default() -> Self {
        Self {
            change_threshold: 0.005,
            settle_ms: 1000,
            small_change: 0.03,
            small_change_gap_ms: 10_000,
            moving_after_ms: Some(5_000),
            moving_gap_ms: 10_000,
            moving_threshold: 0.05,
            repeat_window: 20,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyframeReason {
    /// The first screen of the session.
    First,
    /// The screen changed and then held still.
    Settled,
    /// A sample of a screen that keeps moving.
    Moving,
}

impl KeyframeReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::First => "first",
            Self::Settled => "settled",
            Self::Moving => "moving",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Keep the current frame as a keyframe shown since `t_ms`.
    New {
        t_ms: u64,
        reason: KeyframeReason,
        /// Share of the screen that differs from the previous keyframe.
        change: f32,
    },
    /// The screen went back to the keyframe from `of_t_ms`.
    Repeat { t_ms: u64, of_t_ms: u64 },
}

/// The screen counts as still when the share of it that moved since the
/// previous frame is under this much of the change being waited on.
const STILL_RATIO: f32 = 0.1;

/// A cell changing in this many of the last 4 frames is volatile.
const VOLATILE_HITS: u32 = 3;

pub struct KeyframeSelector {
    cfg: KeyframeConfig,
    prev: Option<Fingerprint>,
    /// Per cell: one bit per recent frame, set when the cell changed.
    history: Vec<u8>,
    /// When the screen last changed, i.e. since when it has been still.
    still_since: u64,
    /// When the screen first differed from the current keyframe.
    pending_since: Option<u64>,
    current: Option<(u64, Fingerprint)>,
    last_new_at: u64,
    recent: VecDeque<(u64, Fingerprint)>,
}

impl KeyframeSelector {
    pub fn new(cfg: KeyframeConfig) -> Self {
        Self {
            cfg,
            prev: None,
            history: vec![0; CELLS],
            still_since: 0,
            pending_since: None,
            current: None,
            last_new_at: 0,
            recent: VecDeque::new(),
        }
    }

    /// Feeds one frame, captured at session time `t_ms`.
    pub fn push(&mut self, t_ms: u64, fp: Fingerprint) -> Option<Decision> {
        let (volatile, moved) = self.update_motion(&fp);

        // Change from the current keyframe: all cells, and steady cells only.
        let (raw_change, steady_change) = match &self.current {
            None => (1.0, 1.0),
            Some((_, current)) => {
                let changed = fp.changed(current);
                let raw = fraction(changed.iter().filter(|c| **c).count());
                let steady = changed
                    .iter()
                    .zip(&volatile)
                    .filter(|(c, v)| **c && !**v)
                    .count();
                (raw, fraction(steady))
            }
        };
        // The screen holds still when little moves compared with how much it
        // changed: a page that replaced the whole screen has settled even if
        // one animated thumbnail on it keeps playing.
        let tolerance = self.cfg.change_threshold.max(STILL_RATIO * steady_change);
        if moved.is_none_or(|m| m >= tolerance) {
            self.still_since = t_ms;
        }
        let settled = t_ms.saturating_sub(self.still_since) >= self.cfg.settle_ms;

        let Some((current_t, _)) = &self.current else {
            let since = *self.pending_since.get_or_insert(t_ms);
            let forced = self
                .cfg
                .moving_after_ms
                .is_some_and(|after| t_ms - since >= after);
            if settled || forced {
                let shown_since = if settled { self.still_since } else { t_ms };
                return Some(self.take_new(shown_since, t_ms, fp, KeyframeReason::First, 1.0));
            }
            return None;
        };
        let current_t = *current_t;

        if steady_change < self.cfg.change_threshold && raw_change < self.cfg.moving_threshold {
            self.pending_since = None;
            return None;
        }
        let pending_since = *self.pending_since.get_or_insert(t_ms);
        let since_last = t_ms.saturating_sub(self.last_new_at);

        if settled && steady_change >= self.cfg.change_threshold {
            if steady_change < self.cfg.small_change && since_last < self.cfg.small_change_gap_ms {
                return None;
            }
            let shown_since = self.still_since.max(current_t + 1);
            if let Some(of) = self.find_repeat(current_t, &fp) {
                return Some(self.take_repeat(shown_since, of));
            }
            return Some(self.take_new(
                shown_since,
                t_ms,
                fp,
                KeyframeReason::Settled,
                steady_change,
            ));
        }

        if let Some(after) = self.cfg.moving_after_ms
            && t_ms - pending_since >= after
            && since_last >= self.cfg.moving_gap_ms
            && raw_change >= self.cfg.moving_threshold
        {
            return Some(self.take_new(t_ms, t_ms, fp, KeyframeReason::Moving, raw_change));
        }
        None
    }

    /// Updates which cells are volatile. Returns the volatile mask and the
    /// share of steady cells that changed since the previous frame (`None`
    /// for the first frame).
    fn update_motion(&mut self, fp: &Fingerprint) -> (Vec<bool>, Option<f32>) {
        let changed = match &self.prev {
            Some(prev) => fp.changed(prev),
            None => vec![false; CELLS],
        };
        for (h, c) in self.history.iter_mut().zip(&changed) {
            *h = ((*h << 1) | *c as u8) & 0b1111;
        }
        let volatile: Vec<bool> = self
            .history
            .iter()
            .map(|h| h.count_ones() >= VOLATILE_HITS)
            .collect();
        let moved = changed
            .iter()
            .zip(&volatile)
            .filter(|(c, v)| **c && !**v)
            .count();
        let moved = self.prev.is_some().then(|| fraction(moved));
        self.prev = Some(fp.clone());
        (volatile, moved)
    }

    fn find_repeat(&self, current_t: u64, fp: &Fingerprint) -> Option<u64> {
        self.recent
            .iter()
            .rev()
            .find(|(t, k)| *t != current_t && fp.diff(k) < self.cfg.change_threshold)
            .map(|(t, _)| *t)
    }

    fn take_new(
        &mut self,
        shown_since: u64,
        now: u64,
        fp: Fingerprint,
        reason: KeyframeReason,
        change: f32,
    ) -> Decision {
        self.current = Some((shown_since, fp.clone()));
        self.recent.push_back((shown_since, fp));
        while self.recent.len() > self.cfg.repeat_window {
            self.recent.pop_front();
        }
        self.pending_since = None;
        self.last_new_at = now;
        Decision::New {
            t_ms: shown_since,
            reason,
            change,
        }
    }

    fn take_repeat(&mut self, shown_since: u64, of_t_ms: u64) -> Decision {
        let fp = self
            .recent
            .iter()
            .find(|(t, _)| *t == of_t_ms)
            .map(|(_, f)| f.clone())
            .expect("repeat comes from the recent list");
        self.current = Some((of_t_ms, fp));
        self.pending_since = None;
        Decision::Repeat {
            t_ms: shown_since,
            of_t_ms,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::fingerprint::{HEIGHT, WIDTH};
    use super::*;

    /// A screen made of a background and some filled rectangles
    /// (x, y, w, h in cells, with a brightness).
    fn screen(bg: u8, rects: &[(usize, usize, usize, usize, u8)]) -> Fingerprint {
        let mut cells = vec![bg; CELLS];
        for &(x, y, w, h, v) in rects {
            for row in y..(y + h).min(HEIGHT) {
                for col in x..(x + w).min(WIDTH) {
                    cells[row * WIDTH + col] = v;
                }
            }
        }
        Fingerprint::from_cells(cells)
    }

    fn slide(n: u8) -> Fingerprint {
        // A title bar and a body block whose shade identifies the slide.
        screen(240, &[(10, 5, 100, 8, 30), (10, 20, 100, 40, 40 + n * 30)])
    }

    /// Runs `frames` at 1 fps and returns the decisions with their frame time.
    fn run(mode: ScreenMode, frames: &[Fingerprint]) -> Vec<(u64, Decision)> {
        let mut s = KeyframeSelector::new(KeyframeConfig::for_mode(mode));
        frames
            .iter()
            .enumerate()
            .filter_map(|(i, f)| {
                let t = i as u64 * 1000;
                s.push(t, f.clone()).map(|d| (t, d))
            })
            .collect()
    }

    fn new_times(decisions: &[(u64, Decision)]) -> Vec<u64> {
        decisions
            .iter()
            .filter_map(|(_, d)| match d {
                Decision::New { t_ms, .. } => Some(*t_ms),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn one_keyframe_per_slide_timed_when_it_appeared() {
        let frames: Vec<_> = [0, 0, 0, 1, 1, 1, 1, 2, 2, 2]
            .iter()
            .map(|n| slide(*n))
            .collect();
        let d = run(ScreenMode::Slides, &frames);
        assert_eq!(new_times(&d), vec![0, 3000, 7000]);
        // Each is decided one second after it appeared, once it held still.
        assert_eq!(
            d.iter().map(|(t, _)| *t).collect::<Vec<_>>(),
            vec![1000, 4000, 8000]
        );
    }

    #[test]
    fn transition_frames_are_skipped() {
        let blend = screen(240, &[(10, 5, 100, 8, 30), (10, 20, 100, 40, 70)]);
        let frames = vec![slide(0), slide(0), blend, slide(2), slide(2)];
        let d = run(ScreenMode::Slides, &frames);
        assert_eq!(new_times(&d), vec![0, 3000]);
    }

    #[test]
    fn pointer_movement_is_ignored() {
        let frames: Vec<_> = (0..10)
            .map(|i| screen(240, &[(10, 20, 100, 40, 70), (i * 5, 30, 2, 2, 0)]))
            .collect();
        assert_eq!(new_times(&run(ScreenMode::Auto, &frames)), vec![0]);
    }

    #[test]
    fn going_back_to_a_slide_is_a_repeat() {
        let frames: Vec<_> = [0, 0, 1, 1, 0, 0].iter().map(|n| slide(*n)).collect();
        let d = run(ScreenMode::Slides, &frames);
        assert_eq!(new_times(&d), vec![0, 2000]);
        assert_eq!(
            d.last().unwrap().1,
            Decision::Repeat {
                t_ms: 4000,
                of_t_ms: 0
            }
        );
    }

    #[test]
    fn slides_change_while_a_webcam_thumbnail_plays() {
        // A 20 × 12 cell thumbnail (2.6% of the screen) flickers every frame.
        let with_cam = |n: u8, i: usize| {
            let mut f = slide(n).cells().to_vec();
            for row in 55..67 {
                for col in 105..125 {
                    f[row * WIDTH + col] = if i.is_multiple_of(2) { 20 } else { 200 };
                }
            }
            Fingerprint::from_cells(f)
        };
        let frames: Vec<_> = [0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1]
            .iter()
            .enumerate()
            .map(|(i, n)| with_cam(*n, i))
            .collect();
        let d = run(ScreenMode::Slides, &frames);
        assert_eq!(new_times(&d).len(), 2, "{d:?}");
        assert_eq!(new_times(&d)[1], 5000);
    }

    #[test]
    fn small_edits_are_batched() {
        // One more line of text every 2 s (1.4% of the screen each).
        let frames: Vec<_> = (0..30)
            .map(|i| {
                let lines = (i / 2).min(12);
                let rects: Vec<_> = (0..lines).map(|l| (10, 5 + l * 5, 100, 1, 0)).collect();
                screen(240, &rects)
            })
            .collect();
        // 13 edits become a keyframe each time about 3% has changed.
        let times = new_times(&run(ScreenMode::Slides, &frames));
        assert!(times.len() <= 5, "{times:?}");
        assert!(times.windows(2).all(|w| w[1] - w[0] >= 5000), "{times:?}");
    }

    #[test]
    fn a_moving_screen_is_sampled_only_in_auto_mode() {
        // Scrolling: the content shifts every frame.
        let frames: Vec<_> = (0..40)
            .map(|i| {
                let rects: Vec<_> = (0..14)
                    .map(|l| (10, (l * 5 + 40 - i % 40) % HEIGHT, 20 + (l * 37) % 90, 2, 0))
                    .collect();
                screen(240, &rects)
            })
            .collect();
        assert!(run(ScreenMode::Slides, &frames).is_empty());
        let times = new_times(&run(ScreenMode::Auto, &frames));
        assert!((3..=5).contains(&times.len()), "{times:?}");
        assert!(times.windows(2).all(|w| w[1] - w[0] >= 10_000), "{times:?}");
    }
}
