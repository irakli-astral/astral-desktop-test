use tokio::sync::mpsc;

/// A network interface change event, unified across platforms.
pub struct NetworkChange {
    pub added_ips: Vec<String>,
    pub removed_ips: Vec<String>,
}

/// Channel type for receiving network change events.
pub type NetworkEventRx = mpsc::Receiver<NetworkChange>;

/// Start the platform-appropriate network watcher.
///
/// Returns (receiver, boxed handle that must stay alive).
/// The handle is RAII — dropping it stops notifications.
///
/// - **macOS**: Uses `netwatcher` crate with native platform APIs (instant detection).
/// - **Other platforms**: Uses `if-watch` crate with native APIs (Linux: netlink, Windows: NotifyIpInterfaceChange).
pub fn start() -> Result<(NetworkEventRx, Box<dyn std::any::Any + Send>), String> {
    #[cfg(target_os = "macos")]
    {
        start_netwatcher()
    }

    #[cfg(not(target_os = "macos"))]
    {
        start_if_watch()
    }
}

// =============================================================================
// macOS: netwatcher (instant native detection, no polling)
// =============================================================================

#[cfg(target_os = "macos")]
fn start_netwatcher() -> Result<(NetworkEventRx, Box<dyn std::any::Any + Send>), String> {
    let (tx, rx) = mpsc::channel::<NetworkChange>(16);
    let mut is_first = true;

    let handle = netwatcher::watch_interfaces(move |update| {
        // Skip the first callback — it's the initial state dump, not a change.
        if is_first {
            is_first = false;
            return;
        }

        let mut added = Vec::new();
        let mut removed = Vec::new();

        // New interfaces
        for idx in &update.diff.added {
            if let Some(iface) = update.interfaces.get(idx) {
                for ip_record in &iface.ips {
                    added.push(ip_record.ip.to_string());
                }
            }
        }

        // Removed interfaces
        for idx in &update.diff.removed {
            removed.push(format!("interface-{idx}"));
        }

        // Modified interfaces — extract their IP-level changes
        for diff in update.diff.modified.values() {
            for ip_record in &diff.addrs_added {
                added.push(ip_record.ip.to_string());
            }
            for ip_record in &diff.addrs_removed {
                removed.push(ip_record.ip.to_string());
            }
        }

        // Only send if there's an actual IP-level change
        if !added.is_empty() || !removed.is_empty() {
            log::info!(
                "Network change (macOS/netwatcher): +{} -{} IPs",
                added.len(),
                removed.len()
            );
            // try_send: safe from non-tokio thread, drops if channel full (events are idempotent)
            let _ = tx.try_send(NetworkChange {
                added_ips: added,
                removed_ips: removed,
            });
        }
    })
    .map_err(|e| format!("Failed to start netwatcher: {e:?}"))?;

    log::info!("Network watcher started (macOS/netwatcher — native instant detection)");
    Ok((rx, Box::new(handle)))
}

// =============================================================================
// Non-macOS: if-watch with debounce (native events on Linux/Windows)
// =============================================================================

#[cfg(not(target_os = "macos"))]
fn start_if_watch() -> Result<(NetworkEventRx, Box<dyn std::any::Any + Send>), String> {
    use futures::StreamExt;
    use if_watch::IfEvent;
    use std::time::Duration;
    use tokio::time::Instant;

    const DEBOUNCE_MS: u64 = 500;

    let (tx, rx) = mpsc::channel::<NetworkChange>(16);

    let join_handle = tauri::async_runtime::spawn(async move {
        let watcher = match if_watch::tokio::IfWatcher::new() {
            Ok(w) => w,
            Err(e) => {
                log::error!("Failed to create if-watch watcher: {e}");
                return;
            }
        };

        let mut stream = watcher.fuse();
        let debounce_duration = Duration::from_millis(DEBOUNCE_MS);
        let mut pending_added: Vec<String> = Vec::new();
        let mut pending_removed: Vec<String> = Vec::new();
        let mut deadline = Instant::now();
        let mut has_pending = false;

        loop {
            if has_pending {
                tokio::select! {
                    result = stream.select_next_some() => {
                        match result {
                            Ok(event) => {
                                match &event {
                                    IfEvent::Up(net) => {
                                        log::info!("Network change (debouncing): up {net}");
                                        pending_added.push(net.addr().to_string());
                                    }
                                    IfEvent::Down(net) => {
                                        log::info!("Network change (debouncing): down {net}");
                                        pending_removed.push(net.addr().to_string());
                                    }
                                }
                                deadline = Instant::now() + debounce_duration;
                            }
                            Err(e) => {
                                log::error!("Network watcher error: {e}");
                            }
                        }
                    }
                    _ = tokio::time::sleep_until(deadline) => {
                        log::info!(
                            "Network debounce fired: +{} -{} IPs",
                            pending_added.len(),
                            pending_removed.len()
                        );
                        let _ = tx.try_send(NetworkChange {
                            added_ips: std::mem::take(&mut pending_added),
                            removed_ips: std::mem::take(&mut pending_removed),
                        });
                        has_pending = false;
                    }
                }
            } else {
                match stream.select_next_some().await {
                    Ok(event) => {
                        match &event {
                            IfEvent::Up(net) => {
                                log::info!("Network change detected: up {net}");
                                pending_added.push(net.addr().to_string());
                            }
                            IfEvent::Down(net) => {
                                log::info!("Network change detected: down {net}");
                                pending_removed.push(net.addr().to_string());
                            }
                        }
                        deadline = Instant::now() + debounce_duration;
                        has_pending = true;
                    }
                    Err(e) => {
                        log::error!("Network watcher error: {e}");
                    }
                }
            }
        }
    });

    log::info!("Network watcher started (if-watch — native events with 500ms debounce)");
    Ok((rx, Box::new(join_handle)))
}
