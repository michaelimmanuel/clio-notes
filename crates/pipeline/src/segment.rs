//! Voice-activity detection and cutting a 16 kHz stream into utterances.

use std::collections::VecDeque;

use crate::resample::PIPELINE_RATE;

/// earshot's frame: 256 samples, 16 ms at 16 kHz.
pub const FRAME: usize = 256;
const FRAME_MS: u64 = 16;

#[derive(Debug, Clone)]
pub struct SegmentConfig {
    /// A frame scoring at least this starts or continues speech.
    pub speech_threshold: f32,
    /// While speaking, a frame scoring below this counts towards a pause.
    pub silence_threshold: f32,
    /// A pause this long ends the utterance. Shorter means lower latency but
    /// more mid-sentence cuts, which hurt transcription accuracy.
    pub min_pause_ms: u64,
    /// Utterances are cut at this length even without a pause, so a long
    /// monologue still shows up live.
    pub max_utterance_ms: u64,
    /// When forced to cut, the cut goes at the quietest frame in this last stretch.
    pub cut_search_ms: u64,
    /// Audio kept before the first speech frame, so soft word starts are not lost.
    pub pre_roll_ms: u64,
    /// Pause kept after the last speech frame.
    pub post_roll_ms: u64,
    /// Utterances with less speech than this are dropped (coughs, clicks).
    pub min_speech_ms: u64,
}

impl Default for SegmentConfig {
    fn default() -> Self {
        Self {
            speech_threshold: 0.5,
            silence_threshold: 0.35,
            min_pause_ms: 600,
            max_utterance_ms: 12_000,
            cut_search_ms: 3_000,
            pre_roll_ms: 250,
            post_roll_ms: 200,
            min_speech_ms: 300,
        }
    }
}

/// A stretch of speech, 16 kHz mono.
#[derive(Debug, Clone)]
pub struct Utterance {
    /// Session time of the first sample.
    pub start_ms: u64,
    pub samples: Vec<f32>,
    /// Frames that scored as speech.
    pub speech_ms: u64,
}

impl Utterance {
    pub fn end_ms(&self) -> u64 {
        self.start_ms + self.samples.len() as u64 * 1000 / PIPELINE_RATE as u64
    }
}

/// Runs VAD on 16 kHz audio and returns utterances as they close.
pub struct Segmenter {
    cfg: SegmentConfig,
    vad: Box<earshot::Detector>,
    /// Samples waiting to fill a frame.
    partial: Vec<f32>,
    /// Absolute 16 kHz sample index of the next complete frame.
    next_frame_at: u64,
    state: State,
}

enum State {
    Idle {
        /// The most recent audio, for pre-roll.
        recent: VecDeque<f32>,
    },
    Speaking(Current),
}

struct Current {
    start_sample: u64,
    samples: Vec<f32>,
    /// One score per frame in `samples` (pre-roll frames get 0).
    scores: Vec<f32>,
    pause_frames: usize,
}

impl Segmenter {
    pub fn new(cfg: SegmentConfig) -> Self {
        Self {
            cfg,
            vad: earshot::Detector::default_boxed(),
            partial: Vec::with_capacity(FRAME),
            next_frame_at: 0,
            state: State::Idle {
                recent: VecDeque::new(),
            },
        }
    }

    /// Feeds 16 kHz samples; returns any utterances that closed.
    pub fn push(&mut self, samples: &[f32]) -> Vec<Utterance> {
        let mut out = Vec::new();
        let mut rest = samples;
        while !rest.is_empty() {
            let take = (FRAME - self.partial.len()).min(rest.len());
            // Resampling can overshoot a little on full-scale audio; earshot
            // and the WAV encoder both need [-1, 1].
            self.partial
                .extend(rest[..take].iter().map(|s| s.clamp(-1.0, 1.0)));
            rest = &rest[take..];
            if self.partial.len() == FRAME {
                let frame = std::mem::take(&mut self.partial);
                let score = self.vad.predict_f32(&frame);
                if let Some(u) = self.on_frame(&frame, score) {
                    out.push(u);
                }
                self.partial = frame;
                self.partial.clear();
            }
        }
        out
    }

