//! Anthropic Messages API adapter (raw HTTP; there is no official Rust SDK).

use async_trait::async_trait;
use base64::Engine;
use serde_json::{Value, json};

use crate::error::{ProviderError, Result};
use crate::http::{self, RetryPolicy};
use crate::types::{CheckResult, TextRequest, TextResponse, Usage};
use crate::{OnText, TextGenerator};

pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
pub const DEFAULT_MODEL: &str = "claude-opus-5-5";
/// Under this version `temperature`, `top_p` and `top_k` are rejected, so the
/// adapter never sends them.
const API_VERSION: &str = "2026-10-21";
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";

/// Models that accept `fallbacks: "default"`: when a safety classifier
/// declines a request, the API retries it on a suitable model in the same call.
const FALLBACK_MODELS: &[&str] = &[
    "claude-fable-5-2",
    "claude-fable-5-1",
    "claude-opus-5-5",
    "claude-opus-5",
    "claude-sonnet-5-5",
];

#[derive(Clone)]
pub struct AnthropicClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    effort: Option<String>,
    pub(crate) retry: RetryPolicy,
}

impl AnthropicClient {
    pub fn new(
        http: reqwest::Client,
        base_url: Option<&str>,
        api_key: String,
        model: impl Into<String>,
        effort: Option<String>,
    ) -> Self {
        Self {
            http,
            base_url: base_url.unwrap_or(DEFAULT_BASE_URL).to_string(),
            api_key,
            model: model.into(),
            effort,
            retry: RetryPolicy::default(),
        }
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, http::join(&self.base_url, path))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
    }

    fn uses_fallbacks(&self) -> bool {
        FALLBACK_MODELS.contains(&self.model.as_str())
    }

    pub(crate) fn body(&self, req: &TextRequest) -> Value {
        let mut content: Vec<Value> = req
            .images
            .iter()
            .map(|img| {
                json!({
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": img.media_type,
                        "data": base64::engine::general_purpose::STANDARD.encode(&img.data),
                    }
                })
            })
            .collect();
        content.push(json!({ "type": "text", "text": req.prompt }));

        let mut messages: Vec<Value> = req
            .history
            .iter()
            .map(|t| json!({ "role": t.role.as_str(), "content": t.text }))
            .collect();
        messages.push(json!({ "role": "user", "content": content }));

        let mut body = json!({
            "model": self.model,
            "max_tokens": req.max_tokens,
            "messages": messages,
        });
        if let Some(system) = &req.system {
            body["system"] = if req.cache_system {
                json!([{
                    "type": "text",
                    "text": system,
                    "cache_control": { "type": "ephemeral" },
                }])
            } else {
                json!(system)
            };
        }
        let mut output_config = serde_json::Map::new();
        if let Some(effort) = &self.effort {
            output_config.insert("effort".into(), json!(effort));
        }
        if let Some(schema) = &req.json_schema {
            output_config.insert(
                "format".into(),
                json!({ "type": "json_schema", "schema": schema.schema }),
            );
        }
        if !output_config.is_empty() {
            body["output_config"] = Value::Object(output_config);
        }
        if self.uses_fallbacks() {
            body["fallbacks"] = json!("default");
        }
        body
    }

    /// Checks the key and model with the free model-lookup endpoint.
    pub async fn check(&self) -> Result<CheckResult> {
        let path = format!("v1/models/{}", self.model);
        let v = http::send_json(self.retry, &self.model, || {
            self.request(reqwest::Method::GET, &path)
        })
        .await?;
        Ok(CheckResult {
            model: v["id"].as_str().unwrap_or(&self.model).to_string(),
            display_name: v["display_name"].as_str().map(str::to_string),
        })
    }

    pub async fn list_models(&self) -> Result<Vec<String>> {
        let v = http::send_json(self.retry, &self.model, || {
            self.request(reqwest::Method::GET, "v1/models?limit=1000")
        })
        .await?;
        model_ids(&v)
    }
}

#[async_trait]
impl TextGenerator for AnthropicClient {
    async fn generate(&self, req: &TextRequest) -> Result<TextResponse> {
        let body = self.body(req);
        let v = http::send_json(self.retry, &self.model, || self.post_messages(&body)).await?;
        parse_message(&v, &self.model)
    }

    async fn generate_streaming(
        &self,
        req: &TextRequest,
        on_text: OnText<'_>,
    ) -> Result<TextResponse> {
        let mut body = self.body(req);
        body["stream"] = json!(true);
        let mut stream = StreamState::default();
        http::send_sse(
            self.retry,
            &self.model,
            || self.post_messages(&body).timeout(http::STREAM_TIMEOUT),
            |data| stream.event(data, on_text, &self.model),
        )
        .await?;
        stream.finish(&self.model)
    }
}

