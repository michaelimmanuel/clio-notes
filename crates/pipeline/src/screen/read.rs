//! Screen reading: a keyframe in, structured screen content out.

use std::io::Cursor;

use image::RgbaImage;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use providers::{ImageInput, TextGenerator, TextRequest, Usage};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// What a keyframe shows. Every field is always present; empty when the
/// screen has none.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScreenReading {
    /// slides, document, spreadsheet, code, chart, web_page, video_call,
    /// desktop or other.
    pub kind: String,
    pub title: String,
    /// The main readable text, in reading order.
    pub text: String,
    pub tables: Vec<Table>,
    pub charts: Vec<Chart>,
    pub code: Vec<CodeBlock>,
    /// Names, product terms, acronyms and jargon, spelled as on screen.
    pub terms: Vec<String>,
    /// One sentence on what is shown.
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Table {
    pub caption: String,
    pub rows: Vec<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Chart {
    pub title: String,
    /// What the chart shows, with the values that can be read off it.
    pub reading: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CodeBlock {
    pub language: String,
    pub code: String,
}

pub const KINDS: [&str; 9] = [
    "slides",
    "document",
    "spreadsheet",
    "code",
    "chart",
    "web_page",
    "video_call",
    "desktop",
    "other",
];

/// The schema in strict-mode form (every property required, no extras).
pub fn schema() -> Value {
    let string = json!({ "type": "string" });
    let object = |props: Value| {
        let required: Vec<&String> = props.as_object().unwrap().keys().collect();
        json!({
            "type": "object",
            "properties": props,
            "required": required,
            "additionalProperties": false,
        })
    };
    let array = |items: Value| json!({ "type": "array", "items": items });
    object(json!({
        "kind": { "type": "string", "enum": KINDS },
        "title": string,
        "text": string,
        "tables": array(object(json!({
            "caption": string,
            "rows": array(array(string.clone())),
        }))),
        "charts": array(object(json!({ "title": string, "reading": string }))),
        "code": array(object(json!({ "language": string, "code": string }))),
        "terms": array(string.clone()),
        "summary": string,
    }))
}

const SYSTEM: &str = "You read screenshots taken during a meeting and report what is on \
screen, for meeting notes. Report only what is visible; never guess or add knowledge. Copy \
text exactly, keeping names, numbers and technical terms as written. Describe the content \
being presented, not the surrounding software: skip taskbars, browser tabs and toolbars, \
meeting controls and participant video tiles unless they are the subject. If the screen is \
mostly a video call, use kind \"video_call\" and list visible participant names as terms.";

const PROMPT: &str = "Read this screen. Fill every field; use \"\" or [] when the screen has \
none. `title`: the slide or document title, or the window's main heading. `text`: the main \
readable text in reading order, with line breaks, without repeating what goes into tables, \
charts or code. `charts`: what each chart shows and the values you can read off it. `terms`: \
names, product names, acronyms and jargon worth spelling right in a transcript. `summary`: \
one sentence on what is shown.";

/// The largest edge sent to the model. Bigger screens are scaled down; text
/// on a 4K screen stays readable at this size.
const MAX_EDGE: u32 = 2048;

/// Scales and encodes a keyframe for the model.
pub fn encode_for_reading(image: &RgbaImage) -> Vec<u8> {
    let (w, h) = image.dimensions();
    let scale = (MAX_EDGE as f32 / w.max(h) as f32).min(1.0);
    let rgb = if scale < 1.0 {
        let (nw, nh) = (
            (w as f32 * scale).round() as u32,
            (h as f32 * scale).round() as u32,
        );
        image::DynamicImage::ImageRgba8(image::imageops::resize(
            image,
            nw,
            nh,
            FilterType::CatmullRom,
        ))
        .into_rgb8()
    } else {
        image::DynamicImage::ImageRgba8(image.clone()).into_rgb8()
    };
    let mut out = Cursor::new(Vec::new());
    JpegEncoder::new_with_quality(&mut out, 90)
        .encode_image(&rgb)
        .expect("JPEG encoding to memory cannot fail");
    out.into_inner()
}

#[derive(Debug, Clone)]
pub struct ReadResult {
    pub reading: ScreenReading,
    pub model: String,
    pub usage: Usage,
}

/// Reads one screen. A reply that does not match the schema is retried once.
pub async fn read_screen(
    generator: &dyn TextGenerator,
    jpeg: Vec<u8>,
) -> Result<ReadResult, String> {
    let request = TextRequest::new(PROMPT)
        .system(SYSTEM)
        .image(ImageInput {
            media_type: "image/jpeg".into(),
            data: jpeg,
        })
        .max_tokens(6000)
        .json_schema("screen_reading", schema());
    let mut usage = Usage::default();
    let mut last_error = String::new();
    for _ in 0..2 {
        let response = generator
            .generate(&request)
            .await
            .map_err(|e| e.to_string())?;
        usage.input_tokens += response.usage.input_tokens;
        usage.output_tokens += response.usage.output_tokens;
        match parse(&response.text) {
            Ok(reading) => {
                return Ok(ReadResult {
                    reading,
                    model: response.model,
                    usage,
                });
            }
            Err(e) => last_error = e,
        }
    }
    Err(format!(
        "The screen-reading model did not return the expected format: {last_error}"
    ))
}

/// Parses and tidies a reply. Tolerates a Markdown code fence around the JSON.
pub fn parse(text: &str) -> Result<ScreenReading, String> {
    let body = text.trim();
    let body = body
        .strip_prefix("```json")
        .or_else(|| body.strip_prefix("```"))
        .and_then(|b| b.strip_suffix("```"))
        .unwrap_or(body)
        .trim();
    let mut r: ScreenReading = serde_json::from_str(body).map_err(|e| e.to_string())?;
    if !KINDS.contains(&r.kind.as_str()) {
        r.kind = "other".into();
    }
    for s in [&mut r.title, &mut r.text, &mut r.summary] {
        *s = s.trim().to_string();
    }
    r.terms = r
        .terms
        .into_iter()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();
    r.terms.dedup();
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPLY: &str = r#"{"kind":"slides","title":" Q3 roadmap ","text":"Ship v2","tables":[],
        "charts":[{"title":"Revenue","reading":"up 12%"}],"code":[],"terms":["Kubernetes"," ","Kubernetes"],
        "summary":"A roadmap slide."}"#;

    #[test]
    fn parses_and_tidies() {
        let r = parse(REPLY).unwrap();
        assert_eq!(r.title, "Q3 roadmap");
        assert_eq!(r.terms, vec!["Kubernetes"]);
        assert_eq!(r.charts[0].reading, "up 12%");
        let fenced = format!("```json\n{REPLY}\n```");
        assert_eq!(parse(&fenced).unwrap(), r);
    }

    #[test]
    fn unknown_kind_becomes_other_and_missing_fields_fail() {
        let r = parse(&REPLY.replace("\"slides\"", "\"poster\"")).unwrap();
        assert_eq!(r.kind, "other");
        assert!(parse(r#"{"kind":"slides"}"#).is_err());
    }

    #[test]
    fn schema_is_strict() {
        fn check(v: &Value) {
            if v["type"] == "object" {
                let props: Vec<_> = v["properties"].as_object().unwrap().keys().collect();
                let required: Vec<_> = v["required"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|r| r.as_str().unwrap())
                    .collect();
                assert_eq!(props.len(), required.len());
                assert_eq!(v["additionalProperties"], false);
                v["properties"]
                    .as_object()
                    .unwrap()
                    .values()
                    .for_each(check);
            }
            if v["type"] == "array" {
                check(&v["items"]);
            }
        }
        check(&schema());
    }

    #[test]
    fn large_screens_are_scaled_down() {
        let img = RgbaImage::from_pixel(3840, 2160, image::Rgba([255, 255, 255, 255]));
        let jpeg = encode_for_reading(&img);
        let decoded = image::load_from_memory(&jpeg).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (2048, 1152));
    }
}
