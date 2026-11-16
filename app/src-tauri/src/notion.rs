//! Exporting final notes to Notion.
//!
//! The user creates an internal integration in Notion, pastes its secret in
//! Settings, and shares a page with it. Each export creates a new child page
//! under that page: title, summary, decisions, action items as to-dos, and
//! one section per topic with its screenshot.
//!
//! The secret lives in the system key store, like provider keys. The chosen
//! parent page is kept in `notion.json` next to `providers.json`.
//! Screenshots are scaled down to JPEG and sent with Notion's file upload API;
//! a screenshot that fails to upload is left out instead of failing the export.

use std::path::{Path, PathBuf};
use std::time::Duration;

use pipeline::final_pass::{self, FinalNotes, MeetingInput};
use pipeline::language::Words;
use pipeline::notes::{Source, clock};
use providers::KeyStore;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use storage::Timeline;
use tauri::State;

use crate::export::final_notes;
use crate::meetings::meeting_dir;
use crate::settings::{AppState, UiError};

const API: &str = "https://api.notion.com/v1";
const VERSION: &str = "2026-03-11";
/// The key store entry for the integration secret.
const KEY_ID: &str = "_notion";
/// Notion limits: characters per text object, blocks per request.
const MAX_TEXT: usize = 2000;
const MAX_CHILDREN: usize = 100;
/// Screenshots are sent as JPEG at most this wide, well under upload limits.
const IMAGE_WIDTH: u32 = 1920;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NotionConfig {
    pub workspace: Option<String>,
    pub parent_id: Option<String>,
    pub parent_title: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct NotionStatus {
    connected: bool,
    workspace: Option<String>,
    parent_id: Option<String>,
    parent_title: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct NotionPage {
    id: String,
    title: String,
    url: String,
}

#[derive(Debug, Serialize)]
pub struct ExportResult {
    url: String,
    /// Screenshots that could not be uploaded and were left out.
    skipped_images: usize,
}

fn config_path(state: &AppState) -> PathBuf {
    state.store.path().with_file_name("notion.json")
}

fn load_config(state: &AppState) -> NotionConfig {
    std::fs::read_to_string(config_path(state))
        .ok()
        .and_then(|t| serde_json::from_str(t.trim_start_matches('\u{feff}')).ok())
        .unwrap_or_default()
}

fn save_config(state: &AppState, config: &NotionConfig) -> Result<(), UiError> {
    let path = config_path(state);
    let text =
        serde_json::to_string_pretty(config).map_err(|e| UiError::new("config", e.to_string()))?;
    std::fs::write(&path, text)
        .map_err(|e| UiError::new("config", format!("Cannot write {}: {e}", path.display())))
}

fn token(state: &AppState) -> Result<String, UiError> {
    state.keys.get(KEY_ID)?.ok_or_else(|| {
        UiError::new(
            "notion",
            "Notion is not connected. Add your integration secret in Settings > Notion.",
        )
    })
}

/// A Notion API error as a sentence the user can act on.
fn api_error(status: reqwest::StatusCode, body: &str) -> UiError {
    let parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let code = parsed["code"].as_str().unwrap_or("");
    let message = parsed["message"].as_str().unwrap_or(body);
    let text = match (status.as_u16(), code) {
        (401, _) | (_, "unauthorized") => {
            "Notion did not accept the integration secret. Copy it again from \
             notion.so/profile/integrations and reconnect."
                .to_string()
        }
        (404, _) | (_, "object_not_found") => {
            "Clio Notes cannot see that Notion page. In Notion, open the page, click ••• > \
             Connections and add your integration, then try again."
                .to_string()
        }
        (403, _) | (_, "restricted_resource") => {
            "The Notion integration is missing a capability. In its settings, allow \
             Read content, Insert content and Update content."
                .to_string()
        }
        (429, _) => "Notion is limiting requests right now. Try again in a minute.".to_string(),
        _ => format!("Notion returned an error ({status}): {message}"),
    };
    UiError::new("notion", text)
}

struct Client<'a> {
    http: &'a reqwest::Client,
    token: String,
}

impl Client<'_> {
    /// Sends a request built by `build`, waiting and retrying when Notion
    /// says it is busy (429, 502–504, 529).
    async fn send(&self, build: impl Fn() -> reqwest::RequestBuilder) -> Result<Value, UiError> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let response = build()
                .bearer_auth(&self.token)
                .header("Notion-Version", VERSION)
                .timeout(Duration::from_secs(60))
                .send()
                .await
                .map_err(|e| UiError::new("network", format!("Cannot reach Notion: {e}")))?;
            let status = response.status();
            let busy = matches!(status.as_u16(), 429 | 502 | 503 | 504 | 529);
            if busy && attempt < 4 {
                let wait = response
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(2 * attempt)
                    .min(30);
                tokio::time::sleep(Duration::from_secs(wait)).await;
                continue;
            }
            let body = response
                .text()
                .await
                .map_err(|e| UiError::new("network", format!("Cannot read Notion's reply: {e}")))?;
            if !status.is_success() {
                return Err(api_error(status, &body));
            }
            return serde_json::from_str(&body)
                .map_err(|e| UiError::new("notion", format!("Unexpected reply from Notion: {e}")));
        }
    }

    async fn get(&self, path: &str) -> Result<Value, UiError> {
        let url = format!("{API}{path}");
        self.send(|| self.http.get(&url)).await
    }

    async fn post(&self, path: &str, body: &Value) -> Result<Value, UiError> {
        let url = format!("{API}{path}");
        self.send(|| self.http.post(&url).json(body)).await
    }

    async fn patch(&self, path: &str, body: &Value) -> Result<Value, UiError> {
        let url = format!("{API}{path}");
        self.send(|| self.http.patch(&url).json(body)).await
    }

    /// Uploads one image and returns its file upload id.
    async fn upload_image(&self, name: &str, bytes: Vec<u8>) -> Result<String, UiError> {
        let created = self
            .post(
                "/file_uploads",
                &json!({ "filename": name, "content_type": "image/jpeg" }),
            )
            .await?;
        let id = created["id"]
            .as_str()
            .ok_or_else(|| UiError::new("notion", "Notion did not return an upload id."))?
            .to_string();
        let url = format!("{API}/file_uploads/{id}/send");
        self.send(|| {
            let part = reqwest::multipart::Part::bytes(bytes.clone())
                .file_name(name.to_string())
                .mime_str("image/jpeg")
                .expect("image/jpeg is a valid MIME type");
            self.http
                .post(&url)
                .multipart(reqwest::multipart::Form::new().part("file", part))
        })
        .await?;
        Ok(id)
    }
}

