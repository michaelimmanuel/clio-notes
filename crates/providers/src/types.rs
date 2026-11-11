use serde::{Deserialize, Serialize};

/// The three jobs the app hands to AI providers. Each is assigned to one
/// provider profile in settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Audio to text.
    SpeechToText,
    /// Screenshot to structured content. Uses [`crate::TextGenerator`] with images.
    Vision,
    /// Correcting terms, writing note sections and the final summary.
    Text,
}

impl Role {
    pub const ALL: [Role; 3] = [Role::SpeechToText, Role::Vision, Role::Text];

    pub fn label(self) -> &'static str {
        match self {
            Role::SpeechToText => "Speech to text",
            Role::Vision => "Screen reading",
            Role::Text => "Text generation",
        }
    }
}

impl std::str::FromStr for Role {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().replace('-', "_").as_str() {
            "speech" | "speech_to_text" | "stt" => Ok(Role::SpeechToText),
            "vision" | "screen" => Ok(Role::Vision),
            "text" => Ok(Role::Text),
            other => Err(format!(
                "unknown role \"{other}\" (use speech, vision or text)"
            )),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ImageInput {
    /// `image/png` or `image/jpeg`.
    pub media_type: String,
    pub data: Vec<u8>,
}

impl ImageInput {
    pub fn png(data: Vec<u8>) -> Self {
        Self {
            media_type: "image/png".into(),
            data,
        }
    }
}

/// A JSON Schema the response must follow. Providers that support structured
/// output enforce it; callers must still validate the result.
///
/// For OpenAI-compatible providers the schema should be strict-mode compatible:
/// every object lists all its properties in `required` and sets
/// `additionalProperties: false`.
#[derive(Debug, Clone)]
pub struct JsonSchema {
    pub name: String,
    pub schema: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct TextRequest {
    pub system: Option<String>,
    pub prompt: String,
    /// Placed before the prompt text.
    pub images: Vec<ImageInput>,
    pub max_tokens: u32,
    pub json_schema: Option<JsonSchema>,
}

impl TextRequest {
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            system: None,
            prompt: prompt.into(),
            images: Vec::new(),
            max_tokens: 16_000,
            json_schema: None,
        }
    }

    pub fn system(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
    }

    pub fn image(mut self, image: ImageInput) -> Self {
        self.images.push(image);
        self
    }

    pub fn max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    pub fn json_schema(mut self, name: impl Into<String>, schema: serde_json::Value) -> Self {
        self.json_schema = Some(JsonSchema {
            name: name.into(),
            schema,
        });
        self
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TextResponse {
    pub text: String,
    /// The model that actually answered (may differ from the requested one,
    /// for example after a provider-side fallback).
    pub model: String,
    pub usage: Usage,
}

#[derive(Debug, Clone)]
pub struct AudioInput {
    pub data: Vec<u8>,
    /// With extension; providers use it to detect the format.
    pub file_name: String,
    pub mime: String,
}

impl AudioInput {
    pub fn wav(data: Vec<u8>) -> Self {
        Self {
            data,
            file_name: "audio.wav".into(),
            mime: "audio/wav".into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TranscribeRequest {
    pub audio: AudioInput,
    /// ISO-639-1 code such as `en`.
    pub language: Option<String>,
    /// Terms likely to be spoken, such as names and jargon read from the screen.
    pub keywords: Vec<String>,
    /// Free-text context, such as the end of the previous clip's transcript.
    pub prompt: Option<String>,
}

impl TranscribeRequest {
    pub fn new(audio: AudioInput) -> Self {
        Self {
            audio,
            language: None,
            keywords: Vec::new(),
            prompt: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TranscriptSegment {
    /// Relative to the start of the clip.
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Transcript {
    pub text: String,
    /// Empty when the provider returns no timestamps. The pipeline sends short
    /// clips cut at pauses, so the clip's own start time is precise enough.
    pub segments: Vec<TranscriptSegment>,
}

/// Result of a connection check.
#[derive(Debug, Clone, Serialize)]
pub struct CheckResult {
    pub model: String,
    pub display_name: Option<String>,
}
