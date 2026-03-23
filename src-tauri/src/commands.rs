use std::sync::atomic::Ordering;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
use tunnel_core::{
    InitialOutcome, ReconnectConfig, TunnelConfig, TunnelEvent, TunnelStatsSnapshot,
};

use crate::control_plane::ClassifyResult;
use crate::network_runtime::{NetworkRuntime, RuntimeCommand};
use crate::state::TunnelState;

/// Sends a native system notification.
#[tauri::command]
pub async fn send_native_notification(
    app: AppHandle,
    title: String,
    body: Option<String>,
) -> Result<(), String> {
    log::info!("Sending native notification: {title}");

    #[cfg(not(mobile))]
    {
        use tauri_plugin_notification::NotificationExt;

        let mut notification = app.notification().builder().title(title);

        if let Some(body_text) = body {
            notification = notification.body(body_text);
        }

        match notification.show() {
            Ok(_) => {
                log::info!("Native notification sent successfully");
                Ok(())
            }
            Err(e) => {
                log::error!("Failed to send native notification: {e}");
                Err(format!("Failed to send notification: {e}"))
            }
        }
    }

    #[cfg(mobile)]
    {
        let _ = (app, body);
        Err("Native notifications not supported on mobile".to_string())
    }
}

#[derive(Serialize)]
pub struct ConnectResult {
    pub success: bool,
    pub error: Option<String>,
}

#[derive(Serialize)]
pub struct StatusResult {
    pub connected: bool,
    pub stats: Option<TunnelStatsSnapshot>,
}

/// Connect to the relay server (first connect — JS provides credentials).
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn connect(
    device_token: String,
    relay_jwt: String,
    relay_jwt_expires_at: u64,
    relay_url: String,
    api_base_url: String,
    desktop_api_jwt: Option<String>,
    desktop_api_jwt_expires_at: Option<u64>,
    vercel_bypass: Option<String>,
    app: AppHandle,
    tunnel_state: State<'_, TunnelState>,
) -> Result<ConnectResult, String> {
    // Check if already connected — clean up stale handles
    {
        let mut handle = tunnel_state.handle.lock().map_err(|e| e.to_string())?;
        if let Some(ref h) = *handle {
            if h.is_running() {
                return Ok(ConnectResult {
                    success: false,
                    error: Some("Already connected".to_string()),
                });
            }
            handle.take();
        }
    }

    // Update Vercel bypass on control plane client if provided by frontend.
    // The Vite-baked env var is only available in the WebView; Rust reads
    // std::env::var at runtime which may not be set in packaged builds.
    if let Some(ref bypass) = vercel_bypass {
        let cp: tauri::State<'_, std::sync::Arc<crate::control_plane::ControlPlaneClient>> =
            app.state();
        cp.set_vercel_bypass(bypass.clone());
        tunnel_state
            .credential_manager
            .set_vercel_bypass(Some(bypass.clone()))
            .await;
    }

    // Store credentials in credential store + cache JWT
    tunnel_state
        .credential_manager
        .initialize(
            &device_token,
            &relay_jwt,
            relay_jwt_expires_at,
            &relay_url,
            &api_base_url,
            desktop_api_jwt.as_deref(),
            desktop_api_jwt_expires_at.unwrap_or(0),
        )
        .await
        .map_err(|e| format!("Failed to store credentials: {e}"))?;

    // Start tunnel with reconnect loop
    let config = TunnelConfig {
        relay_url,
        relay_jwt,
    };

    let cred_mgr = tunnel_state.credential_manager.clone();
    let network_allowed = tunnel_state.network_allowed.clone();
    let network_notify = tunnel_state.network_notify.clone();

    let (handle, event_rx, outcome) = tunnel_core::start_and_wait(
        config,
        ReconnectConfig::default(),
        network_allowed,
        network_notify,
        move |force| {
            let cred_mgr = cred_mgr.clone();
            Box::pin(async move {
                if force {
                    cred_mgr.force_refresh().await
                } else {
                    cred_mgr.get_jwt().await
                }
            })
        },
    )
    .await;

    // Only keep the tunnel alive if the initial connect succeeded.
    // On failure/timeout, stop the tunnel to prevent background reconnect
    // from re-triggering "Connecting..." via stale tunnel:reconnecting events.
    match &outcome {
        InitialOutcome::Connected => {
            let mut guard = tunnel_state.handle.lock().map_err(|e| e.to_string())?;
            *guard = Some(handle);
            spawn_event_forwarder(app, event_rx);
        }
        _ => {
            handle.stop();
            // Drop event_rx — forwarder task will end when channel closes
        }
    }

    Ok(outcome_to_result(outcome))
}

