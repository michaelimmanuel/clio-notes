use tauri::{Emitter, Manager};

mod devices;
mod logging;
mod meeting;
mod meetings;
mod settings;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    logging::init();
    let state = settings::AppState::new().expect("cannot locate the settings folder");
    meetings::recover_interrupted();
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(state)
        .manage(meeting::MeetingState::default())
        .manage(meetings::Finalizing::default())
        .on_window_event(|window, event| {
            // Closing the window would end the recording without its notes.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event
                && meeting::is_recording(window.app_handle())
            {
                api.prevent_close();
                let _ = window.emit("close-blocked", ());
            }
        })
        .invoke_handler(tauri::generate_handler![
            settings::get_settings,
            settings::save_profile,
            settings::delete_profile,
            settings::assign_role,
            settings::test_profile,
            settings::list_models,
            devices::get_devices,
            devices::set_capture_devices,
            meeting::start_meeting,
            meeting::stop_meeting,
            meeting::meeting_status,
            meetings::list_meetings,
            meetings::get_meeting,
            meetings::meeting_image,
            meetings::finalize_meeting,
            meetings::open_meeting_folder,
            meetings::delete_meeting,
            meetings::open_logs_folder,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
