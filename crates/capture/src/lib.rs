//! Microphone, system-audio loopback and screen capture, all stamped by one shared monotonic clock.

pub mod audio;
pub mod clock;
pub mod screen;

pub use audio::{AudioRecorder, AudioSource, AudioStats, AudioTap};
pub use clock::SessionClock;
pub use screen::{ScreenRecorder, ScreenStats, ScreenTap, ScreenTarget};