/// Connect using stored credentials from credential store (app relaunch).
#[tauri::command]
pub async fn connect_with_stored_credentials(
    app: AppHandle,
    tunnel_state: State<'_, TunnelState>,
) -> Result<ConnectResult, String> {
    // Gate check removed — the runtime owns the network_allowed gate now.
    // initial_classify must be called before this command (JS handles ordering).

    // Check if already connected
    {
        let mut handle = tunnel_state.handle.lock().map_err(|e| e.to_string())?;
        if let Some(ref h) = *handle {
            if h.is_running() {
                return Ok(ConnectResult {
                    success: false,
                    error: Some("Already connected".to_string()),
                });
            }
            handle.take();
        }
    }

    // Load stored credentials and update credential manager's api_base_url
    let (relay_url, api_base_url) = tunnel_state
        .credential_manager
        .load_stored()
        .map_err(|e| format!("No stored credentials: {e}"))?;

    tunnel_state
        .credential_manager
        .set_api_base_url(&api_base_url)
        .await;

    // Get a valid relay JWT from cache or refresh if needed.
    // On cold relaunch, `hydrate_stored_credentials()` already refreshed once
    // for `initial_classify`, so this usually reuses the cached token instead
    // of rotating the device token family a second time.
    let relay_jwt = tunnel_state
        .credential_manager
        .get_jwt()
        .await
        .map_err(|e| match e {
            tunnel_core::RefreshError::AuthExpired => {
                "Auth expired — please sign in again".to_string()
            }
            tunnel_core::RefreshError::Transient => {
                "Could not reach server — try again later".to_string()
            }
        })?;

    let config = TunnelConfig {
        relay_url,
        relay_jwt,
    };

    let cred_mgr = tunnel_state.credential_manager.clone();
    let network_allowed = tunnel_state.network_allowed.clone();
    let network_notify = tunnel_state.network_notify.clone();

    let (handle, event_rx, outcome) = tunnel_core::start_and_wait(
        config,
        ReconnectConfig::default(),
        network_allowed,
        network_notify,
        move |force| {
            let cred_mgr = cred_mgr.clone();
            Box::pin(async move {
                if force {
                    cred_mgr.force_refresh().await
                } else {
                    cred_mgr.get_jwt().await
                }
            })
        },
    )
    .await;

    match &outcome {
        InitialOutcome::Connected => {
            let mut guard = tunnel_state.handle.lock().map_err(|e| e.to_string())?;
            *guard = Some(handle);
            spawn_event_forwarder(app, event_rx);
        }
        _ => {
            handle.stop();
        }
    }

    Ok(outcome_to_result(outcome))
}

/// Check if stored credentials exist in credential store.
#[tauri::command]
pub async fn has_stored_credentials(tunnel_state: State<'_, TunnelState>) -> Result<bool, String> {
    Ok(tunnel_state.credential_manager.has_stored_credentials())
}

/// Set the network_allowed gate (JS → Rust).
/// Pulses the Notify to wake the reconnect loop instantly when gate opens.
#[tauri::command]
pub async fn set_network_allowed(
    allowed: bool,
    tunnel_state: State<'_, TunnelState>,
) -> Result<(), String> {
    tunnel_state
        .network_allowed
        .store(allowed, Ordering::Relaxed);
    // Wake reconnect loop — it will check the AtomicBool and proceed if true.
    tunnel_state.network_notify.notify_one();
    log::info!("Network allowed set to {allowed}");
    Ok(())
}

/// Disconnect from the relay server.
#[tauri::command]
pub async fn disconnect(tunnel_state: State<'_, TunnelState>) -> Result<(), String> {
    let mut guard = tunnel_state.handle.lock().map_err(|e| e.to_string())?;
    if let Some(handle) = guard.take() {
        handle.stop();
    }
    Ok(())
}

/// Clear stored credentials (on sign-out).
#[tauri::command]
pub async fn clear_credentials(tunnel_state: State<'_, TunnelState>) -> Result<(), String> {
    tunnel_state.credential_manager.clear();
    Ok(())
}

