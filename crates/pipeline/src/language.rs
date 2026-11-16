//! The language a meeting is held in and the language its notes are written
//! in. Chosen in Settings and stored in each meeting's timeline, so rewriting
//! the notes later uses the same language.

use serde::{Deserialize, Serialize};
use storage::Timeline;

/// Timeline meta key: the meeting's [`SpeechLanguage`].
pub const SPEECH_LANGUAGE: &str = "speech_language";
/// Timeline meta key: the meeting's [`NotesLanguage`].
pub const NOTES_LANGUAGE: &str = "notes_language";

/// What is spoken in the meeting, as passed to speech to text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SpeechLanguage {
    #[default]
    #[serde(rename = "en")]
    English,
    #[serde(rename = "id")]
    Indonesian,
    /// Indonesian and English, switching between them, often within a
    /// sentence.
    #[serde(rename = "id-en")]
    IndonesianEnglish,
}

impl SpeechLanguage {
    /// The ISO-639-1 code sent with each utterance. `None` lets the model
    /// detect the language of each utterance, which keeps English sentences
    /// in a mixed meeting in English instead of translating them.
    pub fn speech_code(self) -> Option<&'static str> {
        match self {
            Self::English => Some("en"),
            Self::Indonesian => Some("id"),
            Self::IndonesianEnglish => None,
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Self::English => "en",
            Self::Indonesian => "id",
            Self::IndonesianEnglish => "id-en",
        }
    }

    pub fn from_code(code: &str) -> Option<Self> {
        serde_json::from_value(serde_json::Value::String(code.into())).ok()
    }

    /// The meeting's language, English for meetings recorded before it was
    /// stored.
    pub fn of(timeline: &Timeline) -> Self {
        meta(timeline, SPEECH_LANGUAGE)
            .and_then(|c| Self::from_code(&c))
            .unwrap_or_default()
    }
}

/// The language the notes are written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum NotesLanguage {
    #[default]
    #[serde(rename = "en")]
    English,
    #[serde(rename = "id")]
    Indonesian,
}

impl NotesLanguage {
    pub fn code(self) -> &'static str {
        match self {
            Self::English => "en",
            Self::Indonesian => "id",
        }
    }

    pub fn from_code(code: &str) -> Option<Self> {
        serde_json::from_value(serde_json::Value::String(code.into())).ok()
    }

    /// The notes language of a meeting, English for meetings recorded before
    /// it was stored.
    pub fn of(timeline: &Timeline) -> Self {
        meta(timeline, NOTES_LANGUAGE)
            .and_then(|c| Self::from_code(&c))
            .unwrap_or_default()
    }

    /// The rule added to the note writers' instructions.
    pub fn rule(self) -> &'static str {
        match self {
            Self::English => {
                "- Language: write the heading, title, summary and every point in English, \
                 whatever language was spoken (the meeting may be in Indonesian or mix \
                 Indonesian and English). Keep names, product names and technical terms \
                 exactly as said or shown. `heard` in corrections stays as in the transcript."
            }
            Self::Indonesian => {
                "- Language: write the heading, title, summary and every point in Indonesian \
                 (Bahasa Indonesia), in a clear, neutral business style, whatever language was \
                 spoken (the meeting may be in English or mix Indonesian and English). Keep \
                 names, product names and technical terms exactly as said or shown, in English \
                 when they were said in English, as Indonesian speakers do. `heard` in \
                 corrections stays as in the transcript."
            }
        }
    }

    /// Fixed words in the exported notes.
    pub fn words(self) -> &'static Words {
        match self {
            Self::English => &ENGLISH,
            Self::Indonesian => &INDONESIAN,
        }
    }
}

fn meta(timeline: &Timeline, key: &str) -> Option<String> {
    timeline.meta(key).ok().flatten().filter(|v| !v.is_empty())
}

/// The fixed words of the exported notes (`notes.md`, `transcript.md`, the
/// Notion page) in one language.
#[derive(Debug)]
pub struct Words {
    pub decisions: &'static str,
    pub action_items: &'static str,
    pub on_screen: &'static str,
    pub said_and_shown: &'static str,
    /// Followed by the time, as the alt text of a screenshot.
    pub screen_at: &'static str,
    pub footer: &'static str,
    pub transcript: &'static str,
    pub live_notes: &'static str,
    /// Before a screen's title in the transcript.
    pub screen_changed: &'static str,
    pub days: [&'static str; 7],
    pub months: [&'static str; 12],
}

static ENGLISH: Words = Words {
    decisions: "Decisions",
    action_items: "Action items",
    on_screen: "on screen",
    said_and_shown: "said and shown",
    screen_at: "Screen at",
    footer: "Written by Clio Notes. Points marked \"on screen\" were shown but not said.",
    transcript: "Transcript",
    live_notes: "live notes",
    screen_changed: "Screen changed",
    days: [
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
        "Sunday",
    ],
    months: [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ],
};

static INDONESIAN: Words = Words {
    decisions: "Keputusan",
    action_items: "Tindak lanjut",
    on_screen: "di layar",
    said_and_shown: "disebut dan ditampilkan",
    screen_at: "Layar pada",
    footer: "Ditulis oleh Clio Notes. Poin bertanda \"di layar\" hanya ditampilkan, tidak \
             diucapkan.",
    transcript: "Transkrip",
    live_notes: "catatan langsung",
    screen_changed: "Layar berganti",
    days: [
        "Senin", "Selasa", "Rabu", "Kamis", "Jumat", "Sabtu", "Minggu",
    ],
    months: [
        "Januari",
        "Februari",
        "Maret",
        "April",
        "Mei",
        "Juni",
        "Juli",
        "Agustus",
        "September",
        "Oktober",
        "November",
        "Desember",
    ],
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_round_trip() {
        for l in [
            SpeechLanguage::English,
            SpeechLanguage::Indonesian,
            SpeechLanguage::IndonesianEnglish,
        ] {
            assert_eq!(SpeechLanguage::from_code(l.code()), Some(l));
        }
        for l in [NotesLanguage::English, NotesLanguage::Indonesian] {
            assert_eq!(NotesLanguage::from_code(l.code()), Some(l));
        }
        assert_eq!(SpeechLanguage::IndonesianEnglish.speech_code(), None);
        assert_eq!(SpeechLanguage::Indonesian.speech_code(), Some("id"));
        assert_eq!(SpeechLanguage::from_code("fr"), None);
    }
}
