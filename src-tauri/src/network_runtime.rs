use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::mpsc;

use crate::control_plane::{ClassifyResult, ControlPlaneClient, ControlPlaneError};
use crate::network_watcher::NetworkEventRx;
use crate::state::TunnelState;
use tunnel_core::credentials::{CredentialManager, RefreshError};

// ---------------------------------------------------------------------------
// RuntimeState — internal state machine
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum RuntimeState {
    Idle,
    Classifying,
    Connected,
    Unsafe,
    /// Fail-closed: classification failed, slow retries every 15 s.
    Unknown,
    /// Desktop API JWT expired — paused until React re-auths.
    AuthExpired,
}

// ---------------------------------------------------------------------------
// RuntimeCommand — messages from Tauri commands into the runtime loop
// ---------------------------------------------------------------------------

/// Commands sent to the runtime from Tauri commands (single-owner pattern).
/// All state mutations flow through the runtime loop — commands NEVER
/// directly flip `network_allowed` or emit events.
pub enum RuntimeCommand {
    /// Initial classification before first connect / after stored-credential load.
    /// Opens the gate if safe, blocks connect if unsafe. Reply carries is_safe.
    InitialClassify {
        reply: tokio::sync::oneshot::Sender<Result<bool, String>>,
    },
    /// User clicked "Set as Home" / "Set as Work" — classify with save_label.
    SaveNetwork {
        label: String,
        reply: tokio::sync::oneshot::Sender<Result<ClassifyResult, String>>,
    },
    /// React re-authed after AuthExpired — feed fresh tokens, resume classify.
    ResumeWithCredentials,
    /// Shutdown the runtime.
    #[allow(dead_code)]
    Shutdown,
}

// ---------------------------------------------------------------------------
// NetworkRuntime — public handle (Tauri managed state)
// ---------------------------------------------------------------------------

pub struct NetworkRuntime {
    cmd_tx: mpsc::Sender<RuntimeCommand>,
    // WatchHandle stored behind Mutex for the Sync requirement.
    // Tauri managed state requires Send + Sync + 'static.
    // Box<dyn Any + Send> is NOT Sync, but Mutex<T> is Sync when T: Send.
    // The Mutex is never contended — it exists solely to satisfy the trait bound.
    _watcher_handle: std::sync::Mutex<Box<dyn std::any::Any + Send>>,
}

impl NetworkRuntime {
    /// Spawn the runtime loop. Returns a handle with a command channel.
    ///
    /// The `watcher_handle` is moved into the runtime to keep it alive via RAII
    /// (dropping it would stop OS-level network notifications).
    pub fn spawn(
        app: AppHandle,
        net_rx: NetworkEventRx,
        watcher_handle: Box<dyn std::any::Any + Send>,
        control_plane: Arc<ControlPlaneClient>,
        credential_manager: Arc<CredentialManager>,
    ) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        tauri::async_runtime::spawn(run(app, net_rx, cmd_rx, control_plane, credential_manager));
        Self {
            cmd_tx,
            _watcher_handle: std::sync::Mutex::new(watcher_handle),
        }
    }

    /// Send a command to the runtime. Used by Tauri commands.
    pub async fn send(&self, cmd: RuntimeCommand) {
        let _ = self.cmd_tx.send(cmd).await;
    }
}

// ---------------------------------------------------------------------------
// Core event loop
// ---------------------------------------------------------------------------

