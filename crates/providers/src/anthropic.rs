//! Anthropic Messages API adapter (raw HTTP; there is no official Rust SDK).

use async_trait::async_trait;
use base64::Engine;
use serde_json::{Value, json};

use crate::TextGenerator;
use crate::error::{ProviderError, Result};
use crate::http::{self, RetryPolicy};
use crate::types::{CheckResult, TextRequest, TextResponse, Usage};

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

        let mut body = json!({
            "model": self.model,
            "max_tokens": req.max_tokens,
            "messages": [{ "role": "user", "content": content }],
        });
        if let Some(system) = &req.system {
            body["system"] = json!(system);
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
        let v = http::send_json(self.retry, &self.model, || {
            let r = self
                .request(reqwest::Method::POST, "v1/messages")
                .json(&body);
            if self.uses_fallbacks() {
                r.header("anthropic-beta", FALLBACK_BETA)
            } else {
                r
            }
        })
        .await?;
        parse_message(&v, &self.model)
    }
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
            input_tokens: v["usage"]["input_tokens"].as_u64().unwrap_or(0),
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
