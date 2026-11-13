//! Keeps a meeting going through provider outages, and counts what each role
//! uses.
//!
//! Every provider client used during a meeting is wrapped in [`Resilient`].
//! A call that fails for a reason that can pass (network down, timeout,
//! rate limit, provider overloaded) is retried with growing pauses until it
//! succeeds or the [`Patience`] runs out. Nothing is dropped meanwhile:
//! utterances, keyframes and sections wait in their own tasks with their
//! own timestamps, and the notes wait for the transcript. When the
//! connection is back, the waiting calls go through in turn.
//!
//! Each [`Link`] reports its state as [`LinkEvent`]s (lost, still waiting,
//! back) and keeps a [`RoleUsage`] tally for the meeting.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use providers::{
    ProviderError, Role, SpeechToText, TextGenerator, TextRequest, TextResponse, TranscribeRequest,
    Transcript,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

/// What one role used during a meeting.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RoleUsage {
    /// Calls that returned an answer.
    pub calls: u64,
    /// Calls that gave up with an error.
    pub failed: u64,
    /// Extra attempts after a failure that could pass.
    pub retries: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Audio sent for transcription (speech to text only).
    pub audio_ms: u64,
}

impl RoleUsage {
    pub fn add(&mut self, other: &RoleUsage) {
        self.calls += other.calls;
        self.failed += other.failed;
        self.retries += other.retries;
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.audio_ms += other.audio_ms;
    }

    pub fn is_empty(&self) -> bool {
        *self == RoleUsage::default()
    }
}

/// Usage per role, as stored in the timeline (`usage` meta).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MeetingUsage {
    pub speech_to_text: RoleUsage,
    pub vision: RoleUsage,
    /// Live notes and term corrections.
    pub text: RoleUsage,
    /// The end-of-meeting pass (also a text-generation call).
    #[serde(rename = "final")]
    pub final_pass: RoleUsage,
}

impl MeetingUsage {
    pub fn role_mut(&mut self, role: Role) -> &mut RoleUsage {
        match role {
            Role::SpeechToText => &mut self.speech_to_text,
            Role::Vision => &mut self.vision,
            Role::Text => &mut self.text,
        }
    }
}

/// How the connection of one role is doing, sent when it changes.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LinkEvent {
    /// Calls are failing and being retried. Sent when the first call
    /// starts waiting and again whenever the number waiting changes.
    Retrying {
        role: Role,
        message: String,
        /// Calls waiting to be retried.
        waiting: usize,
        /// How long the connection has been down.
        down_ms: u64,
    },
    /// A call went through after an outage.
    Restored { role: Role, down_ms: u64 },
    /// A call gave up after waiting, so its result is missing.
    GaveUp { role: Role, message: String },
}

/// How long calls keep retrying.
///
/// While the meeting runs, a call waits up to `max_wait`. Once the meeting
/// is stopping, every call gives up `after_stop` later, so stopping during
/// an outage does not hang for long.
#[derive(Debug)]
pub struct Patience {
    max_wait: Duration,
    after_stop: Duration,
    stopped_at: Mutex<Option<Instant>>,
}

impl Patience {
    pub fn new(max_wait: Duration, after_stop: Duration) -> Arc<Self> {
        Arc::new(Self {
            max_wait,
            after_stop,
            stopped_at: Mutex::new(None),
        })
    }

    /// For a meeting: up to 20 minutes per call, 90 seconds after stop.
    pub fn meeting() -> Arc<Self> {
        Self::new(Duration::from_secs(20 * 60), Duration::from_secs(90))
    }

    /// The meeting is stopping.
    pub fn stop(&self) {
        self.stopped_at
            .lock()
            .unwrap()
            .get_or_insert_with(Instant::now);
    }

    fn exhausted(&self, started: Instant) -> bool {
        if started.elapsed() >= self.max_wait {
            return true;
        }
        match *self.stopped_at.lock().unwrap() {
            Some(at) => at.elapsed() >= self.after_stop,
            None => false,
        }
    }
}

/// Pauses between retries: 2, 4, 8, 15, then 30 seconds.
fn backoff(attempt: u32) -> Duration {
    const STEPS: [u64; 5] = [2, 4, 8, 15, 30];
    Duration::from_secs(STEPS[(attempt as usize).min(STEPS.len() - 1)])
}

#[derive(Default)]
struct LinkState {
    usage: RoleUsage,
    /// When the current outage began, if one is going on.
    down_since: Option<Instant>,
    last_waiting_sent: usize,
}

/// The connection of one role during one meeting.
pub struct Link {
    role: Role,
    patience: Arc<Patience>,
    events: Option<mpsc::UnboundedSender<LinkEvent>>,
    state: Mutex<LinkState>,
    waiting: AtomicUsize,
    /// The first pause between retries; shortened in tests.
    unit: Duration,
}