    /// Ends the stream and returns the utterance in progress, if any.
    pub fn finish(&mut self) -> Option<Utterance> {
        let state = std::mem::replace(
            &mut self.state,
            State::Idle {
                recent: VecDeque::new(),
            },
        );
        match state {
            State::Speaking(mut cur) => {
                cur.samples.append(&mut self.partial);
                let keep_pause = self.frames(self.cfg.post_roll_ms);
                let trim = cur.pause_frames.saturating_sub(keep_pause) * FRAME;
                let len = cur.samples.len().saturating_sub(trim);
                cur.samples.truncate(len);
                self.emit(cur)
            }
            State::Idle { .. } => None,
        }
    }

    fn frames(&self, ms: u64) -> usize {
        (ms / FRAME_MS) as usize
    }

    fn on_frame(&mut self, frame: &[f32], score: f32) -> Option<Utterance> {
        let frame_start = self.next_frame_at;
        self.next_frame_at += FRAME as u64;
        let cfg = self.cfg.clone();

        match &mut self.state {
            State::Idle { recent } => {
                if score >= cfg.speech_threshold {
                    let pre: Vec<f32> = recent.drain(..).collect();
                    let pre_frames = pre.len() / FRAME;
                    let mut samples = pre;
                    samples.extend_from_slice(frame);
                    let mut scores = vec![0.0; pre_frames];
                    scores.push(score);
                    self.state = State::Speaking(Current {
                        start_sample: frame_start - (pre_frames * FRAME) as u64,
                        samples,
                        scores,
                        pause_frames: 0,
                    });
                } else {
                    recent.extend(frame.iter().copied());
                    let max = (cfg.pre_roll_ms / FRAME_MS) as usize * FRAME;
                    while recent.len() > max {
                        recent.pop_front();
                    }
                }
                None
            }
            State::Speaking(cur) => {
                cur.samples.extend_from_slice(frame);
                cur.scores.push(score);
                if score < cfg.silence_threshold {
                    cur.pause_frames += 1;
                } else {
                    cur.pause_frames = 0;
                }

                if cur.pause_frames >= (cfg.min_pause_ms / FRAME_MS) as usize {
                    return self.close_at_pause();
                }
                if cur.samples.len() as u64 * 1000 / PIPELINE_RATE as u64 >= cfg.max_utterance_ms {
                    return self.force_cut();
                }
                None
            }
        }
    }

    /// The pause is long enough: emit the utterance without most of the pause,
    /// and keep the pause's tail as pre-roll for the next one.
    fn close_at_pause(&mut self) -> Option<Utterance> {
        let pre_roll = self.frames(self.cfg.pre_roll_ms) * FRAME;
        let post_roll = self.frames(self.cfg.post_roll_ms);
        let State::Speaking(mut cur) = std::mem::replace(
            &mut self.state,
            State::Idle {
                recent: VecDeque::new(),
            },
        ) else {
            unreachable!()
        };
        let trim_frames = cur.pause_frames.saturating_sub(post_roll);
        let keep = cur.samples.len() - trim_frames * FRAME;
        let tail = cur.samples.split_off(keep);
        cur.scores.truncate(cur.scores.len() - trim_frames);
        let recent: VecDeque<f32> = tail[tail.len().saturating_sub(pre_roll)..]
            .iter()
            .copied()
            .collect();
        self.state = State::Idle { recent };
        self.emit(cur)
    }

    /// Too long without a pause: cut at the quietest recent frame and keep
    /// talking from there.
    fn force_cut(&mut self) -> Option<Utterance> {
        let search = self.frames(self.cfg.cut_search_ms).max(1);
        let State::Speaking(cur) = &mut self.state else {
            unreachable!()
        };
        let n = cur.scores.len();
        let from = n.saturating_sub(search);
        let cut = (from..n)
            .min_by(|&a, &b| cur.scores[a].total_cmp(&cur.scores[b]))
            .unwrap_or(n - 1)
            + 1;
        let rest_samples = cur.samples.split_off(cut * FRAME);
        let rest_scores = cur.scores.split_off(cut);
        let next = Current {
            start_sample: cur.start_sample + (cut * FRAME) as u64,
            pause_frames: rest_scores
                .iter()
                .rev()
                .take_while(|&&s| s < self.cfg.silence_threshold)
                .count(),
            samples: rest_samples,
            scores: rest_scores,
        };
        let State::Speaking(done) = std::mem::replace(&mut self.state, State::Speaking(next))
        else {
            unreachable!()
        };
        self.emit(done)
    }

