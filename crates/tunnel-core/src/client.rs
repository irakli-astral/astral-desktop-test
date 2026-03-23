use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::future;
use rand::Rng;
use tokio::sync::{mpsc, oneshot, Notify};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use yamux::{Connection, Mode};

use crate::credentials::RefreshError;
use crate::dialer;
use crate::ws_stream::WsStream;
use crate::{ReconnectConfig, TunnelConfig, TunnelEvent, TunnelStats};

/// Possible outcomes of a single connection attempt.
pub enum ConnectOutcome {
    /// User-initiated disconnect — do NOT reconnect.
    UserDisconnect,
    /// Connection failed or dropped. `was_connected` is true if we reached
    /// TunnelEvent::Connected (stable connection established), false if we failed
    /// during handshake/DNS/etc.
    Disconnected { reason: String, was_connected: bool },
    /// Auth rejected by relay (HTTP 401/403 on WebSocket upgrade).
    AuthRejected,
}

/// Single connection attempt: connect to relay, run yamux driver, return when done.
async fn connect_once(
    config: &TunnelConfig,
    cancel: &CancellationToken,
    stats: &Arc<TunnelStats>,
    event_tx: &mpsc::Sender<TunnelEvent>,
    mut invalidate_rx: oneshot::Receiver<()>,
) -> ConnectOutcome {
    // Validate relay URL scheme (require wss:// in production)
    if let Err(e) = validate_relay_url(&config.relay_url) {
        error!(error = %e, "Relay URL validation failed");
        let _ = event_tx.send(TunnelEvent::Error { message: e }).await;
        return ConnectOutcome::Disconnected {
            reason: "Invalid relay URL".to_string(),
            was_connected: false,
        };
    }

    // Build WebSocket request with auth header
    let request = match build_ws_request(config) {
        Ok(r) => r,
        Err(e) => {
            error!(error = %e, "Failed to build WebSocket request");
            let _ = event_tx
                .send(TunnelEvent::Error {
                    message: format!("Invalid relay URL: {e}"),
                })
                .await;
            return ConnectOutcome::Disconnected {
                reason: format!("Invalid relay URL: {e}"),
                was_connected: false,
            };
        }
    };

    let _ = event_tx.send(TunnelEvent::Connecting).await;

    // Connect to the relay via WebSocket (with 15s timeout to prevent indefinite hang)
    const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

    let ws_result = tokio::select! {
        result = tokio_tungstenite::connect_async(request) => result,
        _ = tokio::time::sleep(CONNECT_TIMEOUT) => {
            error!("Timed out connecting to relay ({}s)", CONNECT_TIMEOUT.as_secs());
            let _ = event_tx
                .send(TunnelEvent::Error {
                    message: "Timed out connecting to relay".to_string(),
                })
                .await;
            return ConnectOutcome::Disconnected {
                reason: "Timed out connecting to relay".to_string(),
                was_connected: false,
            };
        }
        _ = cancel.cancelled() => {
            info!("Tunnel shutdown requested during connect");
            return ConnectOutcome::UserDisconnect;
        }
    };

    let (ws_stream, _response) = match ws_result {
        Ok(pair) => pair,
        Err(e) => {
            let msg = format!("{e}");
            error!(error = %e, "Failed to connect to relay");
            // Check for auth rejection (HTTP 401/403)
            if msg.contains("401") || msg.contains("403") {
                return ConnectOutcome::AuthRejected;
            }
            let _ = event_tx
                .send(TunnelEvent::Error {
                    message: format!("Connection failed: {e}"),
                })
                .await;
            return ConnectOutcome::Disconnected {
                reason: format!("Connection failed: {e}"),
                was_connected: false,
            };
        }
    };

    info!(relay_url = %config.relay_url, "Connected to relay");
    let _ = event_tx.send(TunnelEvent::Connected).await;

    // Reset stats for this connection
    stats
        .active_streams
        .store(0, std::sync::atomic::Ordering::Relaxed);

    // Wrap WebSocket in our AsyncRead/AsyncWrite adapter
    let ws = WsStream::new(ws_stream);

    // Create yamux connection in CLIENT mode
    let yamux_config = yamux::Config::default();
    let mut connection = Connection::new(ws, yamux_config, Mode::Client);

    // Keepalive wake timer
    let mut keepalive = tokio::time::interval(std::time::Duration::from_secs(10));
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    keepalive.tick().await; // consume immediate first tick

    // Driver loop
    loop {
        tokio::select! {
            inbound = future::poll_fn(|cx| connection.poll_next_inbound(cx)) => {
                match inbound {
                    Some(Ok(stream)) => {
                        let stats = stats.clone();
                        let event_tx = event_tx.clone();
                        tokio::spawn(dialer::handle_stream(stream, stats, event_tx));
                    }
                    Some(Err(e)) => {
                        warn!(error = %e, "yamux connection error");
                        return ConnectOutcome::Disconnected {
                            reason: format!("yamux error: {e}"),
                            was_connected: true,
                        };
                    }
                    None => {
                        info!("yamux connection closed by relay (EOF)");
                        return ConnectOutcome::Disconnected {
                            reason: "Relay closed connection".to_string(),
                            was_connected: true,
                        };
                    }
                }
            }
            _ = keepalive.tick() => {}
            _ = &mut invalidate_rx => {
                info!("Network invalidated — dropping connection");
                break;
            }
            _ = cancel.cancelled() => {
                info!("Tunnel shutdown requested");
                return ConnectOutcome::UserDisconnect;
            }
        }
    }

    ConnectOutcome::Disconnected {
        reason: "Network invalidated".to_string(),
        was_connected: true,
    }
}