fn client<'a>(state: &'a AppState) -> Result<Client<'a>, UiError> {
    Ok(Client {
        http: &state.http,
        token: token(state)?,
    })
}

// ---- Blocks ----

/// Rich text for `text`, split into pieces Notion accepts.
fn rich(text: &str, italic: bool, color: &str) -> Vec<Value> {
    let chars: Vec<char> = text.chars().collect();
    chars
        .chunks(MAX_TEXT)
        .map(|c| {
            json!({
                "type": "text",
                "text": { "content": c.iter().collect::<String>() },
                "annotations": { "italic": italic, "color": color },
            })
        })
        .collect()
}

fn with_source(text: &str, source: Source, words: &Words) -> Vec<Value> {
    let mut out = rich(text, false, "default");
    let mark = match source {
        Source::Spoken => None,
        Source::Screen => Some(words.on_screen),
        Source::Both => Some(words.said_and_shown),
    };
    if let Some(mark) = mark {
        out.extend(rich(&format!(" ({mark})"), true, "gray"));
    }
    out
}

fn block(kind: &str, rich_text: Vec<Value>) -> Value {
    json!({ "object": "block", "type": kind, kind: { "rich_text": rich_text } })
}

fn paragraph(text: &str) -> Value {
    block("paragraph", rich(text, false, "default"))
}