impl Link {
    pub fn new(
        role: Role,
        patience: Arc<Patience>,
        events: Option<mpsc::UnboundedSender<LinkEvent>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            role,
            patience,
            events,
            state: Mutex::new(LinkState::default()),
            waiting: AtomicUsize::new(0),
            unit: Duration::from_secs(1),
        })
    }

    pub fn role(&self) -> Role {
        self.role
    }

    /// What this role has used so far.
    pub fn usage(&self) -> RoleUsage {
        self.state.lock().unwrap().usage.clone()
    }

    fn send(&self, event: LinkEvent) {
        if let Some(tx) = &self.events {
            let _ = tx.send(event);
        }
    }

    /// Runs `call` until it succeeds, fails for good, or patience runs out.
    async fn run<T, F, Fut>(&self, mut call: F) -> providers::Result<T>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = providers::Result<T>>,
    {
        let started = Instant::now();
        let mut attempt = 0u32;
        let mut is_waiting = false;
        let result = loop {
            match call().await {
                Ok(value) => {
                    let down = {
                        let mut s = self.state.lock().unwrap();
                        s.usage.calls += 1;
                        s.usage.retries += attempt as u64;
                        // The outage is over once nothing else is waiting.
                        let others = self.waiting.load(Ordering::SeqCst) - usize::from(is_waiting);
                        match s.down_since {
                            Some(since) if others == 0 => {
                                s.down_since = None;
                                s.last_waiting_sent = 0;
                                Some(since.elapsed())
                            }
                            _ => None,
                        }
                    };
                    if let Some(down) = down {
                        self.send(LinkEvent::Restored {
                            role: self.role,
                            down_ms: down.as_millis() as u64,
                        });
                    }
                    break Ok(value);
                }
                Err(e) if !e.is_retryable() || self.patience.exhausted(started) => {
                    self.state.lock().unwrap().usage.failed += 1;
                    if attempt > 0 {
                        self.send(LinkEvent::GaveUp {
                            role: self.role,
                            message: e.to_string(),
                        });
                    }
                    break Err(e);
                }
                Err(e) => {
                    if !is_waiting {
                        is_waiting = true;
                        self.waiting.fetch_add(1, Ordering::SeqCst);
                    }
                    self.report_retrying(&e);
                    let pause = match &e {
                        ProviderError::RateLimited {
                            retry_after_secs: Some(s),
                            ..
                        } => Duration::from_secs(*s).max(self.unit),
                        _ => backoff(attempt).mul_f64(self.unit.as_secs_f64()),
                    };
                    tokio::time::sleep(pause.min(Duration::from_secs(60))).await;
                    attempt += 1;
                }
            }
        };
        if is_waiting {
            self.waiting.fetch_sub(1, Ordering::SeqCst);
        }
        result
    }

    fn report_retrying(&self, error: &ProviderError) {
        let waiting = self.waiting.load(Ordering::SeqCst);
        let down_ms = {
            let mut s = self.state.lock().unwrap();
            let since = *s.down_since.get_or_insert_with(Instant::now);
            if s.last_waiting_sent == waiting {
                return;
            }
            s.last_waiting_sent = waiting;
            since.elapsed().as_millis() as u64
        };
        self.send(LinkEvent::Retrying {
            role: self.role,
            message: error.to_string(),
            waiting,
            down_ms,
        });
    }

    fn count(&self, f: impl FnOnce(&mut RoleUsage)) {
        f(&mut self.state.lock().unwrap().usage);
    }
}

/// A provider client that retries through outages and counts usage.
pub struct Resilient<T: ?Sized> {
    inner: Arc<T>,
    link: Arc<Link>,
}

impl<T: ?Sized> Resilient<T> {
    pub fn new(inner: Arc<T>, link: Arc<Link>) -> Self {
        Self { inner, link }
    }
}

impl Resilient<dyn TextGenerator> {
    pub fn text(inner: Arc<dyn TextGenerator>, link: Arc<Link>) -> Arc<dyn TextGenerator> {
        Arc::new(Self::new(inner, link))
    }
}

impl Resilient<dyn SpeechToText> {
    pub fn speech(inner: Arc<dyn SpeechToText>, link: Arc<Link>) -> Arc<dyn SpeechToText> {
        Arc::new(Self::new(inner, link))
    }
}

#[async_trait]
impl TextGenerator for Resilient<dyn TextGenerator> {
    async fn generate(&self, request: &TextRequest) -> providers::Result<TextResponse> {
        let response = self.link.run(|| self.inner.generate(request)).await?;
        self.link.count(|u| {
            u.input_tokens += response.usage.input_tokens;
            u.output_tokens += response.usage.output_tokens;
        });
        Ok(response)
    }
}

#[async_trait]
impl SpeechToText for Resilient<dyn SpeechToText> {
    async fn transcribe(&self, request: &TranscribeRequest) -> providers::Result<Transcript> {
        let transcript = self.link.run(|| self.inner.transcribe(request)).await?;
        let ms = wav_duration_ms(&request.audio.data);
        self.link.count(|u| u.audio_ms += ms);
        Ok(transcript)
    }
}