    fn emit(&self, cur: Current) -> Option<Utterance> {
        let speech_frames = cur
            .scores
            .iter()
            .filter(|&&s| s >= self.cfg.speech_threshold)
            .count();
        let speech_ms = speech_frames as u64 * FRAME_MS;
        (speech_ms >= self.cfg.min_speech_ms && !cur.samples.is_empty()).then(|| Utterance {
            start_ms: cur.start_sample * 1000 / PIPELINE_RATE as u64,
            samples: cur.samples,
            speech_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drives the state machine with given scores, bypassing the VAD model.
    fn run(cfg: SegmentConfig, scores: &[f32]) -> Vec<Utterance> {
        let mut s = Segmenter::new(cfg);
        let frame = vec![0.01f32; FRAME];
        let mut out = Vec::new();
        for &score in scores {
            out.extend(s.on_frame(&frame, score));
        }
        out.extend(s.finish());
        out
    }

    fn frames(ms: u64) -> usize {
        (ms / FRAME_MS) as usize
    }

    fn scores(parts: &[(f32, u64)]) -> Vec<f32> {
        parts
            .iter()
            .flat_map(|&(s, ms)| std::iter::repeat_n(s, frames(ms)))
            .collect()
    }

    #[test]
    fn splits_on_pauses_with_pre_and_post_roll() {
        let cfg = SegmentConfig::default();
        let s = scores(&[
            (0.0, 1008),
            (0.9, 992),
            (0.0, 1008),
            (0.9, 496),
            (0.0, 1008),
        ]);
        let u = run(cfg.clone(), &s);
        assert_eq!(u.len(), 2);
        // Speech starts at 1008 ms; pre-roll is 240 ms (15 whole frames).
        assert_eq!(u[0].start_ms, 1008 - 240);
        // Ends 192 ms (12 frames of post-roll) after the speech.
        assert_eq!(u[0].end_ms(), 2000 + 192);
        assert_eq!(u[0].speech_ms, 992);
        assert_eq!(u[1].start_ms, 3008 - 240);
        assert_eq!(u[1].end_ms(), 3504 + 192);
    }

    #[test]
    fn short_pauses_do_not_split() {
        let s = scores(&[(0.9, 1504), (0.0, 400), (0.9, 1504), (0.0, 1008)]);
        let u = run(SegmentConfig::default(), &s);
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].speech_ms, 3008);
    }

    #[test]
    fn blips_are_dropped() {
        let s = scores(&[(0.0, 496), (0.9, 112), (0.0, 1008)]);
        assert!(run(SegmentConfig::default(), &s).is_empty());
    }

    #[test]
    fn long_speech_is_cut_at_the_quietest_point() {
        // 10 s of speech, a 0.4-score dip that is not a pause, then 8 s more.
        let s = scores(&[(0.9, 10_000), (0.4, 96), (0.9, 8000), (0.0, 1008)]);
        let u = run(SegmentConfig::default(), &s);
        assert_eq!(u.len(), 2);
        let first_end = u[0].end_ms();
        assert!((10_000..=10_100).contains(&first_end), "cut at {first_end}");
        assert_eq!(u[1].start_ms, first_end);
    }

    #[test]
    fn speech_running_into_the_end_is_kept() {
        let s = scores(&[(0.0, 496), (0.9, 800)]);
        let u = run(SegmentConfig::default(), &s);
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].end_ms(), 1296);
    }

    #[test]
    fn real_vad_finds_no_speech_in_silence() {
        let mut s = Segmenter::new(SegmentConfig::default());
        assert!(s.push(&vec![0.0; 16_000 * 3]).is_empty());
        assert!(s.finish().is_none());
    }
}
