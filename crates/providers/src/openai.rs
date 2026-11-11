//! Adapter for OpenAI and every service that copies its API: OpenRouter,
//! Groq, Gemini's compatible endpoint, Ollama, LM Studio and self-hosted
//! Whisper servers. Covers chat completions (text and images) and audio
//! transcription.
//!
//! OpenRouter gets its own [`Flavor`] because it differs in a few places: how
//! a key is checked, how models are looked up, the vocabulary field for
//! transcription, and errors returned inside successful responses.

use async_trait::async_trait;
use base64::Engine;
use reqwest::multipart::{Form, Part};
use serde_json::{Value, json};

use crate::error::{ProviderError, Result};
use crate::http::{self, RetryPolicy};
use crate::types::{
    CheckResult, TextRequest, TextResponse, TranscribeRequest, Transcript, TranscriptSegment, Usage,
};
use crate::{SpeechToText, TextGenerator};

pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
/// OpenAI's current transcription model; the older ones (including
/// `whisper-1`) are scheduled to shut down on 2027-02-26.
pub const DEFAULT_TRANSCRIBE_MODEL: &str = "gpt-transcribe";
pub const OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api/v1";
/// OpenRouter shows this name for the app's traffic in the user's activity log.
const OPENROUTER_APP_TITLE: &str = "Clio Notes";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    /// OpenAI itself, or a server that follows its API closely.
    OpenAi,
    OpenRouter,
}

#[derive(Clone)]
pub struct OpenAiClient {
    http: reqwest::Client,
    base_url: String,
    /// `None` for local servers that need no key.
    api_key: Option<String>,
    model: String,
    effort: Option<String>,
    /// Send `max_tokens` instead of `max_completion_tokens`, for compatible
    /// servers that only know the older field.
    legacy_max_tokens: bool,
    flavor: Flavor,
    pub(crate) retry: RetryPolicy,
}

impl OpenAiClient {
    pub fn new(
        http: reqwest::Client,
        base_url: Option<&str>,
        api_key: Option<String>,
        model: impl Into<String>,
        effort: Option<String>,
        legacy_max_tokens: bool,
    ) -> Self {
        Self {
            http,
            base_url: base_url.unwrap_or(DEFAULT_BASE_URL).to_string(),
            api_key,
            model: model.into(),
            effort,
            legacy_max_tokens,
            flavor: Flavor::OpenAi,
            retry: RetryPolicy::default(),
        }
    }

    /// An OpenRouter client. `base_url` overrides the official endpoint.
    pub fn openrouter(
        http: reqwest::Client,
        base_url: Option<&str>,
        api_key: String,
        model: impl Into<String>,
        effort: Option<String>,
    ) -> Self {
        Self {
            base_url: base_url.unwrap_or(OPENROUTER_BASE_URL).to_string(),
            // OpenRouter documents `max_tokens` as the main field.
            legacy_max_tokens: true,
            flavor: Flavor::OpenRouter,
            ..Self::new(http, None, Some(api_key), model, effort, false)
        }
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let mut r = self.http.request(method, http::join(&self.base_url, path));
        if let Some(key) = &self.api_key {
            r = r.bearer_auth(key);
        }
        if self.flavor == Flavor::OpenRouter {
            r = r.header("X-OpenRouter-Title", OPENROUTER_APP_TITLE);
        }
        r
    }

    pub(crate) fn chat_body(&self, req: &TextRequest) -> Value {
        let mut messages = Vec::new();
        if let Some(system) = &req.system {
            messages.push(json!({ "role": "system", "content": system }));
        }
        let user = if req.images.is_empty() {
            json!(req.prompt)
        } else {
            let mut parts: Vec<Value> = req
                .images
                .iter()
                .map(|img| {
                    let b64 = base64::engine::general_purpose::STANDARD.encode(&img.data);
                    json!({
                        "type": "image_url",
                        "image_url": { "url": format!("data:{};base64,{b64}", img.media_type) }
                    })
                })
                .collect();
            parts.push(json!({ "type": "text", "text": req.prompt }));
            Value::Array(parts)
        };
        messages.push(json!({ "role": "user", "content": user }));

        let token_field = if self.legacy_max_tokens {
            "max_tokens"
        } else {
            "max_completion_tokens"
        };
        let mut body = json!({ "model": self.model, "messages": messages });
        body[token_field] = json!(req.max_tokens);
        if let Some(effort) = &self.effort {
            body["reasoning_effort"] = json!(effort);
        }
        if let Some(schema) = &req.json_schema {
            body["response_format"] = json!({
                "type": "json_schema",
                "json_schema": { "name": schema.name, "schema": schema.schema, "strict": true }
            });
        }
        body
    }

