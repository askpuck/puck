mod ask;
mod cli;
mod cloud;
mod context;
mod coordinatore;
mod coder;
mod image;
mod provider;
mod todo;
mod shell;
mod stream;
mod view;

use std::sync::Mutex;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_deep_link::init())
        .manage(Mutex::new(context::Memory::load()))
        .manage(ask::AskGate::new())
        .invoke_handler(tauri::generate_handler![
            coordinatore::ask_coordinatore,
            ask::answer_user,
            coder::save_workspace_files,
            context::reset_context,
            context::load_ui_session,
            context::save_ui_session,
            context::get_workspace,
            context::set_workspace,
            cloud::cloud_status,
            cloud::cloud_connect,
            cloud::cloud_auth_callback,
            cloud::cloud_signout,
            cli::cli_ack,
            cli::cli_load_file,
        ])
        .setup(|app| {
            cloud::load_user_env(app.handle());
            cli::spawn_watcher(app.handle().clone());
            let h = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let _ = cloud::prepare_app(&h).await;
            });
            #[cfg(desktop)]
            {
                use tauri_plugin_deep_link::DeepLinkExt;
                let h = app.handle().clone();
                app.deep_link().on_open_url(move |event| {
                    for url in event.urls() {
                        let raw = url.to_string();
                        if raw.starts_with("puck://auth") {
                            let h = h.clone();
                            tauri::async_runtime::spawn(async move {
                                cloud::auth_from_app(&h, &raw).await;
                            });
                        }
                    }
                });
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { .. } = event {
                let _ = cloud::push_now();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
