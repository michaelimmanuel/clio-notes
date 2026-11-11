//! Tauri commands behind the provider settings screen.
//!
//! API keys travel from the UI to these commands and straight into Windows
//! Credential Manager. They are never sent back to the UI, written to the
//! settings file, or logged.

use std::sync::Mutex;

use providers::{
    CheckResult, KeyStore, Profile, ProviderClient, ProviderError, ProviderKind, Role,
    RoleAssignments, Settings, SettingsStore, SystemKeyStore,
};
use serde::{Deserialize, Serialize};
use tauri::State;

pub struct AppState {
    store: SettingsStore,
    keys: SystemKeyStore,
    http: reqwest::Client,
    /// Serializes read-modify-write cycles on the settings file.
    write_lock: Mutex<()>,
}

impl AppState {
    pub fn new() -> Result<Self, ProviderError> {
        Ok(Self {
            store: SettingsStore::default_location()?,
            keys: SystemKeyStore,
            http: providers::http_client(),
            write_lock: Mutex::new(()),
        })
    }

    fn update(
        &self,
        change: impl FnOnce(&mut Settings) -> Result<(), ProviderError>,
    ) -> Result<SettingsView, UiError> {
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut settings = self.store.load()?;
        change(&mut settings)?;
        self.store.save(&settings)?;
        Ok(self.view(settings))
    }

    fn view(&self, settings: Settings) -> SettingsView {
        let profiles = settings
            .profiles
            .into_iter()
            .map(|p| ProfileView {
                has_key: matches!(self.keys.get(&p.id), Ok(Some(_))),
                requires_key: p.requires_key(),
                default_base_url: Profile {
                    base_url: None,
                    ..p.clone()
                }
                .base_url()
                .to_string(),
                profile: p,
            })
            .collect();
        SettingsView {
            profiles,
            roles: settings.roles,
            path: self.store.path().display().to_string(),
        }
    }
}

/// An error as the UI shows it: a category for styling and a sentence for the user.
#[derive(Debug, Serialize)]
pub struct UiError {
    kind: String,
    message: String,
}

impl From<ProviderError> for UiError {
    fn from(e: ProviderError) -> Self {
        let kind = serde_json::to_value(&e)
            .ok()
            .and_then(|v| v["kind"].as_str().map(str::to_string))
            .unwrap_or_else(|| "error".into());
        Self {
            kind,
            message: e.to_string(),
        }
    }
}

#[derive(Serialize)]
pub struct ProfileView {
    #[serde(flatten)]
    profile: Profile,
    has_key: bool,
    requires_key: bool,
    default_base_url: String,
}

#[derive(Serialize)]
pub struct SettingsView {
    profiles: Vec<ProfileView>,
    roles: RoleAssignments,
    path: String,
}

/// What the profile form sends. `id` is `None` for a new profile.
#[derive(Debug, Deserialize)]
pub struct ProfileInput {
    id: Option<String>,
    name: String,
    kind: ProviderKind,
    base_url: Option<String>,
    model: String,
    effort: Option<String>,
    #[serde(default)]
    legacy_max_tokens: bool,
}

impl ProfileInput {
    fn into_profile(self) -> Profile {
        let blank = |s: Option<String>| s.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let mut p = Profile::new(self.name.trim(), self.kind, self.model.trim());
        if let Some(id) = self.id {
            p.id = id;
        }
        p.base_url = blank(self.base_url);
        p.effort = blank(self.effort);
        p.legacy_max_tokens = self.kind == ProviderKind::OpenaiCompatible && self.legacy_max_tokens;
        p
    }
}

#[tauri::command]
pub fn get_settings(state: State<'_, AppState>) -> Result<SettingsView, UiError> {
    Ok(state.view(state.store.load()?))
}

/// Saves a profile. A non-empty `key` replaces the saved key; an empty or
/// missing one keeps whatever is saved.
#[tauri::command]
pub fn save_profile(
    state: State<'_, AppState>,
    input: ProfileInput,
    key: Option<String>,
) -> Result<SettingsView, UiError> {
    let profile = input.into_profile();
    let id = profile.id.clone();
    let view = state.update(|s| s.upsert_profile(profile))?;
    if let Some(key) = key.filter(|k| !k.trim().is_empty()) {
        state.keys.set(&id, &key)?;
        // Re-read so `has_key` reflects the key just saved.
        return Ok(state.view(state.store.load()?));
    }
    Ok(view)
}

#[tauri::command]
pub fn delete_profile(state: State<'_, AppState>, id: String) -> Result<SettingsView, UiError> {
    let view = state.update(|s| {
        s.remove_profile(&id);
        Ok(())
    })?;
    state.keys.delete(&id)?;
    Ok(view)
}

#[tauri::command]
pub fn assign_role(
    state: State<'_, AppState>,
    role: Role,
    profile_id: Option<String>,
) -> Result<SettingsView, UiError> {
    state.update(|s| s.assign(role, profile_id.as_deref()))
}

/// Builds a client from the form's current values, so a profile can be tested
/// before it is saved. Uses the typed key, or the saved key when none is typed.
fn draft_client(
    state: &AppState,
    input: ProfileInput,
    key: Option<String>,
) -> Result<ProviderClient, ProviderError> {
    let profile = input.into_profile();
    let key = match key.filter(|k| !k.trim().is_empty()) {
        Some(k) => Some(k),
        None => state.keys.get(&profile.id)?,
    };
    ProviderClient::new(&profile, key, &state.http)
}

#[tauri::command]
pub async fn test_profile(
    state: State<'_, AppState>,
    input: ProfileInput,
    key: Option<String>,
) -> Result<CheckResult, UiError> {
    let client = draft_client(&state, input, key)?;
    Ok(client.check().await?)
}

#[tauri::command]
pub async fn list_models(
    state: State<'_, AppState>,
    input: ProfileInput,
    key: Option<String>,
) -> Result<Vec<String>, UiError> {
    let client = draft_client(&state, input, key)?;
    Ok(client.list_models().await?)
}
