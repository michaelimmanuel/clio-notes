//! The bring-your-own-key provider layer.
//!
//! The rest of the app sees two traits, [`SpeechToText`] and
//! [`TextGenerator`], and three [`Role`]s. Screen reading (the `Vision` role)
//! is a `TextGenerator` call with images attached, assigned to a profile whose
//! model accepts images. Which provider sits behind each role comes from the
//! user's [`Settings`].

pub mod anthropic;
pub mod error;
mod http;
pub mod keys;
pub mod openai;
pub mod profile;
pub mod types;

use std::sync::Arc;

use async_trait::async_trait;

pub use anthropic::AnthropicClient;
pub use error::{ProviderError, Result};
pub use http::client as http_client;
pub use keys::{KeyStore, MemoryKeyStore, SystemKeyStore};
pub use openai::OpenAiClient;
pub use profile::{Profile, ProviderKind, RoleAssignments, Settings, SettingsStore};
pub use types::*;

#[async_trait]
pub trait TextGenerator: Send + Sync {
    async fn generate(&self, request: &TextRequest) -> Result<TextResponse>;
}

#[async_trait]
pub trait SpeechToText: Send + Sync {
    async fn transcribe(&self, request: &TranscribeRequest) -> Result<Transcript>;
}

/// A ready-to-use client for one profile.
#[derive(Clone)]
pub enum ProviderClient {
    Anthropic(AnthropicClient),
    OpenAi(OpenAiClient),
}

impl ProviderClient {
    /// Builds a client with an explicit key, for example one typed into the
    /// settings screen but not saved yet.
    pub fn new(profile: &Profile, key: Option<String>, http: &reqwest::Client) -> Result<Self> {
        let key = key.filter(|k| !k.trim().is_empty());
        if key.is_none() && profile.requires_key() {
            return Err(ProviderError::MissingKey {
                profile: profile.name.clone(),
            });
        }
        Ok(match profile.kind {
            ProviderKind::Anthropic => Self::Anthropic(AnthropicClient::new(
                http.clone(),
                profile.base_url.as_deref(),
                key.unwrap_or_default(),
                profile.model.clone(),
                profile.effort.clone(),
            )),
            ProviderKind::OpenaiCompatible => Self::OpenAi(OpenAiClient::new(
                http.clone(),
                profile.base_url.as_deref(),
                key,
                profile.model.clone(),
                profile.effort.clone(),
                profile.legacy_max_tokens,
            )),
            ProviderKind::Openrouter => Self::OpenAi(OpenAiClient::openrouter(
                http.clone(),
                profile.base_url.as_deref(),
                key.unwrap_or_default(),
                profile.model.clone(),
                profile.effort.clone(),
            )),
        })
    }

    /// Builds a client using the key saved for this profile.
    pub fn for_profile(
        profile: &Profile,
        keys: &dyn KeyStore,
        http: &reqwest::Client,
    ) -> Result<Self> {
        Self::new(profile, keys.get(&profile.id)?, http)
    }

    /// Checks the key and model without generating anything (free).
    pub async fn check(&self) -> Result<CheckResult> {
        match self {
            Self::Anthropic(c) => c.check().await,
            Self::OpenAi(c) => c.check().await,
        }
    }

    pub async fn list_models(&self) -> Result<Vec<String>> {
        match self {
            Self::Anthropic(c) => c.list_models().await,
            Self::OpenAi(c) => c.list_models().await,
        }
    }

    pub fn into_text_generator(self) -> Arc<dyn TextGenerator> {
        match self {
            Self::Anthropic(c) => Arc::new(c),
            Self::OpenAi(c) => Arc::new(c),
        }
    }

    pub fn into_speech_to_text(self) -> Option<Arc<dyn SpeechToText>> {
        match self {
            Self::Anthropic(_) => None,
            Self::OpenAi(c) => Some(Arc::new(c)),
        }
    }
}

fn assigned_client(
    settings: &Settings,
    role: Role,
    keys: &dyn KeyStore,
    http: &reqwest::Client,
) -> Result<ProviderClient> {
    let profile = settings
        .assigned(role)
        .ok_or_else(|| ProviderError::Config {
            detail: format!("No profile is assigned to {} in Settings.", role.label()),
        })?;
    ProviderClient::for_profile(profile, keys, http)
}

/// The text generator for the `Vision` or `Text` role.
pub fn text_generator(
    settings: &Settings,
    role: Role,
    keys: &dyn KeyStore,
    http: &reqwest::Client,
) -> Result<Arc<dyn TextGenerator>> {
    if role == Role::SpeechToText {
        return Err(ProviderError::Config {
            detail: "Speech to text is not a text generation role.".into(),
        });
    }
    Ok(assigned_client(settings, role, keys, http)?.into_text_generator())
}

/// The transcriber assigned to the `SpeechToText` role.
pub fn speech_to_text(
    settings: &Settings,
    keys: &dyn KeyStore,
    http: &reqwest::Client,
) -> Result<Arc<dyn SpeechToText>> {
    assigned_client(settings, Role::SpeechToText, keys, http)?
        .into_speech_to_text()
        .ok_or_else(|| ProviderError::Config {
            detail: "The profile assigned to speech to text cannot transcribe audio.".into(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_key_is_reported_by_profile_name() {
        let p = Profile::new("My Claude", ProviderKind::Anthropic, "claude-opus-5-5");
        let e = ProviderClient::for_profile(&p, &MemoryKeyStore::default(), &http_client())
            .err()
            .unwrap();
        assert_eq!(
            e,
            ProviderError::MissingKey {
                profile: "My Claude".into()
            }
        );
    }

    #[test]
    fn unassigned_role_is_a_clear_config_error() {
        let e = text_generator(
            &Settings::default(),
            Role::Text,
            &MemoryKeyStore::default(),
            &http_client(),
        )
        .err()
        .unwrap();
        assert_eq!(
            e.to_string(),
            "No profile is assigned to Text generation in Settings."
        );
    }

    #[test]
    fn roles_resolve_to_the_assigned_profile() {
        let keys = MemoryKeyStore::default();
        let openai = Profile::new("OpenAI", ProviderKind::OpenaiCompatible, "gpt-transcribe");
        keys.set(&openai.id, "sk-x").unwrap();
        let mut s = Settings::default();
        s.upsert_profile(openai.clone()).unwrap();
        s.assign(Role::SpeechToText, Some(&openai.id)).unwrap();
        assert!(speech_to_text(&s, &keys, &http_client()).is_ok());
    }
}
