use tauri::{
    menu::{MenuBuilder, MenuItemBuilder},
    tray::{MouseButton, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager,
};

/// Set up the system tray icon.
///
/// macOS behavior: if a menu is attached via `.menu()`, AppKit intercepts ALL
/// clicks at the OS level and shows the menu — `on_tray_icon_event` never fires.
/// Fix: don't attach the menu to the tray. Show it programmatically on right-click
/// so that left-click can open the window independently.
pub fn setup_tray(app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    let quit_i = MenuItemBuilder::with_id("quit", "Quit Astral").build(app)?;
    let menu = MenuBuilder::new(app).items(&[&quit_i]).build()?;
    let menu_for_click = menu.clone();

    let tray = TrayIconBuilder::new()
        .icon(
            app.default_window_icon()
                .ok_or("No default window icon configured")?
                .clone(),
        )
        // No .menu() here — attaching a menu causes macOS to swallow all clicks.
        .on_tray_icon_event(move |tray, event| {
            let app = tray.app_handle();
            match event {
                // Left click: show and focus the main window
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    ..
                } => {
                    if let Some(window) = app.get_webview_window("main") {
                        let _ = window.unminimize();
                        let _ = window.show();
                        let _ = window.set_focus();
                    }
                }
                // Right click: show "Quit Astral" context menu near the cursor
                TrayIconEvent::Click {
                    button: MouseButton::Right,
                    ..
                } => {
                    if let Some(window) = app.get_webview_window("main") {
                        let _ = window.popup_menu(&menu_for_click);
                    }
                }
                _ => {}
            }
        })
        .build(app)?;

    // CRITICAL: store the handle in managed state so it is not dropped.
    // Dropping TrayIcon unregisters all event handlers — the icon stays visible
    // in the OS but clicks do nothing.
    app.manage(tray);

    // Handle menu item events from the popup menu
    app.on_menu_event(|app, event| {
        if event.id().as_ref() == "quit" {
            app.exit(0);
        }
    });

    Ok(())
}
