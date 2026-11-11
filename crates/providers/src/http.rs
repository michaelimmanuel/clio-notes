//! Shared HTTP plumbing: client setup, retries and error classification.

use std::time::Duration;

use reqwest::header::HeaderMap;
use serde_json::Value;

use crate::error::{ProviderError, Result};

/// Builds the HTTP client shared by all adapters.
pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(180))
        .user_agent(concat!("clio-notes/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("HTTP client configuration is valid")
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct RetryPolicy {
    pub max_retries: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 2,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(20),
        }
    }
}

/// Sends a request, retrying rate limits, server errors and network failures.
///
/// `build` is called once per attempt because multipart bodies cannot be
/// reused. Returns the parsed JSON body of a successful response.
pub(crate) async fn send_json(
    policy: RetryPolicy,
    model: &str,
    build: impl Fn() -> reqwest::RequestBuilder,
) -> Result<Value> {
    let mut attempt = 0;
    loop {
        let err = match send_once(model, build()).await {
            Ok(v) => return Ok(v),
            Err(e) => e,
        };
        if !err.is_retryable() || attempt >= policy.max_retries {
            return Err(err);
        }
        let delay = match &err {
            ProviderError::RateLimited {
                retry_after_secs: Some(s),
                ..
            } => Duration::from_secs(*s),
            _ => policy.base_delay * 2u32.pow(attempt),
        };
        tokio::time::sleep(delay.min(policy.max_delay)).await;
        attempt += 1;
    }
}

async fn send_once(model: &str, request: reqwest::RequestBuilder) -> Result<Value> {
    let response = request.send().await.map_err(transport_error)?;
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.text().await.map_err(transport_error)?;

    if status.is_success() {
        return serde_json::from_str(&body)
            .map_err(|e| ProviderError::invalid_response(format!("{e}: {}", snippet(&body))));
    }
    Err(classify(status.as_u16(), &headers, &body, model))
}

fn transport_error(e: reqwest::Error) -> ProviderError {
    if e.is_timeout() {
        return ProviderError::Timeout;
    }
    let host = e
        .url()
        .and_then(|u| u.host_str())
        .unwrap_or("the provider")
        .to_string();
    // reqwest's Display omits the root cause, which is the useful part.
    let mut detail = e.to_string();
    let mut source = std::error::Error::source(&e);
    while let Some(s) = source {
        detail = s.to_string();
        source = s.source();
    }
    ProviderError::Network { host, detail }
}

/// Maps an error response onto [`ProviderError`]. Handles both the Anthropic
/// shape (`{"error": {"type", "message"}}`) and the OpenAI shape
/// (`{"error": {"message", "type", "code"}}`).
pub(crate) fn classify(status: u16, headers: &HeaderMap, body: &str, model: &str) -> ProviderError {
    let parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let error = &parsed["error"];
    let field = |k: &str| error[k].as_str().unwrap_or_default().to_string();
    let (kind, code) = (field("type"), field("code"));
    let message = match error["message"].as_str() {
        Some(m) if !m.is_empty() => m.to_string(),
        _ => snippet(body),
    };
    let mut detail = if message.is_empty() {
        format!("HTTP {status}")
    } else {
        message.clone()
    };
    // OpenRouter wraps upstream failures as "Provider returned 403" and puts
    // the upstream provider and its own error text in `error.metadata`.
    let metadata = &error["metadata"];
    if let Some(provider) = metadata["provider_name"].as_str() {
        detail.push_str(&format!(" by {provider}"));
    }
    if let Some(raw) = metadata["raw"].as_str().filter(|r| !r.is_empty()) {
        let raw_message = serde_json::from_str::<Value>(raw)
            .ok()
            .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
            .unwrap_or_else(|| snippet(raw));
        detail.push_str(&format!(": {raw_message}"));
    }
    let has = |s: &str| kind == s || code == s;
    let lower = message.to_lowercase();

    match status {
        401 => ProviderError::InvalidKey { detail },
        402 => ProviderError::NoCredit { detail },
        403 => ProviderError::PermissionDenied { detail },
        404 => ProviderError::ModelNotFound {
            model: model.to_string(),
            detail,
        },
        408 => ProviderError::Timeout,
        413 => ProviderError::TooLarge { detail },
        429 if has("insufficient_quota") => ProviderError::NoCredit { detail },
        429 => ProviderError::RateLimited {
            retry_after_secs: retry_after(headers),
            detail,
        },
        400 if has("model_not_found") => ProviderError::ModelNotFound {
            model: model.to_string(),
            detail,
        },
        400 if lower.contains("credit balance") => ProviderError::NoCredit { detail },
        500..=599 => ProviderError::Unavailable { status, detail },
        _ => ProviderError::BadRequest {
            detail: format!("HTTP {status}: {detail}"),
        },
    }
}

fn retry_after(headers: &HeaderMap) -> Option<u64> {
    headers
        .get("retry-after")?
        .to_str()
        .ok()?
        .trim()
        .parse::<f64>()
        .ok()
        .map(|s| s.ceil().max(0.0) as u64)
}

fn snippet(body: &str) -> String {
    let body = body.trim();
    match body.char_indices().nth(300) {
        Some((i, _)) => format!("{}…", &body[..i]),
        None => body.to_string(),
    }
}

/// Joins a base URL and a path without doubling or dropping the slash.
pub(crate) fn join(base: &str, path: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify_body(status: u16, body: &str) -> ProviderError {
        classify(status, &HeaderMap::new(), body, "m")
    }

    #[test]
    fn openai_quota_is_no_credit_not_rate_limit() {
        let body = r#"{"error":{"message":"You exceeded your current quota","type":"insufficient_quota","code":"insufficient_quota"}}"#;
        assert!(matches!(
            classify_body(429, body),
            ProviderError::NoCredit { .. }
        ));
    }

    #[test]
    fn anthropic_low_credit_is_no_credit() {
        let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"Your credit balance is too low to access the Anthropic API."}}"#;
        assert!(matches!(
            classify_body(400, body),
            ProviderError::NoCredit { .. }
        ));
    }

    #[test]
    fn overloaded_529_is_retryable() {
        let e = classify_body(
            529,
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
        );
        assert!(matches!(e, ProviderError::Unavailable { status: 529, .. }));
        assert!(e.is_retryable());
    }

    #[test]
    fn retry_after_header_is_read() {
        let mut h = HeaderMap::new();
        h.insert("retry-after", "7".parse().unwrap());
        let e = classify(429, &h, "{}", "m");
        assert!(matches!(
            e,
            ProviderError::RateLimited {
                retry_after_secs: Some(7),
                ..
            }
        ));
    }

    #[test]
    fn non_json_error_body_still_gives_a_message() {
        let e = classify_body(502, "<html>Bad gateway</html>");
        assert_eq!(
            e,
            ProviderError::Unavailable {
                status: 502,
                detail: "<html>Bad gateway</html>".into()
            }
        );
    }

    #[test]
    fn openrouter_upstream_error_detail_is_kept() {
        let body = r#"{"error":{"code":403,"message":"Provider returned 403","metadata":{"provider_name":"OpenAI","raw":"{\"error\":{\"message\":\"Region not supported\"}}"}}}"#;
        let e = classify_body(403, body);
        assert!(
            e.to_string()
                .contains("Provider returned 403 by OpenAI: Region not supported"),
            "{e}"
        );
    }

    #[test]
    fn join_handles_slashes() {
        assert_eq!(join("https://a/v1/", "/models"), "https://a/v1/models");
        assert_eq!(join("https://a/v1", "models"), "https://a/v1/models");
    }
}
