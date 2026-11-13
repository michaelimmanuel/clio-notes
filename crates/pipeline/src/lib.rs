//! Live pipeline: VAD, frame differ, segmenter, corrector, section writer and final pass.
//!
//! - Audio (M2): [`resample`] to 16 kHz, [`segment`] into utterances with
//!   voice-activity detection, and [`transcribe`] each one.
//! - Notes (M4): split the meeting into sections, fix misheard terms from
//!   the screen and write each section ([`notes`]).
//! - Final notes (M5): merge the live notes into one document and export
//!   it with its images ([`final_pass`]).
//! - Screen (M3): pick keyframes from captured frames and read them with a
//!   vision model ([`screen`]).
//! - Hardening (M6): retry through provider outages and count usage
//!   ([`resilient`]); finish an interrupted meeting ([`recover`]).

pub mod final_pass;
pub mod notes;
pub mod recover;
pub mod resample;
pub mod resilient;
pub mod screen;
pub mod segment;
pub mod transcribe;

pub use final_pass::{FinalNotes, finalize};
pub use notes::{NotesEvent, NotesInput, Section, SectionConfig, notes_stream};
pub use resilient::{Link, LinkEvent, MeetingUsage, Patience, Resilient, RoleUsage};
pub use screen::{Frame, ScreenEvent, ScreenMode, ScreenOptions, ScreenReading, screen_stream};
pub use segment::{SegmentConfig, Utterance};
pub use transcribe::{PipelineEvent, StreamOptions, transcribe_stream};
