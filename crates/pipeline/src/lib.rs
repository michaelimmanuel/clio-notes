//! Live pipeline: VAD, frame differ, segmenter, corrector, section writer and final pass.
//!
//! - Audio (M2): [`resample`] to 16 kHz, [`segment`] into utterances with
//!   voice-activity detection, and [`transcribe`] each one.
//! - Screen (M3): pick keyframes from captured frames and read them with a
//!   vision model ([`screen`]).

pub mod resample;
pub mod screen;
pub mod segment;
pub mod transcribe;

pub use screen::{Frame, ScreenEvent, ScreenMode, ScreenOptions, ScreenReading, screen_stream};
pub use segment::{SegmentConfig, Utterance};
pub use transcribe::{PipelineEvent, StreamOptions, transcribe_stream};