    fn transcription_form(&self, req: &TranscribeRequest) -> Result<Form> {
        let file = Part::bytes(req.audio.data.clone())
            .file_name(req.audio.file_name.clone())
            .mime_str(&req.audio.mime)
            .map_err(|e| ProviderError::config(format!("bad audio MIME type: {e}")))?;
        let mut form = Form::new()
            .part("file", file)
            .text("model", self.model.clone());

        if wants_segments(&self.model) {
            form = form
                .text("response_format", "verbose_json")
                .text("timestamp_granularities[]", "segment");
        } else {
            form = form.text("response_format", "json");
        }
        if let Some(lang) = &req.language {
            form = form.text("language", lang.clone());
        }
        // OpenRouter takes vocabulary as `keyterms[]` and ignores `prompt`.
        // OpenAI's gpt-transcribe takes `keywords[]`. Elsewhere the terms go
        // into the prompt, which every Whisper-style endpoint accepts.
        let mut prompt = req.prompt.clone();
        if self.flavor == Flavor::OpenRouter {
            for k in &req.keywords {
                form = form.text("keyterms[]", k.clone());
            }
        } else if supports_keywords(&self.model) {
            for k in &req.keywords {
                form = form.text("keywords[]", k.clone());
            }
        } else if !req.keywords.is_empty() {
            let vocab = format!("Vocabulary: {}.", req.keywords.join(", "));
            prompt = Some(match prompt {
                Some(p) => format!("{vocab} {p}"),
                None => vocab,
            });
        }
        if let Some(p) = prompt.filter(|_| self.flavor != Flavor::OpenRouter) {
            form = form.text("prompt", p);
        }
        Ok(form)
    }

    pub async fn check(&self) -> Result<CheckResult> {
        if self.flavor == Flavor::OpenRouter {
            return self.check_openrouter().await;
        }
        let path = format!("models/{}", self.model);
        let v = http::send_json(self.retry, &self.model, || {
            self.request(reqwest::Method::GET, &path)
        })
        .await?;
        Ok(CheckResult {
            model: v["id"].as_str().unwrap_or(&self.model).to_string(),
            display_name: None,
        })
    }

    /// OpenRouter lists models without a key, so the key is checked with
    /// `GET /key`, then the model with `GET /models/{author}/{slug}/endpoints`.
    async fn check_openrouter(&self) -> Result<CheckResult> {
        http::send_json(self.retry, &self.model, || {
            self.request(reqwest::Method::GET, "key")
        })
        .await?;
        let path = format!("models/{}/endpoints", self.model);
        let v = http::send_json(self.retry, &self.model, || {
            self.request(reqwest::Method::GET, &path)
        })
        .await?;
        Ok(CheckResult {
            model: v["data"]["id"].as_str().unwrap_or(&self.model).to_string(),
            display_name: v["data"]["name"].as_str().map(str::to_string),
        })
    }

    pub async fn list_models(&self) -> Result<Vec<String>> {
        let v = http::send_json(self.retry, &self.model, || {
            self.request(reqwest::Method::GET, "models")
        })
        .await?;
        let mut ids = crate::anthropic::model_ids(&v)?;
        if self.flavor == Flavor::OpenRouter {
            // Transcription models are listed separately. Missing them only
            // makes the list shorter, so a failure here is not an error.
            let speech = http::send_json(self.retry, &self.model, || {
                self.request(
                    reqwest::Method::GET,
                    "models?output_modalities=transcription",
                )
            })
            .await;
            if let Ok(more) = speech.and_then(|v| crate::anthropic::model_ids(&v)) {
                ids.extend(more);
                ids.sort();
                ids.dedup();
            }
        }
        Ok(ids)
    }
}