/// Length of a 16-bit mono WAV made by [`crate::transcribe::encode_wav`].
fn wav_duration_ms(wav: &[u8]) -> u64 {
    let samples = wav.len().saturating_sub(44) as u64 / 2;
    samples * 1000 / crate::resample::PIPELINE_RATE as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use providers::Usage;
    use std::sync::atomic::AtomicU32;

    /// Fails with `error` for the first `fail` calls, then answers.
    struct Flaky {
        fail: u32,
        calls: AtomicU32,
        error: ProviderError,
    }

    #[async_trait]
    impl TextGenerator for Flaky {
        async fn generate(&self, _: &TextRequest) -> providers::Result<TextResponse> {
            if self.calls.fetch_add(1, Ordering::SeqCst) < self.fail {
                return Err(self.error.clone());
            }
            Ok(TextResponse {
                text: "ok".into(),
                model: "m".into(),
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 3,
                },
            })
        }
    }

    fn offline() -> ProviderError {
        ProviderError::Network {
            host: "api.example.com".into(),
            detail: "dns error".into(),
        }
    }

    fn link(patience: Arc<Patience>) -> (Arc<Link>, mpsc::UnboundedReceiver<LinkEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut link = Link::new(Role::Text, patience, Some(tx));
        Arc::get_mut(&mut link).unwrap().unit = Duration::from_millis(1);
        (link, rx)
    }

    fn drain(rx: &mut mpsc::UnboundedReceiver<LinkEvent>) -> Vec<LinkEvent> {
        let mut out = Vec::new();
        while let Ok(e) = rx.try_recv() {
            out.push(e);
        }
        out
    }

    #[tokio::test]
    async fn an_outage_is_waited_out_and_reported() {
        let (link, mut rx) = link(Patience::meeting());
        let inner = Arc::new(Flaky {
            fail: 3,
            calls: AtomicU32::new(0),
            error: offline(),
        });
        let g = Resilient::text(inner, link.clone());
        let r = g.generate(&TextRequest::new("x")).await.unwrap();
        assert_eq!(r.text, "ok");

        let events = drain(&mut rx);
        assert!(matches!(
            events.first(),
            Some(LinkEvent::Retrying { waiting: 1, .. })
        ));
        assert!(matches!(events.last(), Some(LinkEvent::Restored { .. })));
        let usage = link.usage();
        assert_eq!((usage.calls, usage.retries, usage.failed), (1, 3, 0));
        assert_eq!((usage.input_tokens, usage.output_tokens), (10, 3));
    }

    #[tokio::test]
    async fn errors_that_cannot_pass_are_not_retried() {
        let (link, mut rx) = link(Patience::meeting());
        let inner = Arc::new(Flaky {
            fail: 5,
            calls: AtomicU32::new(0),
            error: ProviderError::InvalidKey { detail: "x".into() },
        });
        let g = Resilient::text(inner.clone(), link.clone());
        assert!(g.generate(&TextRequest::new("x")).await.is_err());
        assert_eq!(inner.calls.load(Ordering::SeqCst), 1);
        assert!(drain(&mut rx).is_empty());
        assert_eq!(link.usage().failed, 1);
    }

    #[tokio::test]
    async fn stopping_ends_the_wait() {
        let patience = Patience::new(Duration::from_secs(600), Duration::ZERO);
        let (link, mut rx) = link(patience.clone());
        let inner = Arc::new(Flaky {
            fail: u32::MAX,
            calls: AtomicU32::new(0),
            error: offline(),
        });
        let g = Resilient::text(inner, link.clone());
        let call = tokio::spawn(async move { g.generate(&TextRequest::new("x")).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        patience.stop();
        let result = tokio::time::timeout(Duration::from_secs(5), call)
            .await
            .expect("gives up after stop")
            .unwrap();
        assert!(result.is_err());
        assert!(matches!(
            drain(&mut rx).last(),
            Some(LinkEvent::GaveUp { .. })
        ));
    }

    #[test]
    fn wav_length_is_counted_in_ms() {
        let wav = crate::transcribe::encode_wav(&vec![0.0; 16_000 * 3]);
        assert_eq!(wav_duration_ms(&wav), 3000);
    }

    #[test]
    fn usage_round_trips_with_the_final_key() {
        let mut u = MeetingUsage::default();
        u.final_pass.calls = 1;
        let json = serde_json::to_string(&u).unwrap();
        assert!(json.contains("\"final\""));
        assert_eq!(serde_json::from_str::<MeetingUsage>(&json).unwrap(), u);
        // Older or partial JSON still reads.
        assert_eq!(
            serde_json::from_str::<MeetingUsage>("{}").unwrap(),
            MeetingUsage::default()
        );
    }
}
