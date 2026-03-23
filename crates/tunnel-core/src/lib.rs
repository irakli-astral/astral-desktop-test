mod client;
pub mod credentials;
mod dialer;
mod ws_stream;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot, Notify};
use tokio_util::sync::CancellationToken;

pub use credentials::RefreshError;

/// Configuration for connecting to the relay server.
#[derive(Clone, Debug)]
pub struct TunnelConfig {
    /// WebSocket URL of the relay (e.g., "wss://relay.astral.com/tunnel")
    pub relay_url: String,
    /// Relay JWT for authentication (sent as Bearer token). Short-lived (1h).
    pub relay_jwt: String,
}

/// Configuration for the reconnect loop.
#[derive(Clone, Debug)]
pub struct ReconnectConfig {
    /// Base delay for exponential backoff (default: 1 second)
    pub base_delay_secs: u64,
    /// Maximum delay cap (default: 60 seconds)
    pub max_delay_secs: u64,
}

impl Default for ReconnectConfig {
    fn default() -> Self {
        Self {
            base_delay_secs: 1,
            max_delay_secs: 60,
        }
    }
}

/// Events emitted by the tunnel for the UI to display.
#[derive(Clone, Debug)]
pub enum TunnelEvent {
    Connecting,
    Connected,
    StreamOpened { target: String },
    StreamClosed { target: String },
    Disconnected { reason: String },
    Error { message: String },
    Reconnecting { attempt: u32, delay_ms: u64 },
    AuthExpired,
}

/// Live statistics from the tunnel, updated atomically by stream tasks.
#[derive(Debug)]
pub struct TunnelStats {
    pub bytes_up: AtomicU64,
    pub bytes_down: AtomicU64,
    pub active_streams: AtomicU64,
    pub total_streams: AtomicU64,
}

impl TunnelStats {
    fn new() -> Self {
        Self {
            bytes_up: AtomicU64::new(0),
            bytes_down: AtomicU64::new(0),
            active_streams: AtomicU64::new(0),
            total_streams: AtomicU64::new(0),
        }
    }
}

/// Snapshot of tunnel stats for serialization to the frontend.
#[derive(Clone, Debug, serde::Serialize)]
pub struct TunnelStatsSnapshot {
    pub bytes_up: u64,
    pub bytes_down: u64,
    pub active_streams: u64,
    pub total_streams: u64,
}

impl TunnelStats {
    pub fn snapshot(&self) -> TunnelStatsSnapshot {
        TunnelStatsSnapshot {
            bytes_up: self.bytes_up.load(Ordering::Relaxed),
            bytes_down: self.bytes_down.load(Ordering::Relaxed),
            active_streams: self.active_streams.load(Ordering::Relaxed),
            total_streams: self.total_streams.load(Ordering::Relaxed),
        }
    }
}

/// Handle to a running tunnel. Call `stop()` for graceful shutdown.
pub struct TunnelHandle {
    cancel: CancellationToken,
    pub stats: Arc<TunnelStats>,
    /// Shared sender for network invalidation. Each reconnect iteration creates
    /// a fresh oneshot pair and stores the sender here. `network_invalidate()`
    /// takes the current sender and fires it, causing the active connection to
    /// drop without cancelling the reconnect loop.
    network_invalidate_tx: Arc<Mutex<Option<oneshot::Sender<()>>>>,
}

impl TunnelHandle {
    pub fn stop(&self) {
        self.cancel.cancel();
    }

    pub fn is_running(&self) -> bool {
        !self.cancel.is_cancelled()
    }

    /// Drop the current WebSocket connection so the reconnect loop can
    /// re-evaluate the network gate immediately. Does NOT cancel the tunnel.
    pub fn network_invalidate(&self) {
        if let Ok(mut guard) = self.network_invalidate_tx.lock() {
            if let Some(tx) = guard.take() {
                let _ = tx.send(());
            }
        }
    }
}

/// Result of the initial connection attempt (before handing off to reconnect loop).
#[derive(Clone, Debug)]
pub enum InitialOutcome {
    /// Tunnel connected successfully.
    Connected,
    /// Connection failed with a reason.
    Failed { reason: String },
    /// Auth was rejected by relay (401/403).
    AuthExpired,
    /// Timed out waiting for the initial connection.
    Timeout,
}

