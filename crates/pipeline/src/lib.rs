//! Live pipeline: VAD, frame differ, segmenter, corrector, section writer and final pass.
//!
//! M2 covers the audio side: [`resample`] to 16 kHz, [`segment`] into
//! utterances with voice-activity detection, and [`transcribe`] each one.

pub mod resample;
pub mod segment;
pub mod transcribe;

pub use segment::{SegmentConfig, Utterance};
pub use transcribe::{PipelineEvent, StreamOptions, transcribe_stream};