impl AnthropicClient {
    fn post_messages(&self, body: &Value) -> reqwest::RequestBuilder {
        let r = self
            .request(reqwest::Method::POST, "v1/messages")
            .json(body);
        if self.uses_fallbacks() {
            r.header("anthropic-beta", FALLBACK_BETA)
        } else {
            r
        }
    }
}

/// What has arrived so far of a streamed message.
#[derive(Default)]
struct StreamState {
    text: String,
    model: Option<String>,
    usage: Usage,
    stop_reason: Option<String>,
    refusal: Option<String>,
}

impl StreamState {
    /// Reads one event. Returns `Ok(false)` once the message is complete.
    fn event(&mut self, data: &str, on_text: OnText<'_>, model: &str) -> Result<bool> {
        let v: Value = serde_json::from_str(data)
            .map_err(|e| ProviderError::invalid_response(format!("{e} in stream event")))?;
        match v["type"].as_str().unwrap_or_default() {
            "message_start" => {
                let message = &v["message"];
                self.model = message["model"].as_str().map(str::to_string);
                self.usage.input_tokens = input_tokens(&message["usage"]);
                self.usage.output_tokens = message["usage"]["output_tokens"].as_u64().unwrap_or(0);
            }
            "content_block_delta" if v["delta"]["type"] == "text_delta" => {
                if let Some(text) = v["delta"]["text"].as_str() {
                    self.text.push_str(text);
                    on_text(text);
                }
            }
            "message_delta" => {
                if let Some(reason) = v["delta"]["stop_reason"].as_str() {
                    self.stop_reason = Some(reason.to_string());
                }
                if let Some(explanation) = v["delta"]["stop_details"]["explanation"].as_str() {
                    self.refusal = Some(explanation.to_string());
                }
                // The usage here is the running total for the message.
                if let Some(out) = v["usage"]["output_tokens"].as_u64() {
                    self.usage.output_tokens = out;
                }
                let input = input_tokens(&v["usage"]);
                if input > 0 {
                    self.usage.input_tokens = input;
                }
            }
            "message_stop" => return Ok(false),
            "error" => {
                let status = match v["error"]["type"].as_str().unwrap_or_default() {
                    "overloaded_error" => 529,
                    "rate_limit_error" => 429,
                    "authentication_error" => 401,
                    "permission_error" => 403,
                    "not_found_error" => 404,
                    "request_too_large" => 413,
                    "invalid_request_error" => 400,
                    _ => 500,
                };
                return Err(http::classify(
                    status,
                    &reqwest::header::HeaderMap::new(),
                    data,
                    model,
                ));
            }
            _ => {}
        }
        Ok(true)
    }

    fn finish(self, requested_model: &str) -> Result<TextResponse> {
        match self.stop_reason.as_deref() {
            Some("refusal") => Err(ProviderError::Refused {
                detail: self
                    .refusal
                    .unwrap_or_else(|| "No explanation was given.".into()),
            }),
            Some("max_tokens") => Err(ProviderError::Truncated),
            None => Err(ProviderError::invalid_response(
                "the answer stopped before it was complete",
            )),
            Some(_) => Ok(TextResponse {
                text: self.text,
                model: self.model.unwrap_or_else(|| requested_model.to_string()),
                usage: self.usage,
            }),
        }
    }
}

/// Input tokens, counting those written to and read from the prompt cache.
fn input_tokens(usage: &Value) -> u64 {
    [
        "input_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
    ]
    .iter()
    .filter_map(|k| usage[*k].as_u64())
    .sum()
}

pub(crate) fn parse_message(v: &Value, requested_model: &str) -> Result<TextResponse> {
    match v["stop_reason"].as_str() {
        Some("refusal") => {
            let detail = v["stop_details"]["explanation"]
                .as_str()
                .unwrap_or("No explanation was given.");
            return Err(ProviderError::Refused {
                detail: detail.to_string(),
            });
        }
        Some("max_tokens") => return Err(ProviderError::Truncated),
        _ => {}
    }
    let blocks = v["content"]
        .as_array()
        .ok_or_else(|| ProviderError::invalid_response("no content array"))?;
    // Only text blocks are the answer; thinking and fallback blocks are skipped.
    let text: String = blocks
        .iter()
        .filter(|b| b["type"] == "text")
        .filter_map(|b| b["text"].as_str())
        .collect();
    Ok(TextResponse {
        text,
        model: v["model"].as_str().unwrap_or(requested_model).to_string(),
        usage: Usage {
            input_tokens: input_tokens(&v["usage"]),
            output_tokens: v["usage"]["output_tokens"].as_u64().unwrap_or(0),
        },
    })
}