fn wants_segments(model: &str) -> bool {
    // Whisper models return per-segment timestamps; the newer gpt-* models
    // accept only plain `json`.
    model.contains("whisper")
}

fn supports_keywords(model: &str) -> bool {
    model == "gpt-transcribe" || model.starts_with("gpt-transcribe-")
}

#[async_trait]
impl TextGenerator for OpenAiClient {
    async fn generate(&self, req: &TextRequest) -> Result<TextResponse> {
        let body = self.chat_body(req);
        let v = http::send_json(self.retry, &self.model, || {
            self.request(reqwest::Method::POST, "chat/completions")
                .json(&body)
        })
        .await?;
        parse_chat(&v, &self.model)
    }
}

#[async_trait]
impl SpeechToText for OpenAiClient {
    async fn transcribe(&self, req: &TranscribeRequest) -> Result<Transcript> {
        // Build once up front so a bad request fails before any network call.
        self.transcription_form(req)?;
        let v = http::send_json(self.retry, &self.model, || {
            let form = self
                .transcription_form(req)
                .expect("form was validated above");
            self.request(reqwest::Method::POST, "audio/transcriptions")
                .multipart(form)
        })
        .await?;
        parse_transcript(&v)
    }
}

pub(crate) fn parse_chat(v: &Value, requested_model: &str) -> Result<TextResponse> {
    let choice = &v["choices"][0];
    if choice.is_null() {
        return Err(ProviderError::invalid_response("no choices in response"));
    }
    // OpenRouter can report an upstream failure inside an HTTP 200 response.
    if let Some(code) = choice["error"]["code"].as_u64() {
        let body = json!({ "error": choice["error"] }).to_string();
        return Err(http::classify(
            code as u16,
            &reqwest::header::HeaderMap::new(),
            &body,
            requested_model,
        ));
    }
    let message = &choice["message"];
    if let Some(refusal) = message["refusal"].as_str() {
        return Err(ProviderError::Refused {
            detail: refusal.to_string(),
        });
    }
    match choice["finish_reason"].as_str() {
        Some("length") => return Err(ProviderError::Truncated),
        Some("error") => {
            return Err(ProviderError::Unavailable {
                status: 502,
                detail: "The upstream provider failed while generating.".into(),
            });
        }
        Some("content_filter") => {
            return Err(ProviderError::Refused {
                detail: "The provider's content filter blocked the response.".into(),
            });
        }
        _ => {}
    }
    Ok(TextResponse {
        text: message["content"].as_str().unwrap_or_default().to_string(),
        model: v["model"].as_str().unwrap_or(requested_model).to_string(),
        usage: Usage {
            input_tokens: v["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
            output_tokens: v["usage"]["completion_tokens"].as_u64().unwrap_or(0),
        },
    })
}

pub(crate) fn parse_transcript(v: &Value) -> Result<Transcript> {
    let text = v["text"]
        .as_str()
        .ok_or_else(|| ProviderError::invalid_response("no text in transcription"))?
        .trim()
        .to_string();
    let to_ms = |s: &Value| (s.as_f64().unwrap_or(0.0) * 1000.0).round() as u64;
    let segments = v["segments"]
        .as_array()
        .map(|segs| {
            segs.iter()
                .map(|s| TranscriptSegment {
                    start_ms: to_ms(&s["start"]),
                    end_ms: to_ms(&s["end"]),
                    text: s["text"].as_str().unwrap_or_default().trim().to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Transcript { text, segments })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AudioInput, ImageInput};
    use mockito::Matcher;
    use std::time::Duration;

    fn client(server: &mockito::Server, model: &str) -> OpenAiClient {
        let mut c = OpenAiClient::new(
            http::client(),
            Some(&server.url()),
            Some("sk-test".into()),
            model,
            None,
            false,
        );
        c.retry.base_delay = Duration::ZERO;
        c
    }

    #[test]
    fn chat_body_uses_data_url_images_and_strict_schema() {
        let c = OpenAiClient::new(http::client(), None, None, "m", Some("low".into()), false);
        let b = c.chat_body(
            &TextRequest::new("What is shown?")
                .system("sys")
                .image(ImageInput::png(vec![1, 2, 3]))
                .max_tokens(500)
                .json_schema("slide", json!({"type": "object"})),
        );
        assert_eq!(b["messages"][0]["role"], "system");
        let parts = &b["messages"][1]["content"];
        assert_eq!(parts[0]["image_url"]["url"], "data:image/png;base64,AQID");
        assert_eq!(parts[1]["text"], "What is shown?");
        assert_eq!(b["max_completion_tokens"], 500);
        assert_eq!(b["reasoning_effort"], "low");
        assert_eq!(b["response_format"]["json_schema"]["strict"], true);
    }

    #[test]
    fn legacy_servers_get_max_tokens() {
        let c = OpenAiClient::new(http::client(), None, None, "m", None, true);
        let b = c.chat_body(&TextRequest::new("x").max_tokens(10));
        assert_eq!(b["max_tokens"], 10);
        assert!(b.get("max_completion_tokens").is_none());
    }

    #[tokio::test]
    async fn chat_parses_content_and_usage() {
        let mut server = mockito::Server::new_async().await;
        let m = server
            .mock("POST", "/chat/completions")
            .match_header("authorization", "Bearer sk-test")
            .match_body(Matcher::PartialJson(json!({"model": "some-model"})))
            .with_body(
                json!({
                    "model": "some-model-2026",
                    "choices": [{"finish_reason": "stop", "message": {"role": "assistant", "content": "OK", "refusal": null}}],
                    "usage": {"prompt_tokens": 9, "completion_tokens": 1}
                })
                .to_string(),
            )
            .create_async()
            .await;
        let r = client(&server, "some-model")
            .generate(&TextRequest::new("hi"))
            .await
            .unwrap();
        m.assert_async().await;
        assert_eq!(r.text, "OK");
        assert_eq!(r.model, "some-model-2026");
        assert_eq!(r.usage.input_tokens, 9);
    }

    #[tokio::test]
    async fn truncated_chat_is_an_error() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/chat/completions")
            .with_body(r#"{"choices":[{"finish_reason":"length","message":{"content":"par"}}]}"#)
            .create_async()
            .await;
        let e = client(&server, "m")
            .generate(&TextRequest::new("x"))
            .await
            .unwrap_err();
        assert_eq!(e, ProviderError::Truncated);
    }

    #[tokio::test]
    async fn quota_error_is_not_retried() {
        let mut server = mockito::Server::new_async().await;
        let m = server
            .mock("POST", "/chat/completions")
            .with_status(429)
            .with_body(r#"{"error":{"message":"You exceeded your current quota","type":"insufficient_quota","code":"insufficient_quota"}}"#)
            .expect(1)
            .create_async()
            .await;
        let e = client(&server, "m")
            .generate(&TextRequest::new("x"))
            .await
            .unwrap_err();
        m.assert_async().await;
        assert!(matches!(e, ProviderError::NoCredit { .. }));
    }

    #[tokio::test]
    async fn rate_limit_is_retried_with_retry_after() {
        let mut server = mockito::Server::new_async().await;
        let limited = server
            .mock("POST", "/chat/completions")
            .with_status(429)
            .with_header("retry-after", "0")
            .with_body(r#"{"error":{"message":"Rate limit reached","type":"requests","code":"rate_limit_exceeded"}}"#)
            .expect(1)
            .create_async()
            .await;
        let ok = server
            .mock("POST", "/chat/completions")
            .with_body(r#"{"choices":[{"finish_reason":"stop","message":{"content":"done"}}]}"#)
            .expect(1)
            .create_async()
            .await;
        let r = client(&server, "m")
            .generate(&TextRequest::new("x"))
            .await
            .unwrap();
        limited.assert_async().await;
        ok.assert_async().await;
        assert_eq!(r.text, "done");
    }

    #[tokio::test]
    async fn transcription_sends_keywords_to_gpt_transcribe() {
        let mut server = mockito::Server::new_async().await;
        let m = server
            .mock("POST", "/audio/transcriptions")
            .match_header("content-type", Matcher::Regex("multipart/form-data".into()))
            .match_body(Matcher::AllOf(vec![
                Matcher::Regex(r#"name="model"\r\n\r\ngpt-transcribe\r\n"#.into()),
                Matcher::Regex(r#"name="response_format"\r\n\r\njson\r\n"#.into()),
                Matcher::Regex(r#"name="keywords\[\]"\r\n\r\nkubectl\r\n"#.into()),
                Matcher::Regex(r#"filename="audio.wav""#.into()),
            ]))
            .with_body(r#"{"text":" Run kubectl apply. "}"#)
            .create_async()
            .await;
        let mut req = TranscribeRequest::new(AudioInput::wav(b"RIFF....".to_vec()));
        req.keywords = vec!["kubectl".into()];
        let t = client(&server, "gpt-transcribe")
            .transcribe(&req)
            .await
            .unwrap();
        m.assert_async().await;
        assert_eq!(t.text, "Run kubectl apply.");
        assert!(t.segments.is_empty());
    }

    #[tokio::test]
    async fn whisper_gets_segments_and_keywords_in_prompt() {
        let mut server = mockito::Server::new_async().await;
        let m = server
            .mock("POST", "/audio/transcriptions")
            .match_body(Matcher::AllOf(vec![
                Matcher::Regex(r#"name="response_format"\r\n\r\nverbose_json\r\n"#.into()),
                Matcher::Regex(r#"name="prompt"\r\n\r\nVocabulary: Kubernetes, Helm\.\r\n"#.into()),
            ]))
            .with_body(
                json!({
                    "text": "Hello. World.",
                    "segments": [
                        {"start": 0.0, "end": 1.25, "text": " Hello."},
                        {"start": 1.25, "end": 2.5, "text": " World."}
                    ]
                })
                .to_string(),
            )
            .create_async()
            .await;
        let mut req = TranscribeRequest::new(AudioInput::wav(b"RIFF".to_vec()));
        req.keywords = vec!["Kubernetes".into(), "Helm".into()];
        let t = client(&server, "whisper-1").transcribe(&req).await.unwrap();
        m.assert_async().await;
        assert_eq!(
            t.segments[1],
            TranscriptSegment {
                start_ms: 1250,
                end_ms: 2500,
                text: "World.".into()
            }
        );
    }

    #[tokio::test]
    async fn local_server_without_key_sends_no_auth_header() {
        let mut server = mockito::Server::new_async().await;
        let m = server
            .mock("GET", "/models")
            .match_header("authorization", Matcher::Missing)
            .with_body(r#"{"data":[{"id":"llama3"},{"id":"gemma"}]}"#)
            .create_async()
            .await;
        let c = OpenAiClient::new(
            http::client(),
            Some(&server.url()),
            None,
            "llama3",
            None,
            false,
        );
        assert_eq!(c.list_models().await.unwrap(), vec!["gemma", "llama3"]);
        m.assert_async().await;
    }

    // ---- OpenRouter ----

    fn openrouter(server: &mockito::Server, model: &str) -> OpenAiClient {
        let mut c = OpenAiClient::openrouter(
            http::client(),
            Some(&server.url()),
            "sk-or-test".into(),
            model,
            None,
        );
        c.retry.base_delay = Duration::ZERO;
        c
    }

    #[tokio::test]
    async fn openrouter_check_validates_key_then_model() {
        let mut server = mockito::Server::new_async().await;
        let key = server
            .mock("GET", "/key")
            .match_header("authorization", "Bearer sk-or-test")
            .match_header("x-openrouter-title", "Clio Notes")
            .with_body(r#"{"data":{"label":"sk-or-...","usage":0,"limit":null}}"#)
            .expect(1)
            .create_async()
            .await;
        let model = server
            .mock("GET", "/models/openai/whisper-large-v3/endpoints")
            .with_body(r#"{"data":{"id":"openai/whisper-large-v3","name":"OpenAI: Whisper Large V3","endpoints":[]}}"#)
            .expect(1)
            .create_async()
            .await;
        let r = openrouter(&server, "openai/whisper-large-v3")
            .check()
            .await
            .unwrap();
        key.assert_async().await;
        model.assert_async().await;
        assert_eq!(r.display_name.as_deref(), Some("OpenAI: Whisper Large V3"));
    }

    #[tokio::test]
    async fn openrouter_bad_key_stops_before_model_lookup() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/key")
            .with_status(401)
            .with_body(r#"{"error":{"code":401,"message":"No auth credentials found"}}"#)
            .create_async()
            .await;
        let model = server
            .mock("GET", Matcher::Regex("^/models/".into()))
            .expect(0)
            .create_async()
            .await;
        let e = openrouter(&server, "a/b").check().await.unwrap_err();
        model.assert_async().await;
        assert!(matches!(e, ProviderError::InvalidKey { .. }));
    }

    #[tokio::test]
    async fn openrouter_unknown_model_is_reported() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/key")
            .with_body(r#"{"data":{}}"#)
            .create_async()
            .await;
        server
            .mock("GET", "/models/nobody/nothing/endpoints")
            .with_status(404)
            .with_body(r#"{"error":{"code":404,"message":"Model not found"}}"#)
            .create_async()
            .await;
        let e = openrouter(&server, "nobody/nothing")
            .check()
            .await
            .unwrap_err();
        assert!(
            matches!(e, ProviderError::ModelNotFound { ref model, .. } if model == "nobody/nothing")
        );
    }

    #[test]
    fn openrouter_chat_uses_max_tokens() {
        let c = OpenAiClient::openrouter(http::client(), None, "k".into(), "m", None);
        let b = c.chat_body(&TextRequest::new("x").max_tokens(42));
        assert_eq!(b["max_tokens"], 42);
        assert!(b.get("max_completion_tokens").is_none());
    }

    #[tokio::test]
    async fn error_inside_a_200_response_is_an_error() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/chat/completions")
            .with_body(
                json!({
                    "choices": [{
                        "finish_reason": "error",
                        "message": {"content": ""},
                        "error": {"code": 502, "message": "Upstream provider returned an error"}
                    }]
                })
                .to_string(),
            )
            .create_async()
            .await;
        let e = openrouter(&server, "a/b")
            .generate(&TextRequest::new("x"))
            .await
            .unwrap_err();
        assert!(matches!(e, ProviderError::Unavailable { status: 502, .. }));
    }

    #[tokio::test]
    async fn openrouter_transcription_sends_keyterms_and_no_prompt() {
        let mut server = mockito::Server::new_async().await;
        let m = server
            .mock("POST", "/audio/transcriptions")
            .match_body(Matcher::AllOf(vec![
                Matcher::Regex(r#"name="model"\r\n\r\nopenai/whisper-large-v3\r\n"#.into()),
                Matcher::Regex(r#"name="keyterms\[\]"\r\n\r\nKubernetes\r\n"#.into()),
                Matcher::Regex(r#"name="response_format"\r\n\r\nverbose_json\r\n"#.into()),
            ]))
            .with_body_from_request(|req| {
                let body = String::from_utf8_lossy(req.body().unwrap()).into_owned();
                let text = if body.contains(r#"name="prompt""#) {
                    "PROMPT WAS SENT"
                } else {
                    "ok"
                };
                json!({ "text": text }).to_string().into_bytes()
            })
            .create_async()
            .await;
        let mut req = TranscribeRequest::new(AudioInput::wav(b"RIFF".to_vec()));
        req.keywords = vec!["Kubernetes".into()];
        req.prompt = Some("previous sentence".into());
        let t = openrouter(&server, "openai/whisper-large-v3")
            .transcribe(&req)
            .await
            .unwrap();
        m.assert_async().await;
        assert_eq!(t.text, "ok");
    }

    #[tokio::test]
    async fn openrouter_model_list_includes_transcription_models() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/models")
            .match_query(Matcher::Missing)
            .with_body(r#"{"data":[{"id":"anthropic/claude-opus-5-5"},{"id":"openai/gpt-6-sol"}]}"#)
            .create_async()
            .await;
        server
            .mock("GET", "/models")
            .match_query(Matcher::UrlEncoded(
                "output_modalities".into(),
                "transcription".into(),
            ))
            .with_body(r#"{"data":[{"id":"openai/whisper-large-v3"}]}"#)
            .create_async()
            .await;
        let ids = openrouter(&server, "x").list_models().await.unwrap();
        assert_eq!(
            ids,
            vec![
                "anthropic/claude-opus-5-5",
                "openai/gpt-6-sol",
                "openai/whisper-large-v3"
            ]
        );
    }
}
