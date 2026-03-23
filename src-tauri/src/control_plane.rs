use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Errors from control plane API calls.
#[derive(Debug)]
pub enum ControlPlaneError {
    /// Network/transport error (DNS, timeout, connection refused, parse failure).
    Network(String),
    /// Server returned 401 — desktop API JWT has expired, must refresh.
    AuthExpired,
    /// Server returned 5xx — transient server-side failure.
    ServerError(u16),
    /// Server returned 4xx (not 401) — likely a bad request.
    ClientError(u16),
}

impl fmt::Display for ControlPlaneError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ControlPlaneError::Network(msg) => write!(f, "network error: {msg}"),
            ControlPlaneError::AuthExpired => write!(f, "auth expired (401)"),
            ControlPlaneError::ServerError(code) => write!(f, "server error ({code})"),
            ControlPlaneError::ClientError(code) => write!(f, "client error ({code})"),
        }
    }
}

/// Result of an IP classification request from the backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassifyResult {
    pub classification: String,
    pub reason: String,
    pub trust_score: f64,
    pub ip: String,
    pub city: Option<String>,
    pub isp: Option<String>,
    pub is_safe: bool,
    pub matched_label: Option<String>,
}

/// HTTP client for the control plane API (classify + heartbeat).
///
/// Uses a long-lived reqwest::Client with connection pooling and keep-alive
/// to minimize latency on repeated calls from the network runtime.
pub struct ControlPlaneClient {
    http: reqwest::Client,
    vercel_bypass: std::sync::RwLock<Option<String>>,
}

impl ControlPlaneClient {
    /// Create a new control plane client.
    ///
    /// `vercel_bypass` — optional Vercel deployment protection bypass token.
    /// Can be updated later via `set_vercel_bypass()` when the frontend passes
    /// the Vite-baked token (which isn't available to Rust at process startup
    /// in packaged builds).
    pub fn new(vercel_bypass: Option<String>) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10))
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(15))
            .build()
            .expect("failed to build HTTP client");
        Self {
            http,
            vercel_bypass: std::sync::RwLock::new(vercel_bypass),
        }
    }

    /// Update the Vercel bypass token (called by connect command with the
    /// Vite-baked value from the frontend).
    pub fn set_vercel_bypass(&self, token: String) {
        if let Ok(mut guard) = self.vercel_bypass.write() {
            *guard = Some(token);
        }
    }

    /// Classify the current IP address via the backend control plane.
    ///
    /// Calls `POST {api_base}/api/desktop/runtime/classify` with Bearer auth.
    /// Returns the classification result including trust score and safety flag.
    pub async fn classify_ip(
        &self,
        api_base: &str,
        jwt: &str,
        event_type: &str,
        save_label: Option<&str>,
    ) -> Result<ClassifyResult, ControlPlaneError> {
        let tz = iana_time_zone::get_timezone().unwrap_or_default();
        let mut body = serde_json::json!({
            "timezone": tz,
            "event_type": event_type,
        });
        if let Some(label) = save_label {
            body["save_label"] = serde_json::Value::String(label.to_string());
        }

        let url = format!("{api_base}/api/desktop/runtime/classify");
        let mut req = self.http.post(&url).bearer_auth(jwt).json(&body);

        if let Some(bypass) = self.vercel_bypass.read().ok().and_then(|g| g.clone()) {
            req = req.header("x-vercel-protection-bypass", bypass);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| ControlPlaneError::Network(e.to_string()))?;
        let status = resp.status().as_u16();

        if status == 401 {
            return Err(ControlPlaneError::AuthExpired);
        }
        if status >= 500 {
            return Err(ControlPlaneError::ServerError(status));
        }
        if !resp.status().is_success() {
            return Err(ControlPlaneError::ClientError(status));
        }

        resp.json::<ClassifyResult>()
            .await
            .map_err(|e| ControlPlaneError::Network(e.to_string()))
    }

    /// Send a heartbeat to the backend control plane.
    ///
    /// Calls `POST {api_base}/api/desktop/runtime/heartbeat` with Bearer auth.
    pub async fn send_heartbeat(
        &self,
        api_base: &str,
        jwt: &str,
        status: &str,
        label: Option<&str>,
    ) -> Result<(), ControlPlaneError> {
        let tz = iana_time_zone::get_timezone().unwrap_or_default();
        let body = serde_json::json!({
            "timezone": tz,
            "status": status,
            "label": label.unwrap_or("home"),
        });

        let url = format!("{api_base}/api/desktop/runtime/heartbeat");
        let mut req = self.http.post(&url).bearer_auth(jwt).json(&body);

        if let Some(bypass) = self.vercel_bypass.read().ok().and_then(|g| g.clone()) {
            req = req.header("x-vercel-protection-bypass", bypass);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| ControlPlaneError::Network(e.to_string()))?;
        let status_code = resp.status().as_u16();

        if status_code == 401 {
            return Err(ControlPlaneError::AuthExpired);
        }
        if status_code >= 500 {
            return Err(ControlPlaneError::ServerError(status_code));
        }
        if !resp.status().is_success() {
            return Err(ControlPlaneError::ClientError(status_code));
        }

        Ok(())
    }
}
