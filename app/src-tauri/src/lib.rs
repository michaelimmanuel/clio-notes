mod settings;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let state = settings::AppState::new().expect("cannot locate the settings folder");
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            settings::get_settings,
            settings::save_profile,
            settings::delete_profile,
            settings::assign_role,
            settings::test_profile,
            settings::list_models,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
