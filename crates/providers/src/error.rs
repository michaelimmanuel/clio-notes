use serde::Serialize;

/// Every way a provider call can fail, phrased for the person who owns the key.
///
/// Adapters map provider-specific status codes and error bodies onto these
/// variants, so the rest of the app never inspects raw HTTP errors.
#[derive(Debug, Clone, PartialEq, thiserror::Error, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProviderError {
    #[error("The API key was rejected. Check that it is correct and not revoked.")]
    InvalidKey { detail: String },

    #[error("The account has no credit or quota left. ({detail})")]
    NoCredit { detail: String },

    #[error("This key is not allowed to do this. ({detail})")]
    PermissionDenied { detail: String },

    #[error("Model \"{model}\" was not found, or this key cannot use it. ({detail})")]
    ModelNotFound { model: String, detail: String },

    #[error("The provider is rate limiting requests. Try again shortly.")]
    RateLimited {
        retry_after_secs: Option<u64>,
        detail: String,
    },

    #[error("The provider is unavailable or overloaded right now (HTTP {status}).")]
    Unavailable { status: u16, detail: String },

    #[error("The request was too large for the provider. ({detail})")]
    TooLarge { detail: String },

    #[error("The provider rejected the request: {detail}")]
    BadRequest { detail: String },

    #[error("The request timed out.")]
    Timeout,

    #[error("Could not reach {host}. Check the network or the base URL. ({detail})")]
    Network { host: String, detail: String },

    #[error("The model declined this request. {detail}")]
    Refused { detail: String },

    #[error("The response was cut off at the output token limit.")]
    Truncated,

    #[error("The provider sent a response this app could not read: {detail}")]
    InvalidResponse { detail: String },

    #[error("No API key is saved for profile \"{profile}\".")]
    MissingKey { profile: String },

    #[error("{detail}")]
    Config { detail: String },
}

impl ProviderError {
    /// Whether retrying the same request later can succeed.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::RateLimited { .. }
                | Self::Unavailable { .. }
                | Self::Timeout
                | Self::Network { .. }
        )
    }

    pub(crate) fn config(detail: impl Into<String>) -> Self {
        Self::Config {
            detail: detail.into(),
        }
    }

    pub(crate) fn invalid_response(detail: impl Into<String>) -> Self {
        Self::InvalidResponse {
            detail: detail.into(),
        }
    }
}

pub type Result<T> = std::result::Result<T, ProviderError>;