pub(crate) fn model_ids(v: &Value) -> Result<Vec<String>> {
    let data = v["data"]
        .as_array()
        .ok_or_else(|| ProviderError::invalid_response("no data array in model list"))?;
    let mut ids: Vec<String> = data
        .iter()
        .filter_map(|m| m["id"].as_str().map(str::to_string))
        .collect();
    ids.sort();
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ImageInput;
    use mockito::Matcher;
    use std::time::Duration;

    fn client(server: &mockito::Server, model: &str) -> AnthropicClient {
        let mut c = AnthropicClient::new(
            http::client(),
            Some(&server.url()),
            "test-key".into(),
            model,
            Some("low".into()),
        );
        c.retry.base_delay = Duration::ZERO;
        c
    }

    #[test]
    fn body_has_image_before_text_and_no_sampling_params() {
        let c = AnthropicClient::new(http::client(), None, "k".into(), "claude-haiku-5-5", None);
        let req = TextRequest::new("Describe")
            .system("Be brief")
            .image(ImageInput::png(vec![1, 2, 3]))
            .json_schema("slide", json!({"type": "object"}));
        let b = c.body(&req);
        assert_eq!(b["messages"][0]["content"][0]["type"], "image");
        assert_eq!(b["messages"][0]["content"][0]["source"]["data"], "AQID");
        assert_eq!(b["messages"][0]["content"][1]["text"], "Describe");
        assert_eq!(b["system"], "Be brief");
        assert_eq!(b["output_config"]["format"]["type"], "json_schema");
        assert!(b.get("temperature").is_none());
        // Haiku is not in the fallback list.
        assert!(b.get("fallbacks").is_none());
    }

    #[tokio::test]
    async fn generate_sends_headers_and_parses_text() {
        let mut server = mockito::Server::new_async().await;
        let m = server
            .mock("POST", "/v1/messages")
            .match_header("x-api-key", "test-key")
            .match_header("anthropic-version", API_VERSION)
            .match_header("anthropic-beta", FALLBACK_BETA)
            .match_body(Matcher::PartialJson(json!({
                "model": "claude-opus-5-5",
                "fallbacks": "default",
                "output_config": {"effort": "low"}
            })))
            .with_body(
                json!({
                    "model": "claude-opus-5-5",
                    "stop_reason": "end_turn",
                    "content": [
                        {"type": "thinking", "thinking": ""},
                        {"type": "text", "text": "Hello"},
                        {"type": "text", "text": " there"}
                    ],
                    "usage": {"input_tokens": 12, "output_tokens": 3}
                })
                .to_string(),
            )
            .create_async()
            .await;

        let r = client(&server, "claude-opus-5-5")
            .generate(&TextRequest::new("Hi"))
            .await
            .unwrap();
        m.assert_async().await;
        assert_eq!(r.text, "Hello there");
        assert_eq!(
            r.usage,
            Usage {
                input_tokens: 12,
                output_tokens: 3
            }
        );
    }

    #[test]
    fn history_comes_before_the_prompt_and_system_is_cached() {
        use crate::types::{Turn, TurnRole};
        let c = AnthropicClient::new(http::client(), None, "k".into(), "claude-haiku-5-5", None);
        let req = TextRequest::new("And who owns it?")
            .system("Meeting context")
            .cache_system()
            .history(vec![
                Turn {
                    role: TurnRole::User,
                    text: "What was decided?".into(),
                },
                Turn {
                    role: TurnRole::Assistant,
                    text: "To ship Friday.".into(),
                },
            ]);
        let b = c.body(&req);
        assert_eq!(b["messages"][0]["role"], "user");
        assert_eq!(b["messages"][1]["role"], "assistant");
        assert_eq!(b["messages"][1]["content"], "To ship Friday.");
        assert_eq!(b["messages"][2]["content"][0]["text"], "And who owns it?");
        assert_eq!(b["system"][0]["text"], "Meeting context");
        assert_eq!(b["system"][0]["cache_control"]["type"], "ephemeral");
    }

    fn sse(events: &[Value]) -> String {
        events
            .iter()
            .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
            .collect()
    }

    #[tokio::test]
    async fn streaming_hands_over_text_as_it_arrives() {
        let mut server = mockito::Server::new_async().await;
        let body = sse(&[
            json!({"type": "message_start", "message": {"model": "claude-opus-5-5",
                "usage": {"input_tokens": 10, "cache_read_input_tokens": 90, "output_tokens": 1}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": ""}}),
            json!({"type": "ping"}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "Ship "}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "Friday."}}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 7}}),
            json!({"type": "message_stop"}),
        ]);
        let m = server
            .mock("POST", "/v1/messages")
            .match_body(Matcher::PartialJson(json!({"stream": true})))
            .with_header("content-type", "text/event-stream")
            .with_body(body)
            .create_async()
            .await;
        let pieces = std::sync::Mutex::new(Vec::<String>::new());
        let on_text = |t: &str| pieces.lock().unwrap().push(t.to_string());
        let r = client(&server, "claude-opus-5-5")
            .generate_streaming(&TextRequest::new("Hi"), &on_text)
            .await
            .unwrap();
        m.assert_async().await;
        assert_eq!(*pieces.lock().unwrap(), vec!["Ship ", "Friday."]);
        assert_eq!(r.text, "Ship Friday.");
        assert_eq!(
            r.usage,
            Usage {
                input_tokens: 100,
                output_tokens: 7
            }
        );
    }

    #[tokio::test]
    async fn streamed_refusal_and_error_events_are_errors() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/v1/messages")
            .with_body(sse(&[
                json!({"type": "message_start", "message": {"model": "m", "usage": {}}}),
                json!({"type": "content_block_delta", "delta": {"type": "text_delta", "text": "Part"}}),
                json!({"type": "message_delta", "delta": {"stop_reason": "refusal",
                    "stop_details": {"explanation": "Declined."}}, "usage": {}}),
                json!({"type": "message_stop"}),
            ]))
            .create_async()
            .await;
        let e = client(&server, "claude-haiku-5-5")
            .generate_streaming(&TextRequest::new("x"), &|_| {})
            .await
            .unwrap_err();
        assert_eq!(
            e,
            ProviderError::Refused {
                detail: "Declined.".into()
            }
        );

        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/v1/messages")
            .with_body(sse(&[json!({"type": "error",
                "error": {"type": "overloaded_error", "message": "Overloaded"}})]))
            .create_async()
            .await;
        let e = client(&server, "claude-haiku-5-5")
            .generate_streaming(&TextRequest::new("x"), &|_| {})
            .await
            .unwrap_err();
        assert!(
            matches!(e, ProviderError::Unavailable { status: 529, .. }),
            "{e:?}"
        );
    }

    #[tokio::test]
    async fn refusal_becomes_refused_error() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/v1/messages")
            .with_body(
                json!({
                    "stop_reason": "refusal",
                    "stop_details": {"type": "refusal", "category": null, "explanation": "Declined."},
                    "content": []
                })
                .to_string(),
            )
            .create_async()
            .await;
        let e = client(&server, "claude-haiku-5-5")
            .generate(&TextRequest::new("x"))
            .await
            .unwrap_err();
        assert_eq!(
            e,
            ProviderError::Refused {
                detail: "Declined.".into()
            }
        );
    }

    #[tokio::test]
    async fn bad_key_is_not_retried() {
        let mut server = mockito::Server::new_async().await;
        let m = server
            .mock("GET", "/v1/models/claude-opus-5-5")
            .with_status(401)
            .with_body(r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#)
            .expect(1)
            .create_async()
            .await;
        let e = client(&server, "claude-opus-5-5")
            .check()
            .await
            .unwrap_err();
        m.assert_async().await;
        assert!(matches!(e, ProviderError::InvalidKey { .. }));
    }

    #[tokio::test]
    async fn overloaded_is_retried_then_succeeds() {
        let mut server = mockito::Server::new_async().await;
        let busy = server
            .mock("GET", "/v1/models/claude-opus-5-5")
            .with_status(529)
            .with_body(
                r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
            )
            .expect(1)
            .create_async()
            .await;
        let ok = server
            .mock("GET", "/v1/models/claude-opus-5-5")
            .with_body(r#"{"id":"claude-opus-5-5","display_name":"Claude Opus 5.5"}"#)
            .expect(1)
            .create_async()
            .await;
        let r = client(&server, "claude-opus-5-5").check().await.unwrap();
        busy.assert_async().await;
        ok.assert_async().await;
        assert_eq!(r.display_name.as_deref(), Some("Claude Opus 5.5"));
    }
}