/// Main tunnel entry point. Wraps connect_once() in a reconnect loop with
/// exponential backoff + full jitter. Checks network_allowed gate before each attempt.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    config: TunnelConfig,
    reconnect_config: ReconnectConfig,
    cancel: CancellationToken,
    stats: Arc<TunnelStats>,
    event_tx: mpsc::Sender<TunnelEvent>,
    network_allowed: Arc<AtomicBool>,
    network_notify: Arc<Notify>,
    get_jwt: impl Fn(bool) -> futures_util::future::BoxFuture<'static, Result<String, RefreshError>>
        + Send
        + Sync
        + 'static,
    network_invalidate_tx: Arc<Mutex<Option<oneshot::Sender<()>>>>,
) {
    let mut attempt: u32 = 0;
    let mut current_config = config;

    loop {
        // Create a fresh invalidate channel for this iteration
        let (inv_tx, inv_rx) = oneshot::channel::<()>();
        {
            let mut guard = network_invalidate_tx
                .lock()
                .expect("invalidate mutex poisoned");
            *guard = Some(inv_tx);
        }

        let outcome = connect_once(&current_config, &cancel, &stats, &event_tx, inv_rx).await;

        match outcome {
            ConnectOutcome::UserDisconnect => {
                let _ = event_tx
                    .send(TunnelEvent::Disconnected {
                        reason: "Disconnected by user".to_string(),
                    })
                    .await;
                return;
            }
            ConnectOutcome::AuthRejected => match (get_jwt)(true).await {
                Ok(fresh_jwt) => {
                    current_config.relay_jwt = fresh_jwt;
                    // Create a fresh invalidate channel for the auth-retry attempt
                    let (retry_inv_tx, retry_inv_rx) = oneshot::channel::<()>();
                    {
                        let mut guard = network_invalidate_tx
                            .lock()
                            .expect("invalidate mutex poisoned");
                        *guard = Some(retry_inv_tx);
                    }
                    let retry_outcome =
                        connect_once(&current_config, &cancel, &stats, &event_tx, retry_inv_rx)
                            .await;
                    match retry_outcome {
                        ConnectOutcome::UserDisconnect => {
                            let _ = event_tx
                                .send(TunnelEvent::Disconnected {
                                    reason: "Disconnected by user".to_string(),
                                })
                                .await;
                            return;
                        }
                        ConnectOutcome::AuthRejected => {
                            let _ = event_tx.send(TunnelEvent::AuthExpired).await;
                            return;
                        }
                        ConnectOutcome::Disconnected {
                            reason,
                            was_connected,
                        } => {
                            let _ = event_tx.send(TunnelEvent::Disconnected { reason }).await;
                            if was_connected {
                                attempt = 0;
                            }
                        }
                    }
                }
                Err(RefreshError::AuthExpired) => {
                    let _ = event_tx.send(TunnelEvent::AuthExpired).await;
                    return;
                }
                Err(RefreshError::Transient) => {
                    warn!("JWT refresh failed transiently after relay 401 — will retry");
                }
            },
            ConnectOutcome::Disconnected {
                reason,
                was_connected,
            } => {
                let _ = event_tx
                    .send(TunnelEvent::Disconnected {
                        reason: reason.clone(),
                    })
                    .await;
                // Network invalidation: skip backoff, go straight to network gate
                if reason == "Network invalidated" {
                    continue;
                }
                if was_connected {
                    attempt = 0;
                }
            }
        }

        // Backoff with full jitter
        let base = reconnect_config.base_delay_secs as f64;
        let cap = reconnect_config.max_delay_secs as f64;
        let max_delay = cap.min(base * 2.0_f64.powi(attempt as i32));
        let delay_ms = (rand::thread_rng().gen::<f64>() * max_delay * 1000.0) as u64;

        attempt = attempt.saturating_add(1);
        let _ = event_tx
            .send(TunnelEvent::Reconnecting { attempt, delay_ms })
            .await;

        // Sleep with cancellation check
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(delay_ms)) => {}
            _ = cancel.cancelled() => {
                let _ = event_tx.send(TunnelEvent::Disconnected {
                    reason: "Disconnected by user".to_string(),
                }).await;
                return;
            }
        }

        // Check network gate — wait for Rust NetworkRuntime to classify network as safe.
        // The runtime owns the gate: it classifies with backoff (30s immediate + 15s slow retry)
        // and opens the gate only when classification succeeds and the network is safe.
        // No timeout — fail-closed by design. The gate stays closed until the runtime opens it
        // or the tunnel is cancelled (user disconnect / app shutdown).
        if !network_allowed.load(Ordering::Relaxed) {
            info!("Network gate closed — waiting for runtime classification");
            loop {
                tokio::select! {
                    _ = network_notify.notified() => {
                        if network_allowed.load(Ordering::Relaxed) {
                            info!("Network gate opened — safe to reconnect");
                            break;
                        }
                    }
                    _ = cancel.cancelled() => {
                        let _ = event_tx.send(TunnelEvent::Disconnected {
                            reason: "Disconnected by user".to_string(),
                        }).await;
                        return;
                    }
                }
            }
        }

        // Get (possibly refreshed) JWT before reconnecting
        match (get_jwt)(false).await {
            Ok(jwt) => {
                current_config.relay_jwt = jwt;
            }
            Err(RefreshError::AuthExpired) => {
                let _ = event_tx.send(TunnelEvent::AuthExpired).await;
                return;
            }
            Err(RefreshError::Transient) => {
                warn!("JWT refresh failed transiently — will retry after backoff");
                continue;
            }
        }
    }
}