fn note(text: &str) -> Value {
    block("paragraph", rich(text, true, "gray"))
}

fn heading(text: &str) -> Value {
    block("heading_2", rich(text, false, "default"))
}

fn image(upload_id: &str, caption: &str) -> Value {
    json!({
        "object": "block",
        "type": "image",
        "image": {
            "type": "file_upload",
            "file_upload": { "id": upload_id },
            "caption": rich(caption, false, "default"),
        },
    })
}

/// The page body. `uploads` maps an image file (relative to the meeting
/// folder) to its Notion upload id; images missing from it are left out.
fn blocks(notes: &FinalNotes, input: &MeetingInput, uploads: &[(String, String)]) -> Vec<Value> {
    let words = input.notes_language.words();
    let with_source = |text: &str, source: Source| with_source(text, source, words);
    let mut out = vec![note(&final_pass::header_line(input))];
    if let Some(n) = &notes.note {
        out.push(block("quote", rich(n, false, "default")));
    }
    for part in notes
        .summary
        .split("\n\n")
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        out.push(paragraph(part));
    }
    if !notes.decisions.is_empty() {
        out.push(heading(words.decisions));
        for d in &notes.decisions {
            out.push(block("bulleted_list_item", with_source(&d.text, d.source)));
        }
    }
    if !notes.actions.is_empty() {
        out.push(heading(words.action_items));
        for a in &notes.actions {
            let text = if a.owner.is_empty() {
                a.text.clone()
            } else {
                format!("{} — {}", a.text, a.owner)
            };
            let mut todo = block("to_do", with_source(&text, a.source));
            todo["to_do"]["checked"] = json!(false);
            out.push(todo);
        }
    }
    for t in &notes.topics {
        out.push(heading(&t.heading));
        out.push(note(&format!("{}–{}", clock(t.start_ms), clock(t.end_ms))));
        if let Some(img) = &t.image
            && let Some((_, id)) = uploads.iter().find(|(file, _)| *file == img.file)
        {
            out.push(image(
                id,
                &format!("{} {}", words.screen_at, clock(img.t_ms)),
            ));
        }
        for b in &t.bullets {
            out.push(block("bulleted_list_item", with_source(&b.text, b.source)));
        }
    }
    out.push(json!({ "object": "block", "type": "divider", "divider": {} }));
    out.push(note(words.footer));
    out
}

fn page_title(page: &Value) -> String {
    let props = page["properties"].as_object();
    let title = props
        .and_then(|p| p.values().find(|v| v["type"] == "title"))
        .and_then(|v| v["title"].as_array())
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p["plain_text"].as_str())
                .collect::<String>()
        })
        .unwrap_or_default();
    if title.trim().is_empty() {
        "Untitled".into()
    } else {
        title
    }
}

// ---- Commands ----

#[tauri::command]
pub fn notion_status(state: State<'_, AppState>) -> NotionStatus {
    let config = load_config(&state);
    NotionStatus {
        connected: matches!(state.keys.get(KEY_ID), Ok(Some(_))),
        workspace: config.workspace,
        parent_id: config.parent_id,
        parent_title: config.parent_title,
    }
}

/// Checks the integration secret with Notion and stores it.
#[tauri::command]
pub async fn notion_connect(
    state: State<'_, AppState>,
    secret: String,
) -> Result<NotionStatus, UiError> {
    let secret = secret.trim().to_string();
    if secret.is_empty() {
        return Err(UiError::new(
            "notion",
            "Paste the integration secret first.",
        ));
    }
    let client = Client {
        http: &state.http,
        token: secret.clone(),
    };
    let me = client.get("/users/me").await?;
    let workspace = me["bot"]["workspace_name"].as_str().map(str::to_string);
    state.keys.set(KEY_ID, &secret)?;
    let mut config = load_config(&state);
    config.workspace = workspace;
    save_config(&state, &config)?;
    log::info!("notion connected");
    Ok(notion_status(state))
}

