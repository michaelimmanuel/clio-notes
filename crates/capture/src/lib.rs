//! Microphone, system-audio loopback and screen capture, all stamped by one shared monotonic clock.

pub mod audio;
pub mod clock;
pub mod screen;

pub use audio::{AudioRecorder, AudioSource, AudioStats};
pub use clock::SessionClock;
pub use screen::{ScreenRecorder, ScreenStats, ScreenTarget};
