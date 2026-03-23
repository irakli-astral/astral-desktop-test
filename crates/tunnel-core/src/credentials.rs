use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

/// HTTP timeout for refresh requests — prevents control-plane stalls from
/// making reconnect look frozen.
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

/// JWT refresh buffer — refresh when less than 60 seconds until expiry.
const JWT_REFRESH_BUFFER_SECS: u64 = 60;

/// Error type for credential refresh operations.
/// The reconnect loop uses this to distinguish "retry later" from "give up".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshError {
    /// Refresh returned 401 — device token family is dead, user must re-login.
    /// Reconnect loop should emit TunnelEvent::AuthExpired and stop.
    AuthExpired,
    /// Network error, 5xx, parse failure, storage error — transient problem.
    /// Reconnect loop should backoff and retry, NOT treat as auth expiry.
    Transient,
}

#[derive(Debug, Deserialize)]
struct RefreshResponse {
    device_token: String,
    relay_jwt: String,
    relay_jwt_expires_at: u64,
    #[serde(default)]
    desktop_api_jwt: Option<String>,
    #[serde(default)]
    desktop_api_jwt_expires_at: Option<u64>,
}

/// Stored credentials — serialized to/from a JSON file by the Tauri layer.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StoredCredentials {
    pub device_token: String,
    pub relay_url: String,
    pub api_base_url: String,
}

/// Callback trait for persisting credentials to disk.
/// Implemented by the Tauri layer (file-based store) — keeps tunnel-core
/// independent of Tauri's AppHandle.
pub trait CredentialStore: Send + Sync {
    fn load(&self) -> Result<StoredCredentials, String>;
    fn save(&self, creds: &StoredCredentials) -> Result<(), String>;
    fn delete(&self) -> Result<(), String>;
    fn exists(&self) -> bool;
}

/// Manages device token (persisted via CredentialStore) and relay JWT (in-memory cache).
/// Thread-safe — all mutable state behind a Mutex.
/// IMPORTANT: `refresh_lock` serializes all token refresh operations to prevent
/// concurrent rotations from revoking the entire device token family.
pub struct CredentialManager {
    inner: Mutex<CredentialInner>,
    /// Serializes refresh_jwt calls — two concurrent refreshes with the same
    /// device_token would cause the server to revoke the entire token family.
    refresh_lock: Mutex<()>,
    store: Box<dyn CredentialStore>,
    http: reqwest::Client,
}

struct CredentialInner {
    /// Cached relay JWT (short-lived, 1 hour)
    relay_jwt: Option<String>,
    /// Unix timestamp when relay_jwt expires (server-provided, Rust never parses JWT)
    relay_jwt_expires_at: u64,
    /// Cached desktop API JWT for control plane calls (classify, heartbeat)
    desktop_api_jwt: Option<String>,
    /// Unix timestamp when desktop_api_jwt expires
    desktop_api_jwt_expires_at: u64,
    /// API base URL for refresh calls
    api_base_url: String,
    /// Optional Vercel deployment protection bypass token for preview/dev refresh calls.
    vercel_bypass: Option<String>,
}

impl CredentialManager {
    /// Create a new credential manager with the given persistent store.
    pub fn new(api_base_url: String, store: Box<dyn CredentialStore>) -> Self {
        Self {
            inner: Mutex::new(CredentialInner {
                relay_jwt: None,
                relay_jwt_expires_at: 0,
                desktop_api_jwt: None,
                desktop_api_jwt_expires_at: 0,
                api_base_url,
                vercel_bypass: None,
            }),
            refresh_lock: Mutex::new(()),
            store,
            http: reqwest::Client::builder()
                .timeout(HTTP_TIMEOUT)
                .connect_timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
        }
    }

