use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// One monotonic clock shared by every capture stream in a session.
///
/// Every event is stamped as an offset from the same `Instant`, so audio and
/// screen timestamps can be compared directly. The wall-clock start time is kept
/// only for display and file naming; it is never used for alignment.
#[derive(Debug, Clone, Copy)]
pub struct SessionClock {
    start: Instant,
    started_at: SystemTime,
}

impl SessionClock {
    pub fn start() -> Self {
        Self {
            start: Instant::now(),
            started_at: SystemTime::now(),
        }
    }

    /// Time since the session started.
    pub fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }

    /// Offset of `instant` from the session start (zero if it is earlier).
    pub fn offset_of(&self, instant: Instant) -> Duration {
        instant.saturating_duration_since(self.start)
    }

    /// Wall-clock start time as milliseconds since the Unix epoch.
    pub fn started_at_unix_ms(&self) -> u128 {
        self.started_at
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    }
}