/// Build the WebSocket connection request with Authorization header.
fn build_ws_request(
    config: &TunnelConfig,
) -> Result<tokio_tungstenite::tungstenite::http::Request<()>, anyhow::Error> {
    let request = tokio_tungstenite::tungstenite::http::Request::builder()
        .uri(&config.relay_url)
        .header("Authorization", format!("Bearer {}", config.relay_jwt))
        .header("Host", extract_host(&config.relay_url).unwrap_or_default())
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header(
            "Sec-WebSocket-Key",
            tokio_tungstenite::tungstenite::handshake::client::generate_key(),
        )
        .body(())?;

    Ok(request)
}

/// Validate the relay URL scheme. Production requires wss://.
/// ws:// is only allowed when TUNNEL_ALLOW_INSECURE=1 (for local dev).
fn validate_relay_url(url: &str) -> Result<(), String> {
    if url.starts_with("wss://") {
        return Ok(());
    }
    if url.starts_with("ws://") {
        if std::env::var("TUNNEL_ALLOW_INSECURE").as_deref() == Ok("1") {
            warn!("Using insecure ws:// relay (TUNNEL_ALLOW_INSECURE=1)");
            return Ok(());
        }
        return Err(
            "Insecure ws:// relay URL rejected. Use wss:// or set TUNNEL_ALLOW_INSECURE=1 for local dev."
                .to_string(),
        );
    }
    Err(format!(
        "Invalid relay URL scheme: {url}. Must start with wss:// or ws://"
    ))
}

/// Extract host from a URL string for the Host header.
fn extract_host(url: &str) -> Option<String> {
    // Simple extraction: strip ws:// or wss://, take up to first /
    let stripped = url
        .strip_prefix("wss://")
        .or_else(|| url.strip_prefix("ws://"))?;
    let host = stripped.split('/').next()?;
    Some(host.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_host() {
        assert_eq!(
            extract_host("ws://localhost:3100/tunnel"),
            Some("localhost:3100".to_string())
        );
        assert_eq!(
            extract_host("wss://relay.astral.com/tunnel"),
            Some("relay.astral.com".to_string())
        );
        assert_eq!(extract_host("http://example.com"), None);
        assert_eq!(extract_host(""), None);
    }

    #[test]
    fn test_validate_relay_url_wss() {
        assert!(validate_relay_url("wss://relay.astral.com/tunnel").is_ok());
    }

    #[test]
    fn test_validate_relay_url_ws_rejected() {
        // Ensure env var is NOT set for this test
        std::env::remove_var("TUNNEL_ALLOW_INSECURE");
        assert!(validate_relay_url("ws://localhost:3100/tunnel").is_err());
    }

    #[test]
    fn test_validate_relay_url_ws_allowed_with_env() {
        std::env::set_var("TUNNEL_ALLOW_INSECURE", "1");
        assert!(validate_relay_url("ws://localhost:3100/tunnel").is_ok());
        std::env::remove_var("TUNNEL_ALLOW_INSECURE");
    }

    #[test]
    fn test_validate_relay_url_invalid_scheme() {
        assert!(validate_relay_url("http://example.com").is_err());
        assert!(validate_relay_url("").is_err());
    }

    #[test]
    fn test_build_ws_request() {
        let config = TunnelConfig {
            relay_url: "ws://localhost:3100/tunnel".to_string(),
            relay_jwt: "test_jwt".to_string(),
        };
        let request = build_ws_request(&config).unwrap();
        assert_eq!(
            request.headers().get("Authorization").unwrap(),
            "Bearer test_jwt"
        );
        assert_eq!(request.headers().get("Host").unwrap(), "localhost:3100");
    }
}