    /// Initialize with credentials from first login (JS-initiated).
    /// Persists device_token + URLs to file, caches JWT in memory.
    #[allow(clippy::too_many_arguments)]
    pub async fn initialize(
        &self,
        device_token: &str,
        relay_jwt: &str,
        relay_jwt_expires_at: u64,
        relay_url: &str,
        api_base_url: &str,
        desktop_api_jwt: Option<&str>,
        desktop_api_jwt_expires_at: u64,
    ) -> Result<(), String> {
        let creds = StoredCredentials {
            device_token: device_token.to_string(),
            relay_url: relay_url.to_string(),
            api_base_url: api_base_url.to_string(),
        };
        self.store.save(&creds)?;

        let mut inner = self.inner.lock().await;
        inner.relay_jwt = Some(relay_jwt.to_string());
        inner.relay_jwt_expires_at = relay_jwt_expires_at;
        inner.desktop_api_jwt = desktop_api_jwt.map(|s| s.to_string());
        inner.desktop_api_jwt_expires_at = desktop_api_jwt_expires_at;
        inner.api_base_url = api_base_url.to_string();

        info!("Credentials initialized — device token stored");
        Ok(())
    }

    /// Load stored credentials (for app relaunch).
    /// Returns (relay_url, api_base_url) if credentials exist.
    pub fn load_stored(&self) -> Result<(String, String), String> {
        let creds = self.store.load()?;
        Ok((creds.relay_url, creds.api_base_url))
    }

    /// Update the API base URL (needed when loading stored credentials).
    pub async fn set_api_base_url(&self, url: &str) {
        let mut inner = self.inner.lock().await;
        inner.api_base_url = url.to_string();
    }

    /// Update the optional Vercel deployment protection bypass token.
    pub async fn set_vercel_bypass(&self, token: Option<String>) {
        let mut inner = self.inner.lock().await;
        inner.vercel_bypass = token;
    }

    /// Check if stored credentials exist.
    pub fn has_stored_credentials(&self) -> bool {
        self.store.exists()
    }