/// Get current tunnel status and stats.
#[tauri::command]
pub async fn get_status(tunnel_state: State<'_, TunnelState>) -> Result<StatusResult, String> {
    let guard = tunnel_state.handle.lock().map_err(|e| e.to_string())?;
    match guard.as_ref() {
        Some(handle) if handle.is_running() => Ok(StatusResult {
            connected: true,
            stats: Some(handle.stats.snapshot()),
        }),
        _ => Ok(StatusResult {
            connected: false,
            stats: None,
        }),
    }
}

/// Hydrate stored credentials on cold relaunch — loads device token + api_base_url
/// from disk so the runtime can call classify/refresh endpoints.
/// Must be called BEFORE initial_classify on the stored-credentials path.
#[tauri::command]
pub async fn hydrate_stored_credentials(
    tunnel_state: State<'_, TunnelState>,
) -> Result<(), String> {
    let (_relay_url, api_base_url) = tunnel_state
        .credential_manager
        .load_stored()
        .map_err(|e| format!("No stored credentials: {e}"))?;

    tunnel_state
        .credential_manager
        .set_api_base_url(&api_base_url)
        .await;

    // Force-refresh both JWTs so get_api_jwt() has a valid token for classify
    tunnel_state
        .credential_manager
        .force_refresh()
        .await
        .map_err(|e| match e {
            tunnel_core::RefreshError::AuthExpired => {
                "Auth expired — please sign in again".to_string()
            }
            tunnel_core::RefreshError::Transient => {
                "Could not reach server — try again later".to_string()
            }
        })?;

    Ok(())
}

/// Set the Vercel deployment protection bypass token on the control plane client.
/// Called by the frontend with the Vite-baked env var (not available to Rust at runtime).
#[tauri::command]
pub async fn set_vercel_bypass(
    app: AppHandle,
    token: String,
    tunnel_state: State<'_, TunnelState>,
) -> Result<(), String> {
    let cp: tauri::State<'_, std::sync::Arc<crate::control_plane::ControlPlaneClient>> =
        app.state();
    cp.set_vercel_bypass(token.clone());
    tunnel_state
        .credential_manager
        .set_vercel_bypass(Some(token))
        .await;
    Ok(())
}

/// Prime credential storage/caches before the first fail-closed classification.
///
/// This stores freshly registered credentials without opening the tunnel yet,
/// so `initial_classify` can use the device token / desktop API JWT safely.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn prime_credentials(
    tunnel_state: State<'_, TunnelState>,
    device_token: String,
    relay_jwt: String,
    relay_jwt_expires_at: u64,
    relay_url: String,
    api_base_url: String,
    desktop_api_jwt: Option<String>,
    desktop_api_jwt_expires_at: Option<u64>,
) -> Result<(), String> {
    tunnel_state
        .credential_manager
        .initialize(
            &device_token,
            &relay_jwt,
            relay_jwt_expires_at,
            &relay_url,
            &api_base_url,
            desktop_api_jwt.as_deref(),
            desktop_api_jwt_expires_at.unwrap_or(0),
        )
        .await
        .map_err(|e| format!("Failed to store credentials: {e}"))?;

    Ok(())
}

/// Initial network classification — must be called before connect.
/// Delegates to the runtime, which classifies and opens the gate if safe.
/// Returns true if safe, false if unsafe.
#[tauri::command]
pub async fn initial_classify(runtime: State<'_, NetworkRuntime>) -> Result<bool, String> {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    runtime
        .send(RuntimeCommand::InitialClassify { reply: reply_tx })
        .await;
    reply_rx
        .await
        .map_err(|_| "Runtime not responding".to_string())?
}

/// Save a network as home/work — delegates to the runtime loop via command channel.
///
/// The runtime classifies the current IP with `save_label`, updates its state machine,
/// flips the gate, and emits events. The result is sent back via a oneshot channel.
#[tauri::command]
pub async fn save_network(
    runtime: State<'_, NetworkRuntime>,
    label: String,
) -> Result<ClassifyResult, String> {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    runtime
        .send(RuntimeCommand::SaveNetwork {
            label,
            reply: reply_tx,
        })
        .await;
    reply_rx
        .await
        .map_err(|_| "Runtime not responding".to_string())?
}

