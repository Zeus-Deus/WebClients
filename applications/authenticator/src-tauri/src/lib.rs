use log::LevelFilter;

#[cfg(debug_assertions)]
use specta_typescript::Typescript;

use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

mod auth;
mod biometrics;
mod crypto;
mod error;
#[cfg(target_os = "linux")]
mod helper;
mod storage_key;
mod store;

use tauri_specta::{collect_commands, collect_events, Builder};

// TODO: remove this once a patch is released for OpenPGP.js.
// Fix Proton login & app password lock crashing on recent Linux distros
// with WebKitGTK 2.50+ (Ubuntu 26.04+, Fedora 43+). The WebView hangs while
// loading openpgp.js's Argon2 SIMD module. Turning off WebAssembly SIMD
// makes openpgp use its non-SIMD build instead.
// Older distros e.g Ubuntu 24.04 / Fedora 42 and below (WebKitGTK 2.48) are
// not affected, but we apply this everywhere as newer versions are becoming LTS
#[cfg(target_os = "linux")]
fn apply_webkitgtk_workaround() {
    if std::env::var_os("JSC_useWasmSIMD").is_none() {
        std::env::set_var("JSC_useWasmSIMD", "false");
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[cfg(target_os = "linux")]
    apply_webkitgtk_workaround();

    #[cfg(target_os = "linux")]
    let builder = Builder::<tauri::Wry>::new()
        .commands(collect_commands![
            auth::log_in,
            biometrics::can_check_presence,
            biometrics::check_presence,
            helper::publish_helper_snapshot,
            storage_key::generate_storage_key,
            storage_key::get_storage_key,
            storage_key::remove_storage_key,
            store::get_theme,
            store::set_theme,
        ])
        .events(collect_events![]);

    #[cfg(not(target_os = "linux"))]
    let builder = Builder::<tauri::Wry>::new()
        .commands(collect_commands![
            auth::log_in,
            biometrics::can_check_presence,
            biometrics::check_presence,
            storage_key::generate_storage_key,
            storage_key::get_storage_key,
            storage_key::remove_storage_key,
            store::get_theme,
            store::set_theme,
        ])
        .events(collect_events![]);

    #[cfg(debug_assertions)]
    builder
        .export(
            Typescript::default().header("// @ts-nocheck"),
            "../src/lib/tauri/generated/__bindings__.ts",
        )
        .expect("Failed to export typescript bindings");

    let app_builder = tauri::Builder::default();
    #[cfg(target_os = "linux")]
    let app_builder = app_builder.manage(helper::HelperState::default());

    app_builder
        .invoke_handler(builder.invoke_handler())
        .setup(|app| {
            let version = app.package_info().version.to_string();
            #[cfg(target_os = "linux")]
            let background_mode = std::env::args().any(|arg| arg == "--background");
            #[cfg(target_os = "linux")]
            let login_requested = std::env::args().any(|arg| arg == "--login");
            let mut win_builder = WebviewWindowBuilder::new(app, "main", WebviewUrl::default())
                .title("Proton Authenticator")
                .user_agent(&auth::get_user_agent(version))
                .accept_first_mouse(true)
                .inner_size(800.0, 600.0)
                .min_inner_size(420.0, 480.0);

            #[cfg(target_os = "linux")]
            if background_mode && !login_requested {
                win_builder = win_builder.visible(false);
            }

            if !cfg!(debug_assertions) {
                win_builder = win_builder.content_protected(true)
            }

            let window = win_builder.build()?;

            #[cfg(not(target_os = "linux"))]
            let _ = window;

            #[cfg(target_os = "linux")]
            if background_mode {
                let helper_window = window.clone();
                window.on_window_event(move |event| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                        api.prevent_close();
                        let _ = helper_window.hide();
                    }
                });
            }

            #[cfg(target_os = "linux")]
            helper::start_socket_server(app.state::<helper::HelperState>().inner().clone())?;

            Ok(())
        })
        .plugin(
            tauri_plugin_log::Builder::new()
                .clear_targets()
                .level(LevelFilter::Debug)
                .target(tauri_plugin_log::Target::new(
                    tauri_plugin_log::TargetKind::Stdout,
                ))
                .target(tauri_plugin_log::Target::new(
                    tauri_plugin_log::TargetKind::LogDir {
                        file_name: Some("logs".to_string()),
                    },
                ))
                .build(),
        )
        .plugin(tauri_plugin_window_state::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            let login_requested = args.iter().any(|arg| arg == "--login");
            #[cfg(target_os = "linux")]
            if login_requested {
                app.state::<helper::HelperState>().unlock();
            }
            let _ = app.get_webview_window("main").and_then(|window| {
                if login_requested {
                    let _ = window.show();
                    let _ = window.unminimize();
                }
                window.set_focus().ok()
            });
        }))
        .plugin(tauri_plugin_store::Builder::new().build())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_http::init())
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