    /// Check if the relay JWT is within 10 minutes of expiry.
    /// Used by the network runtime to proactively reconnect before
    /// the relay session becomes stale.
    ///
    /// The threshold (10min) matches the check interval (10min) so it's
    /// mathematically impossible to skip the refresh window:
    /// - JWT lifetime: 60min
    /// - Danger zone: last 10min (T=50..60)
    /// - Check interval: 10min
    /// - Guarantee: at least one check lands in T=50..60
    pub async fn relay_jwt_needs_refresh(&self) -> Result<bool, ()> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let inner = self.inner.lock().await;
        // 600s = 10 minutes before expiry — matches the 10min check interval
        Ok(inner.relay_jwt_expires_at > 0 && inner.relay_jwt_expires_at <= now + 600)
    }

    /// Get a valid relay JWT — returns cached if still valid, refreshes if near-expiry.
    pub async fn get_jwt(&self) -> Result<String, RefreshError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Fast path: check cache without refresh lock
        {
            let inner = self.inner.lock().await;
            if let Some(ref jwt) = inner.relay_jwt {
                if inner.relay_jwt_expires_at > now + JWT_REFRESH_BUFFER_SECS {
                    return Ok(jwt.clone());
                }
            }
        }

        // Slow path: acquire refresh lock, double-check, then refresh
        let _refresh_guard = self.refresh_lock.lock().await;

        // Double-check: another task may have refreshed while we waited for the lock
        {
            let inner = self.inner.lock().await;
            if let Some(ref jwt) = inner.relay_jwt {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                if inner.relay_jwt_expires_at > now + JWT_REFRESH_BUFFER_SECS {
                    info!("JWT already refreshed by concurrent task — using cached");
                    return Ok(jwt.clone());
                }
            }
        }

        self.refresh_jwt_locked().await
    }

    /// Force refresh the JWT (called after relay 401/403).
    pub async fn force_refresh(&self) -> Result<String, RefreshError> {
        // Serialize: only one refresh at a time to prevent concurrent token rotation
        let _refresh_guard = self.refresh_lock.lock().await;
        self.refresh_jwt_locked().await
    }

    /// Get a valid desktop API JWT — returns cached if still valid, refreshes if near-expiry.
    /// Same cache → lock → double-check → refresh pattern as `get_jwt()`.
    pub async fn get_api_jwt(&self) -> Result<String, RefreshError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Fast path: check cache without refresh lock
        {
            let inner = self.inner.lock().await;
            if let Some(ref jwt) = inner.desktop_api_jwt {
                if inner.desktop_api_jwt_expires_at > now + JWT_REFRESH_BUFFER_SECS {
                    return Ok(jwt.clone());
                }
            }
        }

        // Slow path: acquire refresh lock, double-check, then refresh
        let _refresh_guard = self.refresh_lock.lock().await;

        // Double-check: another task may have refreshed while we waited for the lock
        {
            let inner = self.inner.lock().await;
            if let Some(ref jwt) = inner.desktop_api_jwt {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                if inner.desktop_api_jwt_expires_at > now + JWT_REFRESH_BUFFER_SECS {
                    info!("Desktop API JWT already refreshed by concurrent task — using cached");
                    return Ok(jwt.clone());
                }
            }
        }

        // Refresh both JWTs together
        self.refresh_jwt_locked().await?;

        let inner = self.inner.lock().await;
        inner.desktop_api_jwt.clone().ok_or(RefreshError::Transient)
    }

    /// Get the API base URL (needed by ControlPlaneClient and NetworkRuntime).
    pub async fn get_api_base_url(&self) -> String {
        let inner = self.inner.lock().await;
        inner.api_base_url.clone()
    }

    /// Call POST /api/desktop/refresh with device_token.
    /// Updates stored credentials with new device_token, caches new JWT.
    ///
    /// MUST be called with `refresh_lock` held — concurrent calls with the same
    /// device_token cause the server to revoke the entire token family (RFC 8252 replay detection).
    async fn refresh_jwt_locked(&self) -> Result<String, RefreshError> {
        let inner = self.inner.lock().await;
        let api_base_url = inner.api_base_url.clone();
        let vercel_bypass = inner.vercel_bypass.clone();
        drop(inner);

        let creds = self.store.load().map_err(|e| {
            error!("Failed to load stored credentials: {e}");
            RefreshError::Transient
        })?;

        let url = format!("{api_base_url}/api/desktop/refresh");
        info!("Refreshing relay JWT via {url}");

        let mut req = self
            .http
            .post(&url)
            .json(&serde_json::json!({ "device_token": creds.device_token }));

        if let Some(bypass) = vercel_bypass {
            req = req.header("x-vercel-protection-bypass", bypass);
        }

        let resp = req.send().await.map_err(|e| {
            warn!("Refresh HTTP request failed: {e}");
            RefreshError::Transient
        })?;

        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            error!("Refresh returned 401 — auth expired");
            return Err(RefreshError::AuthExpired);
        }

        if !resp.status().is_success() {
            warn!("Refresh returned {}", resp.status());
            return Err(RefreshError::Transient);
        }

        let data: RefreshResponse = resp.json().await.map_err(|e| {
            warn!("Failed to parse refresh response: {e}");
            RefreshError::Transient
        })?;

        // Persist new device token (rotation)
        let updated_creds = StoredCredentials {
            device_token: data.device_token,
            relay_url: creds.relay_url,
            api_base_url: creds.api_base_url,
        };
        self.store.save(&updated_creds).map_err(|e| {
            error!("Failed to persist rotated device token: {e}");
            RefreshError::Transient
        })?;

        // Cache new JWTs
        let mut inner = self.inner.lock().await;
        inner.relay_jwt = Some(data.relay_jwt.clone());
        inner.relay_jwt_expires_at = data.relay_jwt_expires_at;
        inner.desktop_api_jwt = data.desktop_api_jwt;
        inner.desktop_api_jwt_expires_at = data.desktop_api_jwt_expires_at.unwrap_or(0);

        info!(
            "JWT refreshed successfully, expires at {}",
            data.relay_jwt_expires_at
        );
        Ok(data.relay_jwt)
    }

    /// Clear all stored credentials (on sign-out).
    pub fn clear(&self) {
        let _ = self.store.delete();
    }
}
