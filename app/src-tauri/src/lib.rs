use tauri::{Emitter, Manager};

mod ask;
mod devices;
mod export;
mod library;
mod logging;
mod meeting;
mod meetings;
mod notion;
mod settings;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    logging::init();
    let state = settings::AppState::new().expect("cannot locate the settings folder");
    meetings::recover_interrupted();
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .manage(state)
        .manage(meeting::MeetingState::default())
        .manage(meetings::Finalizing::default())
        .manage(ask::Asking::default())
        .manage(library::LibraryState::default())
        .setup(|app| {
            library::start_worker(app.handle().clone());
            Ok(())
        })
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
            meetings::save_final_notes,
            meetings::open_meeting_folder,
            meetings::delete_meeting,
            meetings::open_logs_folder,
            ask::chat_history,
            ask::ask_meeting,
            ask::clear_chat,
            library::search_meetings,
            library::library_chat,
            library::ask_all_meetings,
            library::clear_library_chat,
            library::get_meaning_status,
            library::retry_meaning,
            export::save_notes,
            notion::notion_status,
            notion::notion_connect,
            notion::notion_disconnect,
            notion::notion_pages,
            notion::notion_set_parent,
            notion::notion_export,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
