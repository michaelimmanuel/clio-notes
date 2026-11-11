//! Provider profiles and role assignments, saved as JSON in the app's config
//! folder. API keys are never stored here; see [`crate::keys`].

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{ProviderError, Result};
use crate::types::Role;
use crate::{anthropic, openai};

/// Same folder Tauri uses for the app's config, so the desktop app and the
/// command-line tools share one settings file.
const APP_DIR: &str = "com.clionotes.app";
const FILE_NAME: &str = "providers.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Anthropic,
    /// OpenAI or any service with an OpenAI-compatible API.
    OpenaiCompatible,
}

impl ProviderKind {
    pub fn label(self) -> &'static str {
        match self {
            ProviderKind::Anthropic => "Anthropic",
            ProviderKind::OpenaiCompatible => "OpenAI-compatible",
        }
    }
}

impl std::str::FromStr for ProviderKind {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_ascii_lowercase().replace('-', "_").as_str() {
            "anthropic" | "claude" => Ok(Self::Anthropic),
            "openai" | "openai_compatible" => Ok(Self::OpenaiCompatible),
            other => Err(format!(
                "unknown provider kind \"{other}\" (use anthropic or openai)"
            )),
        }
    }
}

/// One saved connection to a provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub kind: ProviderKind,
    /// `None` uses the provider's official endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    pub model: String,
    /// Reasoning effort passed to the provider (`low`, `medium`, `high`...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// For OpenAI-compatible servers that only accept `max_tokens`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub legacy_max_tokens: bool,
}

impl Profile {
    pub fn new(name: impl Into<String>, kind: ProviderKind, model: impl Into<String>) -> Self {
        Self {
            id: new_profile_id(),
            name: name.into(),
            kind,
            base_url: None,
            model: model.into(),
            effort: None,
            legacy_max_tokens: false,
        }
    }

    /// Whether this profile can be assigned to `role`. Anthropic's API does
    /// not transcribe audio; an OpenAI-compatible endpoint may do anything.
    pub fn supports(&self, role: Role) -> bool {
        !(self.kind == ProviderKind::Anthropic && role == Role::SpeechToText)
    }

    pub fn base_url(&self) -> &str {
        self.base_url.as_deref().unwrap_or(match self.kind {
            ProviderKind::Anthropic => anthropic::DEFAULT_BASE_URL,
            ProviderKind::OpenaiCompatible => openai::DEFAULT_BASE_URL,
        })
    }

    /// Local servers (Ollama, LM Studio, a self-hosted Whisper) need no key.
    pub fn requires_key(&self) -> bool {
        if self.kind == ProviderKind::Anthropic {
            return true;
        }
        let host = reqwest::Url::parse(self.base_url())
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_default();
        !matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]" | "::1")
    }

    fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            return Err(ProviderError::config("Profile name is empty."));
        }
        if self.model.trim().is_empty() {
            return Err(ProviderError::config("Model is empty."));
        }
        if let Some(url) = &self.base_url {
            let parsed = reqwest::Url::parse(url).map_err(|e| {
                ProviderError::config(format!("Base URL \"{url}\" is not valid: {e}"))
            })?;
            if !matches!(parsed.scheme(), "http" | "https") {
                return Err(ProviderError::config(
                    "Base URL must start with http:// or https://.",
                ));
            }
        }
        Ok(())
    }
}

/// Which profile handles each role. Values are profile ids.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RoleAssignments {
    #[serde(default)]
    pub speech_to_text: Option<String>,
    #[serde(default)]
    pub vision: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
}

impl RoleAssignments {
    pub fn get(&self, role: Role) -> Option<&str> {
        match role {
            Role::SpeechToText => self.speech_to_text.as_deref(),
            Role::Vision => self.vision.as_deref(),
            Role::Text => self.text.as_deref(),
        }
    }