#[tauri::command]
pub fn notion_disconnect(state: State<'_, AppState>) -> Result<NotionStatus, UiError> {
    state.keys.delete(KEY_ID)?;
    save_config(&state, &NotionConfig::default())?;
    log::info!("notion disconnected");
    Ok(notion_status(state))
}

/// Pages shared with the integration, best matches for `query` first.
#[tauri::command]
pub async fn notion_pages(
    state: State<'_, AppState>,
    query: Option<String>,
) -> Result<Vec<NotionPage>, UiError> {
    let client = client(&state)?;
    let reply = client
        .post(
            "/search",
            &json!({
                "query": query.unwrap_or_default(),
                "filter": { "property": "object", "value": "page" },
                "page_size": 50,
            }),
        )
        .await?;
    Ok(reply["results"]
        .as_array()
        .map(|results| {
            results
                .iter()
                .filter(|p| p["in_trash"] != true && p["archived"] != true)
                .filter_map(|p| {
                    Some(NotionPage {
                        id: p["id"].as_str()?.to_string(),
                        title: page_title(p),
                        url: p["url"].as_str().unwrap_or_default().to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default())
}

#[tauri::command]
pub fn notion_set_parent(
    state: State<'_, AppState>,
    id: String,
    title: String,
) -> Result<NotionStatus, UiError> {
    let mut config = load_config(&state);
    config.parent_id = Some(id);
    config.parent_title = Some(title);
    save_config(&state, &config)?;
    Ok(notion_status(state))
}

/// Creates a Notion page with the final notes of meeting `id` under the
/// chosen parent page, and remembers its link with the meeting.
#[tauri::command]
pub async fn notion_export(
    state: State<'_, AppState>,
    id: String,
) -> Result<ExportResult, UiError> {
    let dir = meeting_dir(&id)?;
    let client = client(&state)?;
    let parent = load_config(&state).parent_id.ok_or_else(|| {
        UiError::new(
            "notion",
            "Choose the Notion page to export under in Settings > Notion.",
        )
    })?;
    let load_dir = dir.clone();
    let (notes, input, images) = tauri::async_runtime::spawn_blocking(move || {
        let (notes, input) = final_notes(&load_dir)?;
        let files = crate::export::used_images(&notes, &load_dir);
        let images: Vec<(String, Result<Vec<u8>, String>)> = files
            .into_iter()
            .map(|f| {
                let bytes = pipeline::screen::jpeg_of_file(&load_dir.join(&f), IMAGE_WIDTH, 85);
                (f, bytes)
            })
            .collect();
        Ok::<_, UiError>((notes, input, images))
    })
    .await
    .map_err(|e| UiError::new("storage", e.to_string()))??;

    let mut uploads = Vec::new();
    let mut skipped = 0;
    for (file, bytes) in images {
        let name = Path::new(&file)
            .with_extension("jpg")
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "screen.jpg".into());
        let result = match bytes {
            Ok(bytes) => client.upload_image(&name, bytes).await,
            Err(e) => Err(UiError::new("storage", e)),
        };
        match result {
            Ok(upload) => uploads.push((file, upload)),
            Err(e) => {
                log::warn!("notion image upload failed: {}", e.message());
                skipped += 1;
            }
        }
    }

    let mut body = blocks(&notes, &input, &uploads);
    let rest = if body.len() > MAX_CHILDREN {
        body.split_off(MAX_CHILDREN)
    } else {
        Vec::new()
    };
    let page = client
        .post(
            "/pages",
            &json!({
                "parent": { "page_id": parent },
                "icon": { "type": "emoji", "emoji": "📝" },
                "properties": {
                    "title": { "title": rich(&notes.title, false, "default") },
                },
                "children": body,
            }),
        )
        .await?;
    let page_id = page["id"].as_str().unwrap_or_default().to_string();
    for chunk in rest.chunks(MAX_CHILDREN) {
        client
            .patch(
                &format!("/blocks/{page_id}/children"),
                &json!({ "children": chunk }),
            )
            .await?;
    }
    let url = page["url"].as_str().unwrap_or_default().to_string();
    if let Ok(timeline) = Timeline::open(&dir.join("timeline.sqlite")) {
        let _ = timeline.set_meta("notion_url", &url);
    }
    log::info!(
        "notes exported to notion: {} blocks, {} images, {skipped} skipped",
        body.len() + rest.len(),
        uploads.len()
    );
    Ok(ExportResult {
        url,
        skipped_images: skipped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pipeline::final_pass::{ActionItem, Item, Topic, TopicImage};

    fn sample() -> (FinalNotes, MeetingInput) {
        let notes = FinalNotes {
            title: "Launch review".into(),
            summary: "We agreed the date.\n\nMarketing starts next week.".into(),
            decisions: vec![Item {
                text: "Launch on 3 March".into(),
                source: Source::Both,
            }],
            actions: vec![ActionItem {
                text: "Book the venue".into(),
                owner: "Dana".into(),
                source: Source::Spoken,
            }],
            topics: vec![Topic {
                heading: "Timeline".into(),
                start_ms: 0,
                end_ms: 90_000,
                bullets: vec![Item {
                    text: "Beta ends in February".into(),
                    source: Source::Screen,
                }],
                image: Some(TopicImage {
                    t_ms: 30_000,
                    keyframe: "keyframes/30000.png".into(),
                    file: "images/01-timeline.png".into(),
                }),
            }],
            model: None,
            note: None,
        };
        let input = MeetingInput {
            started_at_unix_ms: None,
            duration_ms: 90_000,
            sections: vec![],
            keyframes: vec![],
            transcript: vec![],
            notes_language: Default::default(),
        };
        (notes, input)
    }

    #[test]
    fn blocks_follow_the_notes() {
        let (notes, input) = sample();
        let uploads = vec![("images/01-timeline.png".to_string(), "up-1".to_string())];
        let b = blocks(&notes, &input, &uploads);
        let kinds: Vec<&str> = b.iter().map(|x| x["type"].as_str().unwrap()).collect();
        assert_eq!(
            kinds,
            [
                "paragraph",
                "paragraph",
                "paragraph",
                "heading_2",
                "bulleted_list_item",
                "heading_2",
                "to_do",
                "heading_2",
                "paragraph",
                "image",
                "bulleted_list_item",
                "divider",
                "paragraph",
            ]
        );
        assert_eq!(
            b[6]["to_do"]["rich_text"][0]["text"]["content"],
            "Book the venue — Dana"
        );
        assert_eq!(b[6]["to_do"]["checked"], false);
        assert_eq!(b[9]["image"]["file_upload"]["id"], "up-1");
        assert_eq!(
            b[10]["bulleted_list_item"]["rich_text"][1]["text"]["content"],
            " (on screen)"
        );
    }

    #[test]
    fn missing_upload_leaves_the_image_out() {
        let (notes, input) = sample();
        let b = blocks(&notes, &input, &[]);
        assert!(b.iter().all(|x| x["type"] != "image"));
    }

    #[test]
    fn long_text_is_split() {
        let parts = rich(&"a".repeat(4500), false, "default");
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[2]["text"]["content"].as_str().unwrap().len(), 500);
    }

    #[test]
    fn errors_say_what_to_do() {
        let e = api_error(
            reqwest::StatusCode::NOT_FOUND,
            r#"{"code":"object_not_found","message":"Could not find page"}"#,
        );
        assert!(e.message().contains("Connections"));
    }
}
