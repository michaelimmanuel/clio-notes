//! Live transcription of one audio stream: resample, cut at pauses, send each
//! utterance to the speech-to-text provider and report the results.

use std::io::Cursor;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use providers::{AudioInput, SpeechToText, TranscribeRequest};
use serde::Serialize;
use storage::{Speaker, TranscriptLine};
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinSet;

use crate::resample::{PIPELINE_RATE, Resampler};
use crate::segment::{SegmentConfig, Segmenter, Utterance};

/// What the pipeline reports about each utterance, in order per utterance:
/// `Detected`, then exactly one of `Transcribed`, `Empty` or `Failed`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PipelineEvent {
    /// Speech was cut out and sent for transcription.
    Detected {
        speaker: Speaker,
        start_ms: u64,
        end_ms: u64,
    },
    Transcribed {
        line: TranscriptLine,
        /// From the utterance being cut (the end of the pause) to the text
        /// arriving. Speech-to-text latency as the user sees it is this plus
        /// the pause length the segmenter waits for.
        latency_ms: u64,
    },
    /// The provider found no words.
    Empty {
        speaker: Speaker,
        start_ms: u64,
        end_ms: u64,
    },
    Failed {
        speaker: Speaker,
        start_ms: u64,
        end_ms: u64,
        message: String,
    },
}

#[derive(Debug, Clone)]
pub struct StreamOptions {
    pub speaker: Speaker,
    /// Sample rate of the incoming mono blocks.
    pub input_rate: u32,
    pub segment: SegmentConfig,
    /// ISO-639-1 code; v1 is English only.
    pub language: Option<String>,
    /// Terms likely to be spoken (later: read from the screen).
    pub keywords: Vec<String>,
    /// Requests running at once for this stream. More than one keeps
    /// back-to-back utterances from queueing behind a slow request.
    pub max_in_flight: usize,
    /// Recorded on every line as the model that produced it.
    pub model: String,
}

impl StreamOptions {
    pub fn new(speaker: Speaker, input_rate: u32, model: impl Into<String>) -> Self {
        Self {
            speaker,
            input_rate,
            segment: SegmentConfig::default(),
            language: Some("en".into()),
            keywords: Vec::new(),
            max_in_flight: 3,
            model: model.into(),
        }
    }
}

/// How much of the previous text is passed as context to the next request.
const CONTEXT_CHARS: usize = 200;

/// Runs until `input` closes and every request has finished.
///
/// `input` carries mono blocks at `opts.input_rate`, aligned so that sample
/// `n` is session time `n / input_rate` (what [`capture::AudioTap`] delivers
/// and what a recorded WAV contains). Events go to `events`; a closed
/// receiver is ignored.
pub async fn transcribe_stream(
    opts: StreamOptions,
    mut input: mpsc::UnboundedReceiver<Vec<f32>>,
    stt: Arc<dyn SpeechToText>,
    events: mpsc::UnboundedSender<PipelineEvent>,
) {
    let mut resampler = Resampler::new(opts.input_rate, PIPELINE_RATE);
    let mut segmenter = Segmenter::new(opts.segment.clone());
    let slots = Arc::new(Semaphore::new(opts.max_in_flight.max(1)));
    let context = Arc::new(Mutex::new(String::new()));
    let mut tasks = JoinSet::new();

    let dispatch = async |u: Utterance, tasks: &mut JoinSet<()>| {
        let cut_at = Instant::now();
        let _ = events.send(PipelineEvent::Detected {
            speaker: opts.speaker.clone(),
            start_ms: u.start_ms,
            end_ms: u.end_ms(),
        });
        // At most `max_in_flight` requests run at once; the rest wait in
        // their own tasks, so cutting new utterances never stops, even when
        // the provider is unreachable for minutes.
        let (opts, stt, events, context, slots) = (
            opts.clone(),
            stt.clone(),
            events.clone(),
            context.clone(),
            slots.clone(),
        );
        tasks.spawn(async move {
            let permit = slots.acquire_owned().await.expect("never closed");
            let event = transcribe_one(&opts, &u, stt.as_ref(), &context, cut_at).await;
            drop(permit);
            let _ = events.send(event);
        });
        while tasks.try_join_next().is_some() {}
    };

    while let Some(block) = input.recv().await {
        let samples = resampler.push(&block);
        for u in segmenter.push(&samples) {
            dispatch(u, &mut tasks).await;
        }
    }
    let tail = resampler.finish();
    let mut last = segmenter.push(&tail);
    last.extend(segmenter.finish());
    for u in last {
        dispatch(u, &mut tasks).await;
    }
    while tasks.join_next().await.is_some() {}
}

async fn transcribe_one(
    opts: &StreamOptions,
    u: &Utterance,
    stt: &dyn SpeechToText,
    context: &Mutex<String>,
    cut_at: Instant,
) -> PipelineEvent {
    let (start_ms, end_ms) = (u.start_ms, u.end_ms());
    let mut request = TranscribeRequest::new(AudioInput::wav(encode_wav(&u.samples)));
    request.language = opts.language.clone();
    request.keywords = opts.keywords.clone();
    let previous = context.lock().unwrap().clone();
    request.prompt = (!previous.is_empty()).then_some(previous);

    match stt.transcribe(&request).await {
        Ok(t) => {
            let text = t.text.split_whitespace().collect::<Vec<_>>().join(" ");
            if !text.chars().any(char::is_alphanumeric) {
                return PipelineEvent::Empty {
                    speaker: opts.speaker.clone(),
                    start_ms,
                    end_ms,
                };
            }
            *context.lock().unwrap() = tail_chars(&text, CONTEXT_CHARS);
            PipelineEvent::Transcribed {
                line: TranscriptLine {
                    speaker: opts.speaker.clone(),
                    start_ms,
                    end_ms,
                    text,
                    model: opts.model.clone(),
                },
                latency_ms: cut_at.elapsed().as_millis() as u64,
            }
        }
        Err(e) => PipelineEvent::Failed {
            speaker: opts.speaker.clone(),
            start_ms,
            end_ms,
            message: e.to_string(),
        },
    }
}