    fn slot(&mut self, role: Role) -> &mut Option<String> {
        match role {
            Role::SpeechToText => &mut self.speech_to_text,
            Role::Vision => &mut self.vision,
            Role::Text => &mut self.text,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "current_version")]
    pub version: u32,
    #[serde(default)]
    pub profiles: Vec<Profile>,
    #[serde(default)]
    pub roles: RoleAssignments,
}

fn current_version() -> u32 {
    1
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: current_version(),
            profiles: Vec::new(),
            roles: RoleAssignments::default(),
        }
    }
}

impl Settings {
    /// Finds a profile by id, or by name ignoring case.
    pub fn profile(&self, reference: &str) -> Option<&Profile> {
        self.profiles
            .iter()
            .find(|p| p.id == reference)
            .or_else(|| {
                self.profiles
                    .iter()
                    .find(|p| p.name.eq_ignore_ascii_case(reference))
            })
    }

    /// Adds a new profile or replaces the one with the same id.
    pub fn upsert_profile(&mut self, profile: Profile) -> Result<()> {
        profile.validate()?;
        if self
            .profiles
            .iter()
            .any(|p| p.id != profile.id && p.name.eq_ignore_ascii_case(&profile.name))
        {
            return Err(ProviderError::config(format!(
                "A profile named \"{}\" already exists.",
                profile.name
            )));
        }
        // An edit can change the kind; drop roles the profile can no longer serve.
        for role in Role::ALL {
            if self.roles.get(role) == Some(profile.id.as_str()) && !profile.supports(role) {
                *self.roles.slot(role) = None;
            }
        }
        match self.profiles.iter_mut().find(|p| p.id == profile.id) {
            Some(existing) => *existing = profile,
            None => self.profiles.push(profile),
        }
        Ok(())
    }

    /// Removes a profile and clears any role it was assigned to.
    pub fn remove_profile(&mut self, id: &str) -> Option<Profile> {
        let index = self.profiles.iter().position(|p| p.id == id)?;
        for role in Role::ALL {
            if self.roles.get(role) == Some(id) {
                *self.roles.slot(role) = None;
            }
        }
        Some(self.profiles.remove(index))
    }

    /// Assigns `profile_id` to `role`, or clears the role with `None`.
    pub fn assign(&mut self, role: Role, profile_id: Option<&str>) -> Result<()> {
        if let Some(id) = profile_id {
            let profile =
                self.profiles.iter().find(|p| p.id == id).ok_or_else(|| {
                    ProviderError::config(format!("No profile with id \"{id}\"."))
                })?;
            if !profile.supports(role) {
                return Err(ProviderError::config(format!(
                    "{} profiles cannot be used for {}.",
                    profile.kind.label(),
                    role.label().to_lowercase()
                )));
            }
        }
        *self.roles.slot(role) = profile_id.map(str::to_string);
        Ok(())
    }

    pub fn assigned(&self, role: Role) -> Option<&Profile> {
        let id = self.roles.get(role)?;
        self.profiles.iter().find(|p| p.id == id)
    }
}

/// Reads and writes `providers.json`.
#[derive(Debug, Clone)]
pub struct SettingsStore {
    path: PathBuf,
}

impl SettingsStore {
    /// `%APPDATA%\com.clionotes.app\providers.json` on Windows. The
    /// `CLIO_NOTES_CONFIG_DIR` environment variable overrides the folder.
    pub fn default_location() -> Result<Self> {
        let dir = match std::env::var_os("CLIO_NOTES_CONFIG_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => dirs::config_dir()
                .ok_or_else(|| ProviderError::config("Cannot find the user config folder."))?
                .join(APP_DIR),
        };
        Ok(Self::at(dir.join(FILE_NAME)))
    }

    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Missing file means no profiles yet.
    pub fn load(&self) -> Result<Settings> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| {
                ProviderError::config(format!("{} is not valid: {e}", self.path.display()))
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
            Err(e) => Err(ProviderError::config(format!(
                "Cannot read {}: {e}",
                self.path.display()
            ))),
        }
    }

    /// Writes to a temporary file first so a crash never leaves half a file.
    pub fn save(&self, settings: &Settings) -> Result<()> {
        let io = |e: std::io::Error| {
            ProviderError::config(format!("Cannot write {}: {e}", self.path.display()))
        };
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(io)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(settings).expect("settings serialize");
        std::fs::write(&tmp, text).map_err(io)?;
        std::fs::rename(&tmp, &self.path).map_err(io)
    }
}