async fn run(
    app: AppHandle,
    mut net_rx: NetworkEventRx,
    mut cmd_rx: mpsc::Receiver<RuntimeCommand>,
    control_plane: Arc<ControlPlaneClient>,
    credential_manager: Arc<CredentialManager>,
) {
    let mut state = RuntimeState::Idle;

    // IMPORTANT: tokio::time::interval first tick completes immediately.
    // We must consume it to prevent spurious immediate-fire on first select! iteration.
    // See: https://docs.rs/tokio/latest/tokio/time/fn.interval.html

    // 30 s periodic re-classify when Connected (catches VPN / IP changes).
    let mut periodic_interval = tokio::time::interval(Duration::from_secs(30));
    periodic_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    periodic_interval.tick().await; // consume immediate first tick

    // 15 s slow retry when Unknown (fail-closed, keeps retrying).
    let mut slow_retry_interval = tokio::time::interval(Duration::from_secs(15));
    slow_retry_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    slow_retry_interval.tick().await; // consume immediate first tick

    let mut last_heartbeat_status: Option<String> = None;

    // Proactive JWT refresh timer — reconnects before relay JWT expires.
    let mut jwt_refresh_interval = tokio::time::interval(Duration::from_secs(600)); // check every 10min
    jwt_refresh_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    jwt_refresh_interval.tick().await; // consume immediate first tick

    loop {
        tokio::select! {
            // --- Network change from OS watcher ---
            Some(change) = net_rx.recv() => {
                log::info!(
                    "netwatcher event: +{:?} -{:?} (state={:?})",
                    change.added_ips, change.removed_ips, state
                );
                if !change.added_ips.is_empty() || !change.removed_ips.is_empty() {
                    handle_network_change(
                        &app, &control_plane, &credential_manager,
                        &mut state, &mut last_heartbeat_status,
                    ).await;
                }
            }

            // --- Commands from Tauri commands ---
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    RuntimeCommand::InitialClassify { reply } => {
                        log::info!("RuntimeCommand::InitialClassify received");
                        let result = handle_initial_classify(
                            &app, &control_plane, &credential_manager,
                            &mut state, &mut last_heartbeat_status,
                        ).await;
                        let _ = reply.send(result);
                    }
                    RuntimeCommand::SaveNetwork { label, reply } => {
                        let result = handle_save_network(
                            &app, &control_plane, &credential_manager,
                            &label, &mut state, &mut last_heartbeat_status,
                        ).await;
                        let _ = reply.send(result);
                    }
                    RuntimeCommand::ResumeWithCredentials => {
                        if state == RuntimeState::AuthExpired {
                            log::info!("Credentials updated — resuming classify");
                            handle_network_change(
                                &app, &control_plane, &credential_manager,
                                &mut state, &mut last_heartbeat_status,
                            ).await;
                        }
                    }
                    RuntimeCommand::Shutdown => {
                        log::info!("NetworkRuntime shutting down");
                        break;
                    }
                }
            }

            // --- 30 s periodic re-classify (Connected OR Unsafe) ---
            // Unsafe is included so exit-IP-only changes (VPN disconnect) get detected
            // even without an interface event. Without this, Unsafe would be permanent
            // until an unrelated network event fires.
            _ = periodic_interval.tick(), if state == RuntimeState::Connected || state == RuntimeState::Unsafe => {
                handle_periodic_classify(
                    &app, &control_plane, &credential_manager,
                    &mut state, &mut last_heartbeat_status,
                ).await;
            }

            // --- Proactive JWT refresh — reconnect before relay JWT expires ---
            _ = jwt_refresh_interval.tick(), if state == RuntimeState::Connected => {
                if let Ok(true) = credential_manager.relay_jwt_needs_refresh().await {
                    log::info!("Relay JWT nearing expiry — attempting proactive refresh");
                    // Only invalidate if refresh succeeds. If refresh fails (transient
                    // network issue), keep the existing connection alive — it's still
                    // working with the current JWT. We'll retry on the next tick.
                    match credential_manager.force_refresh().await {
                        Ok(_) => {
                            log::info!("JWT refreshed — reconnecting with fresh token");
                            if let Some(tunnel_state) = app.try_state::<TunnelState>() {
                                if let Ok(guard) = tunnel_state.handle.lock() {
                                    if let Some(ref handle) = *guard {
                                        handle.network_invalidate();
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            log::warn!("JWT refresh failed ({e:?}) — keeping current connection, will retry");
                        }
                    }
                }
            }

            // --- 15 s slow retry (Unknown / fail-closed only) ---
            _ = slow_retry_interval.tick(), if state == RuntimeState::Unknown => {
                handle_periodic_classify(
                    &app, &control_plane, &credential_manager,
                    &mut state, &mut last_heartbeat_status,
                ).await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// handle_initial_classify — pre-connect classification (no backoff, network is up)
// ---------------------------------------------------------------------------

async fn handle_initial_classify(
    app: &AppHandle,
    control_plane: &Arc<ControlPlaneClient>,
    credential_manager: &Arc<CredentialManager>,
    state: &mut RuntimeState,
    last_heartbeat: &mut Option<String>,
) -> Result<bool, String> {
    *state = RuntimeState::Classifying;
    let _ = app.emit("network:classifying", ());

    let jwt = credential_manager
        .get_api_jwt()
        .await
        .map_err(|e| format!("{e:?}"))?;
    let api_base = credential_manager.get_api_base_url().await;

    match control_plane
        .classify_ip(&api_base, &jwt, "pre_connect", None)
        .await
    {
        Ok(classify) if classify.is_safe => {
            *state = RuntimeState::Connected;
            if let Some(tunnel_state) = app.try_state::<TunnelState>() {
                tunnel_state.network_allowed.store(true, Ordering::Relaxed);
                tunnel_state.network_notify.notify_one();
            }
            let _ = app.emit("network:safe", &classify);
            send_heartbeat_if_changed(
                control_plane,
                credential_manager,
                &api_base,
                "connected",
                classify.matched_label.as_deref(),
                last_heartbeat,
            );
            Ok(true)
        }
        Ok(classify) => {
            *state = RuntimeState::Unsafe;
            let _ = app.emit(
                "network:unsafe",
                serde_json::json!({ "reason": classify.reason }),
            );
            Ok(false)
        }
        Err(e) => {
            *state = RuntimeState::Unknown;
            let _ = app.emit(
                "network:unknown",
                serde_json::json!({ "reason": format!("classification failed: {e}") }),
            );
            Err(format!("Classification failed: {e}"))
        }
    }
}

// ---------------------------------------------------------------------------
// handle_network_change — fail-closed classify with exponential backoff
// ---------------------------------------------------------------------------

async fn handle_network_change(
    app: &AppHandle,
    control_plane: &Arc<ControlPlaneClient>,
    credential_manager: &Arc<CredentialManager>,
    state: &mut RuntimeState,
    last_heartbeat: &mut Option<String>,
) {
    // 1. Fail closed — close the gate immediately.
    if let Some(tunnel_state) = app.try_state::<TunnelState>() {
        tunnel_state.network_allowed.store(false, Ordering::Relaxed);
        // 2. Invalidate active WebSocket (not kill) — reconnect loop will wait on gate.
        if let Ok(guard) = tunnel_state.handle.lock() {
            if let Some(ref handle) = *guard {
                handle.network_invalidate();
            }
        }
    }

    *state = RuntimeState::Classifying;
    let _ = app.emit("network:classifying", ());

    // 3. Classify with exponential backoff (1 s → 8 s, 30 s total).
    let backoff_config = backoff::ExponentialBackoffBuilder::new()
        .with_initial_interval(Duration::from_secs(1))
        .with_max_interval(Duration::from_secs(8))
        .with_max_elapsed_time(Some(Duration::from_secs(30)))
        .build();

    // Clone Arcs for the retry closure ('static lifetime required by backoff).
    let cp = control_plane.clone();
    let cm = credential_manager.clone();

    let result = backoff::future::retry(backoff_config, || {
        let cp = cp.clone();
        let cm = cm.clone();
        async move {
            let jwt = cm.get_api_jwt().await.map_err(|e| match e {
                RefreshError::AuthExpired => {
                    backoff::Error::permanent(ControlPlaneError::AuthExpired)
                }
                RefreshError::Transient => backoff::Error::transient(ControlPlaneError::Network(
                    "credential refresh failed".into(),
                )),
            })?;
            let api_base = cm.get_api_base_url().await;
            cp.classify_ip(&api_base, &jwt, "ip_changed", None)
                .await
                .map_err(|e| match e {
                    ControlPlaneError::Network(_) | ControlPlaneError::ServerError(_) => {
                        backoff::Error::transient(e)
                    }
                    _ => backoff::Error::permanent(e),
                })
        }
    })
    .await;

    // 4. Act on result — FAIL CLOSED on any error.
    let api_base = credential_manager.get_api_base_url().await;
    match result {
        Ok(classify) if classify.is_safe => {
            *state = RuntimeState::Connected;
            if let Some(tunnel_state) = app.try_state::<TunnelState>() {
                tunnel_state.network_allowed.store(true, Ordering::Relaxed);
                tunnel_state.network_notify.notify_one();
            }
            let _ = app.emit("network:safe", &classify);
            send_heartbeat_if_changed(
                control_plane,
                credential_manager,
                &api_base,
                "connected",
                classify.matched_label.as_deref(),
                last_heartbeat,
            );
        }
        Ok(classify) => {
            *state = RuntimeState::Unsafe;
            let _ = app.emit(
                "network:unsafe",
                serde_json::json!({ "reason": classify.reason }),
            );
            send_heartbeat_if_changed(
                control_plane,
                credential_manager,
                &api_base,
                "disconnected",
                None,
                last_heartbeat,
            );
        }
        Err(ControlPlaneError::AuthExpired) => {
            *state = RuntimeState::AuthExpired;
            let _ = app.emit("network:auth-expired", ());
            log::warn!("Runtime JWT expired — waiting for React re-auth");
        }
        Err(e) => {
            // Fail closed — gate stays blocked, 15 s slow retry will pick it up.
            *state = RuntimeState::Unknown;
            let _ = app.emit(
                "network:unknown",
                serde_json::json!({ "reason": format!("classification failed: {e}") }),
            );
            log::warn!("Network classification failed: {e} — fail-closed, retrying in 15s");
        }
    }
}

// ---------------------------------------------------------------------------
// handle_periodic_classify — single call, no backoff
// ---------------------------------------------------------------------------

async fn handle_periodic_classify(
    app: &AppHandle,
    control_plane: &Arc<ControlPlaneClient>,
    credential_manager: &Arc<CredentialManager>,
    state: &mut RuntimeState,
    last_heartbeat: &mut Option<String>,
) {
    let jwt = match credential_manager.get_api_jwt().await {
        Ok(j) => j,
        Err(_) => return, // Will retry next tick
    };
    let api_base = credential_manager.get_api_base_url().await;
    let event_type = if *state == RuntimeState::Unknown {
        "retry_classify"
    } else {
        "periodic_check"
    };

    match control_plane
        .classify_ip(&api_base, &jwt, event_type, None)
        .await
    {
        Ok(classify) if classify.is_safe => {
            if *state != RuntimeState::Connected {
                // Transition to safe — open gate, wake reconnect loop.
                *state = RuntimeState::Connected;
                if let Some(tunnel_state) = app.try_state::<TunnelState>() {
                    tunnel_state.network_allowed.store(true, Ordering::Relaxed);
                    tunnel_state.network_notify.notify_one();
                }
                let _ = app.emit("network:safe", &classify);
                send_heartbeat_if_changed(
                    control_plane,
                    credential_manager,
                    &api_base,
                    "connected",
                    classify.matched_label.as_deref(),
                    last_heartbeat,
                );
            }
        }
        Ok(classify) if !classify.is_safe && *state == RuntimeState::Connected => {
            // Was safe, now unsafe — kill switch.
            *state = RuntimeState::Unsafe;
            if let Some(tunnel_state) = app.try_state::<TunnelState>() {
                tunnel_state.network_allowed.store(false, Ordering::Relaxed);
                if let Ok(guard) = tunnel_state.handle.lock() {
                    if let Some(ref handle) = *guard {
                        handle.network_invalidate();
                    }
                }
            }
            let _ = app.emit(
                "network:unsafe",
                serde_json::json!({ "reason": classify.reason }),
            );
            send_heartbeat_if_changed(
                control_plane,
                credential_manager,
                &api_base,
                "disconnected",
                None,
                last_heartbeat,
            );
        }
        Ok(_) => {} // No state change
        Err(ControlPlaneError::AuthExpired) => {
            *state = RuntimeState::AuthExpired;
            let _ = app.emit("network:auth-expired", ());
        }
        Err(_) => {} // Transient failure — retry next tick
    }
}

// ---------------------------------------------------------------------------
// handle_save_network — classify with save_label, return result to caller
// ---------------------------------------------------------------------------

async fn handle_save_network(
    app: &AppHandle,
    control_plane: &Arc<ControlPlaneClient>,
    credential_manager: &Arc<CredentialManager>,
    label: &str,
    state: &mut RuntimeState,
    last_heartbeat: &mut Option<String>,
) -> Result<ClassifyResult, String> {
    let jwt = credential_manager
        .get_api_jwt()
        .await
        .map_err(|e| format!("{e:?}"))?;
    let api_base = credential_manager.get_api_base_url().await;

    let classify = control_plane
        .classify_ip(&api_base, &jwt, "save_network", Some(label))
        .await
        .map_err(|e| format!("{e}"))?;

    if classify.is_safe {
        *state = RuntimeState::Connected;
        if let Some(tunnel_state) = app.try_state::<TunnelState>() {
            tunnel_state.network_allowed.store(true, Ordering::Relaxed);
            tunnel_state.network_notify.notify_one();
        }
        let _ = app.emit("network:safe", &classify);
        send_heartbeat_if_changed(
            control_plane,
            credential_manager,
            &api_base,
            "connected",
            classify.matched_label.as_deref(),
            last_heartbeat,
        );
    }

    Ok(classify)
}

// ---------------------------------------------------------------------------
// send_heartbeat_if_changed — dedup by last status, send in background
// ---------------------------------------------------------------------------

fn send_heartbeat_if_changed(
    control_plane: &Arc<ControlPlaneClient>,
    credential_manager: &Arc<CredentialManager>,
    api_base: &str,
    status: &str,
    label: Option<&str>,
    last_status: &mut Option<String>,
) {
    // Deduplicate — don't spam the same status.
    if last_status.as_deref() == Some(status) {
        return;
    }
    *last_status = Some(status.to_string());

    let control_plane = control_plane.clone();
    let credential_manager = credential_manager.clone();
    let api_base = api_base.to_string();
    let status = status.to_string();
    let label = label.map(str::to_string);

    // Heartbeats should never block the connect/classify critical path.
    // Classification and gate state are already decided above; heartbeat is
    // backend bookkeeping and can complete asynchronously.
    tauri::async_runtime::spawn(async move {
        let jwt = match credential_manager.get_api_jwt().await {
            Ok(j) => j,
            Err(e) => {
                log::warn!("Skipping heartbeat JWT fetch ({e:?})");
                return;
            }
        };

        if status == "connected" {
            // Retry up to 3 times — connected heartbeat has critical side effects
            // (schedule reconciliation, Redis status update, etc.).
            for attempt in 0..3 {
                match control_plane
                    .send_heartbeat(&api_base, &jwt, &status, label.as_deref())
                    .await
                {
                    Ok(()) => return,
                    Err(e) => {
                        log::warn!("Connected heartbeat attempt {attempt} failed: {e}");
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                }
            }
            log::error!("Connected heartbeat failed after 3 attempts");
        } else {
            let _ = control_plane
                .send_heartbeat(&api_base, &jwt, &status, label.as_deref())
                .await;
        }
    });
}