/// 16 kHz mono 16-bit WAV, the format every speech API accepts.
pub fn encode_wav(samples: &[f32]) -> Vec<u8> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: PIPELINE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut bytes = Vec::with_capacity(44 + samples.len() * 2);
    {
        let mut w = hound::WavWriter::new(Cursor::new(&mut bytes), spec).expect("in-memory WAV");
        for s in samples {
            w.write_sample((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
                .expect("in-memory WAV");
        }
        w.finalize().expect("in-memory WAV");
    }
    bytes
}

fn tail_chars(text: &str, n: usize) -> String {
    let count = text.chars().count();
    text.chars().skip(count.saturating_sub(n)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use providers::{ProviderError, Transcript};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Returns a canned answer per call and records the requests.
    struct FakeStt {
        calls: AtomicUsize,
        prompts: Mutex<Vec<Option<String>>>,
        answers: Vec<std::result::Result<&'static str, &'static str>>,
    }

    #[async_trait]
    impl SpeechToText for FakeStt {
        async fn transcribe(&self, r: &TranscribeRequest) -> providers::Result<Transcript> {
            assert_eq!(&r.audio.data[..4], b"RIFF");
            self.prompts.lock().unwrap().push(r.prompt.clone());
            let i = self.calls.fetch_add(1, Ordering::SeqCst);
            match self.answers[i % self.answers.len()] {
                Ok(text) => Ok(Transcript {
                    text: text.into(),
                    segments: vec![],
                }),
                Err(msg) => Err(ProviderError::Unavailable {
                    status: 503,
                    detail: msg.into(),
                }),
            }
        }
    }

    /// A 48 kHz signal that earshot hears as speech: a buzzy voice-like tone
    /// whose pitch and loudness keep moving.
    fn voice_like(seconds: f64) -> Vec<f32> {
        let rate = 48_000.0;
        (0..(seconds * rate) as usize)
            .map(|i| {
                let t = i as f64 / rate;
                let f0 = 140.0 + 30.0 * (2.0 * std::f64::consts::PI * 3.0 * t).sin();
                let env = 0.6 + 0.4 * (2.0 * std::f64::consts::PI * 4.0 * t).sin();
                let mut s = 0.0;
                for h in 1..=12 {
                    let formant = (-((h as f64 * f0 - 700.0) / 500.0).powi(2)).exp()
                        + 0.5 * (-((h as f64 * f0 - 1200.0) / 400.0).powi(2)).exp();
                    s += formant * (2.0 * std::f64::consts::PI * h as f64 * f0 * t).sin();
                }
                (s * env * 0.15) as f32
            })
            .collect()
    }

    async fn run(input: Vec<f32>, stt: Arc<FakeStt>) -> Vec<PipelineEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        let (etx, mut erx) = mpsc::unbounded_channel();
        for block in input.chunks(4800) {
            tx.send(block.to_vec()).unwrap();
        }
        drop(tx);
        let opts = StreamOptions::new(Speaker::Others, 48_000, "fake");
        transcribe_stream(opts, rx, stt, etx).await;
        let mut events = Vec::new();
        while let Some(e) = erx.recv().await {
            events.push(e);
        }
        events
    }

    fn silence(seconds: f64) -> Vec<f32> {
        vec![0.0; (seconds * 48_000.0) as usize]
    }

    #[tokio::test]
    async fn each_utterance_is_detected_then_resolved() {
        let mut input = silence(1.0);
        input.extend(voice_like(2.0));
        input.extend(silence(1.5));
        input.extend(voice_like(1.5));
        input.extend(silence(1.5));
        let stt = Arc::new(FakeStt {
            calls: AtomicUsize::new(0),
            prompts: Mutex::new(vec![]),
            answers: vec![Ok("  hello   there "), Err("upstream down")],
        });
        let events = run(input, stt.clone()).await;

        let detected: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                PipelineEvent::Detected {
                    start_ms, end_ms, ..
                } => Some((*start_ms, *end_ms)),
                _ => None,
            })
            .collect();
        assert_eq!(detected.len(), 2, "{events:#?}");
        // Utterances line up with where the speech is (within VAD slack).
        assert!((600..=1100).contains(&detected[0].0), "{detected:?}");
        assert!((2900..=3600).contains(&detected[0].1), "{detected:?}");
        assert!((4100..=4600).contains(&detected[1].0), "{detected:?}");

        let transcribed: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                PipelineEvent::Transcribed { line, .. } => Some(line.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(transcribed, vec!["hello there"]);
        assert!(events.iter().any(
            |e| matches!(e, PipelineEvent::Failed { message, .. } if message.contains("HTTP 503"))
        ));
    }

    #[tokio::test]
    async fn silence_sends_nothing() {
        let stt = Arc::new(FakeStt {
            calls: AtomicUsize::new(0),
            prompts: Mutex::new(vec![]),
            answers: vec![Ok("x")],
        });
        let events = run(silence(5.0), stt.clone()).await;
        assert!(events.is_empty());
        assert_eq!(stt.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn wav_is_16_khz_mono() {
        let wav = encode_wav(&[0.0, 0.5, -0.5]);
        let r = hound::WavReader::new(Cursor::new(wav)).unwrap();
        assert_eq!(r.spec().sample_rate, 16_000);
        assert_eq!(r.spec().channels, 1);
        assert_eq!(r.len(), 3);
    }
}
