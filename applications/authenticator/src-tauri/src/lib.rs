#[cfg(debug_assertions)]
use specta_typescript::Typescript;

#[cfg(target_os = "linux")]
use tauri::Emitter;
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

#[cfg(target_os = "linux")]
fn helper_window_mode<I, S>(args: I) -> (bool, bool)
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut background = false;
    let mut login = false;
    for argument in args {
        match argument.as_ref() {
            "--background" => background = true,
            "--login" => login = true,
            _ => {}
        }
    }
    (background, login)
}

#[cfg(target_os = "linux")]
fn helper_log_level(background: bool) -> log::LevelFilter {
    if background {
        log::LevelFilter::Off
    } else {
        log::LevelFilter::Debug
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::{helper_log_level, helper_window_mode};

    #[test]
    fn helper_window_mode_hides_background_except_login() {
        assert_eq!(helper_window_mode(["app", "--background"]), (true, false));
        assert_eq!(
            helper_window_mode(["app", "--background", "--login"]),
            (true, true)
        );
        assert_eq!(helper_window_mode(["app"]), (false, false));
    }

    #[test]
    fn background_helper_disables_application_logging() {
        assert_eq!(helper_log_level(true), log::LevelFilter::Off);
        assert_eq!(helper_log_level(false), log::LevelFilter::Debug);
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[cfg(target_os = "linux")]
    apply_webkitgtk_workaround();

    #[cfg(target_os = "linux")]
    let initial_helper_mode = helper_window_mode(std::env::args());
    #[cfg(target_os = "linux")]
    let log_level = helper_log_level(initial_helper_mode.0);
    #[cfg(not(target_os = "linux"))]
    let log_level = log::LevelFilter::Debug;

    #[cfg(target_os = "linux")]
    let builder = Builder::<tauri::Wry>::new()
        .commands(collect_commands![
            auth::log_in,
            biometrics::can_check_presence,
            biometrics::check_presence,
            helper::publish_helper_snapshot,
            helper::take_helper_login_request,
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
    let app_builder = app_builder.manage(helper::HelperState::with_login_requested(
        initial_helper_mode.1,
    ));

    let app_builder = app_builder
        .invoke_handler(builder.invoke_handler())
        .setup(move |app| {
            let version = app.package_info().version.to_string();
            #[cfg(target_os = "linux")]
            let (background_mode, login_requested) = initial_helper_mode;
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

            #[cfg(target_os = "linux")]
            if background_mode && !login_requested {
                window.hide()?;
            }

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
                .level(log_level)
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
        .plugin(tauri_plugin_process::init());

    // Fork-local divergence: the Linux build is maintained locally and must
    // never be replaced by an upstream binary, so the updater plugin is not
    // registered there. The `updater` pubkey stays in `tauri.conf.json` as a
    // trust anchor for verifying official builds.
    #[cfg(not(target_os = "linux"))]
    let app_builder = app_builder.plugin(tauri_plugin_updater::Builder::new().build());

    app_builder
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            let login_requested = args.iter().any(|arg| arg == "--login");
            #[cfg(target_os = "linux")]
            if login_requested {
                let state = app.state::<helper::HelperState>();
                // Login does not need an unlocked snapshot. Calling `unlock` here
                // would let any same-uid process clear the manual lock latch just
                // by running the binary with `--login`.
                state.request_login();
                let _ = app.emit_to("main", "omarchy-helper:login", ());
            }
            let _ = app.get_webview_window("main").and_then(|window| {
                let _ = window.show();
                let _ = window.unminimize();
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
