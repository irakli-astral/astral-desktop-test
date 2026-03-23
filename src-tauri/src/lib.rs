mod commands;
mod control_plane;
mod credential_store;
mod network_runtime;
mod network_watcher;
mod state;
mod tray;

use std::sync::Arc;

#[cfg(not(debug_assertions))]
use tauri::ipc::CapabilityBuilder;
use tauri::utils::config::BackgroundThrottlingPolicy;
use tauri::Manager;
use tauri_plugin_autostart::MacosLauncher;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // In dev mode, load .env from project root (parent of src-tauri/) so Rust
    // has VITE_* env vars like VITE_VERCEL_PROTECTION_BYPASS. Vite only exposes
    // these to the JS bundle, not to the Cargo process.
    if cfg!(debug_assertions) {
        let _ = dotenvy::from_path("../.env");
    }

    // In production, pick a random port for the localhost asset server.
    // This lets Clerk use normal cookies (tauri:// protocol breaks cookies on macOS).
    let port = if cfg!(debug_assertions) {
        0 // unused in dev — Vite dev server handles assets
    } else {
        portpicker::pick_unused_port().expect("failed to find an open port")
    };

    let mut app_builder = tauri::Builder::default();

    // Single instance: focus existing window if user opens a second one
    #[cfg(desktop)]
    {
        app_builder = app_builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.set_focus();
                let _ = window.unminimize();
            }
        }));
    }

    // Auto-start on login: registers a LaunchAgent on macOS, passes --background
    // so the app connects the tunnel silently without showing the window.
    app_builder = app_builder.plugin(tauri_plugin_autostart::init(
        MacosLauncher::LaunchAgent,
        Some(vec!["--background"]),
    ));

    // In production, serve frontend assets via localhost instead of tauri:// protocol.
    if !cfg!(debug_assertions) {
        app_builder = app_builder.plugin(tauri_plugin_localhost::Builder::new(port).build());
    }

    app_builder
        .plugin(tauri_plugin_store::Builder::default().build())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_opener::init())
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(if cfg!(debug_assertions) {
                    log::LevelFilter::Debug
                } else {
                    log::LevelFilter::Info
                })
                .targets([
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::Stdout),
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::Webview),
                ])
                .build(),
        )
        .invoke_handler(tauri::generate_handler![
            commands::connect,
            commands::connect_with_stored_credentials,
            commands::has_stored_credentials,
            commands::disconnect,
            commands::clear_credentials,
            commands::get_status,
            commands::set_network_allowed,
            commands::send_native_notification,
            commands::hydrate_stored_credentials,
            commands::set_vercel_bypass,
            commands::prime_credentials,
            commands::initial_classify,
            commands::save_network,
            commands::update_credentials,
            commands::open_oauth_popup,
        ])
        .setup(move |app| {
            log::info!("Astral starting up");

            // Disable macOS App Nap — without this, macOS throttles the process
            // when the window is unfocused (delays timers, I/O, CPU). This breaks
            // background network detection and periodic classify.
            // NSActivityUserInitiatedAllowingIdleSystemSleep: prevents App Nap but allows laptop sleep.
            // NSActivityLatencyCritical: prevents timer coalescing (30s periodic classify).
            #[cfg(target_os = "macos")]
            {
                use objc2_foundation::{NSActivityOptions, NSProcessInfo, NSString};
                let process_info = NSProcessInfo::processInfo();
                let reason = NSString::from_str(
                    "Astral tunnel must respond to network changes in background",
                );
                let activity = process_info.beginActivityWithOptions_reason(
                    NSActivityOptions::UserInitiatedAllowingIdleSystemSleep
                        | NSActivityOptions::LatencyCritical,
                    &reason,
                );
                // Activity must stay alive for the app's lifetime. Dropping it
                // re-enables App Nap. Intentional leak is fine for a tray app.
                std::mem::forget(activity);
                log::info!("App Nap disabled for background network detection");
            }

            // Initialize tunnel state with file-based credential storage.
            // Uses the app data directory (~/Library/Application Support/com.astral.desktop/
            // on macOS) — no OS keychain = no password prompts with ad-hoc signing.
            let app_data_dir = app
                .path()
                .app_data_dir()
                .expect("failed to resolve app data directory");
            app.manage(state::TunnelState::new(app_data_dir));

            let is_background = std::env::args().any(|a| a == "--background");
            if is_background {
                log::info!("Started in background mode (auto-start), tunnel will connect via frontend auto-connect");
            }

            // Create the main window. In dev, use Vite dev server URL.
            // In production, serve assets via localhost (for Clerk cookie support)
            // and register a CapabilityBuilder so IPC works on the external URL.
            #[cfg(debug_assertions)]
            let url = tauri::WebviewUrl::External("http://localhost:1420".parse().unwrap());

            #[cfg(not(debug_assertions))]
            let url = {
                let localhost_url: tauri::Url = format!("http://localhost:{port}").parse().unwrap();
                app.add_capability(
                    CapabilityBuilder::new("localhost")
                        .permission("core:default")
                        .permission("core:event:default")
                        .permission("notification:default")
                        .permission("core:webview:allow-create-webview-window")
                        .permission("updater:default")
                        .permission("process:default")
                        .remote(localhost_url.to_string())
                        .window("main"),
                )?;
                // OAuth popup window: allow it to close itself and emit events
                // back to the main window after completing SSO callback.
                app.add_capability(
                    CapabilityBuilder::new("localhost-popup")
                        .permission("core:window:allow-close")
                        .permission("core:event:allow-emit-to")
                        .remote(localhost_url.to_string())
                        .window("oauth-popup"),
                )?;
                tauri::WebviewUrl::External(localhost_url)
            };

            let mut win_builder = tauri::webview::WebviewWindowBuilder::new(
                app,
                "main",
                url,
            )
            .title("Astral")
            .inner_size(460.0, 460.0)
            .min_inner_size(400.0, 460.0)
            .resizable(true)
            .center()
            .shadow(true)
            // Tunnel app lives in tray — timers must fire when window is hidden.
            // Without this, macOS WKWebView suspends setInterval after ~5 min.
            .background_throttling(BackgroundThrottlingPolicy::Disabled);

            // macOS: transparent titlebar with hidden title (traffic lights only)
            #[cfg(target_os = "macos")]
            {
                win_builder = win_builder
                    .title_bar_style(tauri::TitleBarStyle::Transparent)
                    .hidden_title(true);
            }

            // Windows: no native decorations (custom window controls in frontend)
            #[cfg(target_os = "windows")]
            {
                win_builder = win_builder.decorations(false);
            }

            let window = win_builder.build()?;

            // In background mode (auto-start), hide the window so the tunnel
            // connects silently. The tray icon click will show it later.
            if is_background {
                let _ = window.hide();
            }

            // Set up system tray
            tray::setup_tray(app.handle())?;

            // Start network watcher and wire into NetworkRuntime.
            let (net_rx, watch_handle) = network_watcher::start()
                .expect("Failed to start network watcher");

            // Read Vercel bypass from multiple sources:
            // 1. Process env (works in dev mode where .env is loaded by Vite)
            // 2. Build-time env via env! macro (baked during `cargo build`)
            // 3. Falls back to None (JS will set it later via set_vercel_bypass)
            let vercel_bypass = std::env::var("VITE_VERCEL_PROTECTION_BYPASS")
                .ok()
                .or_else(|| option_env!("VITE_VERCEL_PROTECTION_BYPASS").map(String::from));
            let control_plane = Arc::new(control_plane::ControlPlaneClient::new(vercel_bypass));

            // Get credential_manager from the TunnelState we just managed above.
            let credential_manager = {
                let ts = app.state::<state::TunnelState>();
                ts.credential_manager.clone()
            };

            let runtime = network_runtime::NetworkRuntime::spawn(
                app.handle().clone(),
                net_rx,
                watch_handle,
                control_plane.clone(),
                credential_manager,
            );
            app.manage(runtime);
            app.manage(control_plane);

            // Intercept window close: hide to tray instead of quitting
            let win = window.clone();
            window.on_window_event(move |event| {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = win.hide();
                }
            });

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building Astral Tunnel")
        .run(|_app, _event| {
            // macOS: clicking the Dock icon when all windows are hidden should
            // reopen the main window (applicationShouldHandleReopen).
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen {
                has_visible_windows: false,
                ..
            } = _event
            {
                if let Some(window) = _app.get_webview_window("main") {
                    let _ = window.unminimize();
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
        });
}
