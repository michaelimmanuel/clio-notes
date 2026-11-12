//! Live pipeline: VAD, frame differ, segmenter, corrector, section writer and final pass.
//!
//! - Audio (M2): [`resample`] to 16 kHz, [`segment`] into utterances with
//!   voice-activity detection, and [`transcribe`] each one.
//! - Notes (M4): split the meeting into sections, fix misheard terms from
//!   the screen and write each section ([`notes`]).
//! - Screen (M3): pick keyframes from captured frames and read them with a
//!   vision model ([`screen`]).

pub mod notes;
pub mod resample;
pub mod screen;
pub mod segment;
pub mod transcribe;

pub use notes::{NotesEvent, NotesInput, Section, SectionConfig, notes_stream};
pub use screen::{Frame, ScreenEvent, ScreenMode, ScreenOptions, ScreenReading, screen_stream};
pub use segment::{SegmentConfig, Utterance};
pub use transcribe::{PipelineEvent, StreamOptions, transcribe_stream};