/// Update credentials after React re-auth (e.g. after AuthExpired).
///
/// Stores the new tokens in the credential manager and signals the runtime
/// to exit AuthExpired state and re-classify the network.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn update_credentials(
    tunnel_state: State<'_, TunnelState>,
    runtime: State<'_, NetworkRuntime>,
    device_token: String,
    relay_jwt: String,
    relay_jwt_expires_at: u64,
    relay_url: String,
    api_base_url: String,
    desktop_api_jwt: Option<String>,
    desktop_api_jwt_expires_at: Option<u64>,
) -> Result<(), String> {
    // Update credential storage + in-memory cache.
    tunnel_state
        .credential_manager
        .initialize(
            &device_token,
            &relay_jwt,
            relay_jwt_expires_at,
            &relay_url,
            &api_base_url,
            desktop_api_jwt.as_deref(),
            desktop_api_jwt_expires_at.unwrap_or(0),
        )
        .await
        .map_err(|e| format!("Failed to store credentials: {e}"))?;

    // Signal runtime to exit AuthExpired state and re-classify.
    runtime.send(RuntimeCommand::ResumeWithCredentials).await;
    Ok(())
}

/// Open the OAuth popup with a navigation guard that blocks Clerk's Account Portal.
/// If an unregistered user completes Google OAuth, Clerk redirects to its hosted
/// sign-in page instead of back to the app. This intercepts that redirect, closes
/// the popup, and emits an error event so the main window can show a message.
#[tauri::command]
pub async fn open_oauth_popup(app: AppHandle, url: String) -> Result<(), String> {
    let app_handle = app.clone();

    tauri::webview::WebviewWindowBuilder::new(
        &app,
        "oauth-popup",
        tauri::WebviewUrl::External(url.parse().map_err(|e| format!("{e}"))?),
    )
    .title("Sign in with Google")
    .inner_size(460.0, 460.0)
    .center()
    .resizable(true)
    .on_navigation(move |nav_url| {
        let url_str = nav_url.as_str();
        // Clerk redirects unregistered OAuth users to its Account Portal.
        // Block that navigation and notify the main window instead.
        if url_str.contains(".accounts.dev/sign-in") {
            log::warn!("Blocked navigation to Clerk Account Portal — user not registered");
            let _ = app_handle.emit("oauth-signup-blocked", ());
            // Close the popup from a spawned task (can't block here)
            let handle = app_handle.clone();
            std::thread::spawn(move || {
                if let Some(popup) = handle.get_webview_window("oauth-popup") {
                    let _ = popup.close();
                }
            });
            return false;
        }
        true
    })
    .build()
    .map_err(|e| e.to_string())?;

    Ok(())
}

/// Convert an InitialOutcome into a ConnectResult for the frontend.
fn outcome_to_result(outcome: InitialOutcome) -> ConnectResult {
    match outcome {
        InitialOutcome::Connected => ConnectResult {
            success: true,
            error: None,
        },
        InitialOutcome::Failed { reason } => ConnectResult {
            success: false,
            error: Some(reason),
        },
        InitialOutcome::AuthExpired => ConnectResult {
            success: false,
            error: Some("Auth expired — please sign in again".to_string()),
        },
        InitialOutcome::Timeout => ConnectResult {
            success: false,
            error: Some("Timed out connecting to relay".to_string()),
        },
    }
}

/// Spawn the event forwarder task: TunnelEvent → Tauri emit.
fn spawn_event_forwarder(app: AppHandle, mut event_rx: tokio::sync::mpsc::Receiver<TunnelEvent>) {
    tokio::spawn(async move {
        while let Some(event) = event_rx.recv().await {
            let event_name = match &event {
                TunnelEvent::Connecting => "tunnel:connecting",
                TunnelEvent::Connected => "tunnel:connected",
                TunnelEvent::StreamOpened { .. } => "tunnel:stream-opened",
                TunnelEvent::StreamClosed { .. } => "tunnel:stream-closed",
                TunnelEvent::Disconnected { .. } => "tunnel:disconnected",
                TunnelEvent::Error { .. } => "tunnel:error",
                TunnelEvent::Reconnecting { .. } => "tunnel:reconnecting",
                TunnelEvent::AuthExpired => "tunnel:auth-expired",
            };

            let payload = match &event {
                TunnelEvent::StreamOpened { target } => {
                    serde_json::json!({ "target": target })
                }
                TunnelEvent::StreamClosed { target } => {
                    serde_json::json!({ "target": target })
                }
                TunnelEvent::Disconnected { reason } => {
                    serde_json::json!({ "reason": reason })
                }
                TunnelEvent::Error { message } => {
                    serde_json::json!({ "message": message })
                }
                TunnelEvent::Reconnecting { attempt, delay_ms } => {
                    serde_json::json!({ "attempt": attempt, "delay_ms": delay_ms })
                }
                _ => serde_json::json!({}),
            };

            let _ = app.emit(event_name, payload);
        }
    });
}