/// Start the tunnel with reconnection support.
///
/// - `network_allowed`: shared AtomicBool checked before each reconnect attempt.
/// - `network_notify`: Notify pulsed when `network_allowed` changes — wakes the
///   reconnect loop instantly instead of polling.
pub fn start(
    config: TunnelConfig,
    reconnect_config: ReconnectConfig,
    network_allowed: Arc<AtomicBool>,
    network_notify: Arc<Notify>,
    get_jwt: impl Fn(
            bool,
        )
            -> futures_util::future::BoxFuture<'static, Result<String, credentials::RefreshError>>
        + Send
        + Sync
        + 'static,
) -> (TunnelHandle, mpsc::Receiver<TunnelEvent>) {
    let cancel = CancellationToken::new();
    let stats = Arc::new(TunnelStats::new());
    let (event_tx, event_rx) = mpsc::channel(64);

    let invalidate_tx: Arc<Mutex<Option<oneshot::Sender<()>>>> = Arc::new(Mutex::new(None));

    let handle = TunnelHandle {
        cancel: cancel.clone(),
        stats: stats.clone(),
        network_invalidate_tx: invalidate_tx.clone(),
    };

    tokio::spawn(client::run(
        config,
        reconnect_config,
        cancel,
        stats,
        event_tx,
        network_allowed,
        network_notify,
        get_jwt,
        invalidate_tx,
    ));

    (handle, event_rx)
}

/// Start the tunnel and wait for the first meaningful outcome before returning.
///
/// Returns `(handle, event_rx, outcome)` where outcome tells the caller whether
/// the initial connection succeeded, failed, or timed out. The tunnel task
/// continues running in the background for reconnects regardless of outcome.
///
/// Command-level timeout (default 20s) acts as a second guard above the
/// 15s WebSocket connect timeout in client.rs.
pub async fn start_and_wait(
    config: TunnelConfig,
    reconnect_config: ReconnectConfig,
    network_allowed: Arc<AtomicBool>,
    network_notify: Arc<Notify>,
    get_jwt: impl Fn(
            bool,
        )
            -> futures_util::future::BoxFuture<'static, Result<String, credentials::RefreshError>>
        + Send
        + Sync
        + 'static,
) -> (TunnelHandle, mpsc::Receiver<TunnelEvent>, InitialOutcome) {
    let cancel = CancellationToken::new();
    let stats = Arc::new(TunnelStats::new());
    let (event_tx, event_rx) = mpsc::channel(64);

    let invalidate_tx: Arc<Mutex<Option<oneshot::Sender<()>>>> = Arc::new(Mutex::new(None));

    let handle = TunnelHandle {
        cancel: cancel.clone(),
        stats: stats.clone(),
        network_invalidate_tx: invalidate_tx.clone(),
    };

    // Use a oneshot channel to receive the first meaningful outcome
    let (outcome_tx, outcome_rx) = tokio::sync::oneshot::channel::<InitialOutcome>();

    // Wrap event_tx so we can intercept the first Connected/Disconnected/Error/AuthExpired
    let (intercept_tx, mut intercept_rx) = mpsc::channel::<TunnelEvent>(64);

    // Spawn the tunnel task with the intercept channel
    tokio::spawn(client::run(
        config,
        reconnect_config,
        cancel,
        stats,
        intercept_tx,
        network_allowed,
        network_notify,
        get_jwt,
        invalidate_tx,
    ));

    // Spawn a forwarder that intercepts the first meaningful event
    let forward_event_tx = event_tx.clone();
    tokio::spawn(async move {
        let mut outcome_tx = Some(outcome_tx);
        while let Some(event) = intercept_rx.recv().await {
            // Check if this is the first meaningful outcome
            if let Some(tx) = outcome_tx.take() {
                let outcome = match &event {
                    TunnelEvent::Connected => Some(InitialOutcome::Connected),
                    TunnelEvent::Disconnected { reason } => Some(InitialOutcome::Failed {
                        reason: reason.clone(),
                    }),
                    TunnelEvent::AuthExpired => Some(InitialOutcome::AuthExpired),
                    TunnelEvent::Error { message } => Some(InitialOutcome::Failed {
                        reason: message.clone(),
                    }),
                    _ => None, // Connecting, Reconnecting — not meaningful yet
                };
                if let Some(o) = outcome {
                    let _ = tx.send(o);
                } else {
                    // Not meaningful yet — put tx back
                    outcome_tx = Some(tx);
                }
            }
            // Forward all events to the real channel
            let _ = forward_event_tx.send(event).await;
        }
        // If tunnel task ended without a meaningful event, signal failure
        if let Some(tx) = outcome_tx {
            let _ = tx.send(InitialOutcome::Failed {
                reason: "Tunnel task ended unexpectedly".to_string(),
            });
        }
    });

    // Wait for the first outcome with a command-level timeout (20s)
    const COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
    let outcome = tokio::select! {
        result = outcome_rx => {
            match result {
                Ok(o) => o,
                Err(_) => InitialOutcome::Failed {
                    reason: "Tunnel task ended before connecting".to_string(),
                },
            }
        }
        _ = tokio::time::sleep(COMMAND_TIMEOUT) => {
            InitialOutcome::Timeout
        }
    };

    // Drop the original event_tx so only the forwarder holds it
    drop(event_tx);

    (handle, event_rx, outcome)
}