/// Unique enough for a handful of local profiles, and stable once saved.
pub fn new_profile_id() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("p{:x}{n:x}", nanos & 0xffff_ffff_ffff)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings_with(profiles: Vec<Profile>) -> Settings {
        let mut s = Settings::default();
        for p in profiles {
            s.upsert_profile(p).unwrap();
        }
        s
    }

    #[test]
    fn anthropic_cannot_do_speech() {
        let claude = Profile::new("Claude", ProviderKind::Anthropic, "claude-opus-5-5");
        let id = claude.id.clone();
        let mut s = settings_with(vec![claude]);
        assert!(s.assign(Role::SpeechToText, Some(&id)).is_err());
        s.assign(Role::Vision, Some(&id)).unwrap();
        assert_eq!(s.assigned(Role::Vision).unwrap().name, "Claude");
    }

    #[test]
    fn removing_a_profile_clears_its_roles() {
        let p = Profile::new("OpenAI", ProviderKind::OpenaiCompatible, "gpt-transcribe");
        let id = p.id.clone();
        let mut s = settings_with(vec![p]);
        s.assign(Role::SpeechToText, Some(&id)).unwrap();
        s.remove_profile(&id).unwrap();
        assert!(s.assigned(Role::SpeechToText).is_none());
    }

    #[test]
    fn changing_kind_drops_roles_it_cannot_serve() {
        let mut p = Profile::new("X", ProviderKind::OpenaiCompatible, "m");
        let mut s = settings_with(vec![p.clone()]);
        s.assign(Role::SpeechToText, Some(&p.id)).unwrap();
        s.assign(Role::Text, Some(&p.id)).unwrap();
        p.kind = ProviderKind::Anthropic;
        s.upsert_profile(p).unwrap();
        assert!(s.assigned(Role::SpeechToText).is_none());
        assert!(s.assigned(Role::Text).is_some());
    }

    #[test]
    fn duplicate_names_are_rejected_case_insensitively() {
        let mut s = settings_with(vec![Profile::new("Claude", ProviderKind::Anthropic, "m")]);
        assert!(
            s.upsert_profile(Profile::new("claude", ProviderKind::Anthropic, "m"))
                .is_err()
        );
    }

    #[test]
    fn local_servers_need_no_key() {
        let mut p = Profile::new("Ollama", ProviderKind::OpenaiCompatible, "llama3");
        assert!(p.requires_key());
        p.base_url = Some("http://localhost:11434/v1".into());
        assert!(!p.requires_key());
    }

    #[test]
    fn bad_base_url_is_rejected() {
        let mut p = Profile::new("X", ProviderKind::OpenaiCompatible, "m");
        p.base_url = Some("ftp://example.com".into());
        assert!(Settings::default().upsert_profile(p).is_err());
    }

    #[test]
    fn store_round_trips_and_missing_file_is_empty() {
        let dir = std::env::temp_dir().join(format!("clio-test-{}", new_profile_id()));
        let store = SettingsStore::at(dir.join("providers.json"));
        assert_eq!(store.load().unwrap(), Settings::default());
        let s = settings_with(vec![Profile::new("Claude", ProviderKind::Anthropic, "m")]);
        store.save(&s).unwrap();
        assert_eq!(store.load().unwrap(), s);
        let text = std::fs::read_to_string(store.path()).unwrap();
        assert!(!text.contains("key"), "settings file must not hold keys");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
