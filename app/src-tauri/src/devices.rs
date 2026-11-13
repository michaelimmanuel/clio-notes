//! What to record, chosen in Settings: microphone, display, and how the
//! screen is turned into keyframes and read.
//!
//! Stored in `capture.json` next to `providers.json`. A missing choice means
//! the system default microphone and the primary display.

use std::path::PathBuf;

use capture::audio::{AudioDeviceInfo, list_devices};
use capture::screen::{MonitorInfo, list_monitors};
use pipeline::ScreenMode;
use serde::{Deserialize, Serialize};
use tauri::State;

use crate::settings::{AppState, UiError};

/// Screen readings per meeting unless the user picks another limit.
pub const DEFAULT_READ_CAP: u32 = 60;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CaptureSettings {
    /// Exact device name; `None` is the system default.
    pub microphone: Option<String>,
    /// `None` is the primary display.
    pub display: Option<DisplayChoice>,
    pub record_screen: bool,
    pub screen_mode: ScreenMode,
    /// Most screen readings per meeting; `None` is no limit.
    pub screen_read_cap: Option<u32>,
    /// Asks the user to tell everyone they are recording before each
    /// meeting starts.
    pub remind_consent: bool,
}

impl Default for CaptureSettings {
    fn default() -> Self {
        Self {
            microphone: None,
            display: None,
            record_screen: true,
            screen_mode: ScreenMode::Auto,
            screen_read_cap: Some(DEFAULT_READ_CAP),
            remind_consent: true,
        }
    }
}

/// Monitors are matched by name, with the position used to tell apart two
/// identical monitors.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DisplayChoice {
    pub name: String,
    pub index: usize,
}

impl DisplayChoice {
    /// The index of this display among `monitors`, if it is connected.
    pub fn resolve(&self, monitors: &[MonitorInfo]) -> Option<usize> {
        monitors
            .iter()
            .find(|m| m.index == self.index && m.name == self.name)
            .or_else(|| monitors.iter().find(|m| m.name == self.name))
            .map(|m| m.index)
    }
}

fn path(state: &AppState) -> PathBuf {
    state.store.path().with_file_name("capture.json")
}

pub(crate) fn load(state: &AppState) -> Result<CaptureSettings, UiError> {
    let path = path(state);
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(text.trim_start_matches('\u{feff}'))
            .map_err(|e| UiError::new("config", format!("{} is not valid: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(CaptureSettings::default()),
        Err(e) => Err(UiError::new(
            "config",
            format!("Cannot read {}: {e}", path.display()),
        )),
    }
}

fn save(state: &AppState, settings: &CaptureSettings) -> Result<(), UiError> {
    let path = path(state);
    let write = || -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(settings)?)
    };
    write().map_err(|e| UiError::new("config", format!("Cannot save {}: {e}", path.display())))
}

#[derive(Debug, Serialize)]
pub struct DevicesView {
    microphones: Vec<AudioDeviceInfo>,
    displays: Vec<MonitorInfo>,
    settings: CaptureSettings,
    /// Where the chosen display is now, or `None` if it is not connected.
    display_index: Option<usize>,
}

/// Lists microphones and displays. Runs off the main thread, where the audio
/// and screen APIs can set up COM as they need.
pub(crate) async fn connected() -> Result<(Vec<AudioDeviceInfo>, Vec<MonitorInfo>), UiError> {
    tauri::async_runtime::spawn_blocking(|| {
        let (inputs, _) = list_devices()
            .map_err(|e| UiError::new("capture", format!("Cannot list microphones: {e:#}")))?;
        let monitors = list_monitors()
            .map_err(|e| UiError::new("capture", format!("Cannot list displays: {e:#}")))?;
        Ok((inputs, monitors))
    })
    .await
    .map_err(|e| UiError::new("capture", e.to_string()))?
}

#[tauri::command]
pub async fn get_devices(state: State<'_, AppState>) -> Result<DevicesView, UiError> {
    let (microphones, displays) = connected().await?;
    let settings = load(&state)?;
    let display_index = settings.display.as_ref().and_then(|d| d.resolve(&displays));
    Ok(DevicesView {
        microphones,
        displays,
        settings,
        display_index,
    })
}

#[tauri::command]
pub async fn set_capture_devices(
    state: State<'_, AppState>,
    settings: CaptureSettings,
) -> Result<DevicesView, UiError> {
    save(&state, &settings)?;
    get_devices(state).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(index: usize, name: &str) -> MonitorInfo {
        MonitorInfo {
            index,
            name: name.into(),
            width: 1920,
            height: 1080,
            is_primary: index == 0,
        }
    }

    #[test]
    fn older_files_get_screen_defaults() {
        let s: CaptureSettings = serde_json::from_str(r#"{"microphone":"Mic"}"#).unwrap();
        assert_eq!(s.microphone.as_deref(), Some("Mic"));
        assert!(s.record_screen);
        assert_eq!(s.screen_read_cap, Some(DEFAULT_READ_CAP));
        let unlimited: CaptureSettings =
            serde_json::from_str(r#"{"screen_read_cap":null}"#).unwrap();
        assert_eq!(unlimited.screen_read_cap, None);
    }

    #[test]
    fn display_is_found_by_name_and_position() {
        let monitors = [monitor(0, "Dell"), monitor(1, "Dell"), monitor(2, "LG")];
        let pick = |name: &str, index| {
            DisplayChoice {
                name: name.into(),
                index,
            }
            .resolve(&monitors)
        };
        // Two identical monitors: the saved position decides.
        assert_eq!(pick("Dell", 1), Some(1));
        // Monitors were rearranged: the name still finds it.
        assert_eq!(pick("LG", 0), Some(2));
        assert_eq!(pick("Samsung", 0), None);
    }
}
