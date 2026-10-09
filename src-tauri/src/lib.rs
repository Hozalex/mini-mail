mod commands;
mod ews;

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .manage(commands::AppState::default())
        .invoke_handler(tauri::generate_handler![
            commands::restore_session,
            commands::login,
            commands::logout,
            commands::list_folders,
            commands::list_messages,
            commands::create_folder,
            commands::get_message,
            commands::mark_read,
            commands::delete_message,
            commands::move_message,
            commands::mark_all_read,
            commands::respond_to_meeting,
            commands::set_badge,
            commands::send_message,
            commands::resolve_names,
            commands::open_link,
            commands::download_attachment,
        ])
        .run(tauri::generate_context!())
        .expect("error while running mini-mail");
}
