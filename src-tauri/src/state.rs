use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;
use tunnel_core::credentials::CredentialManager;
use tunnel_core::TunnelHandle;

use crate::credential_store::FileCredentialStore;

/// Shared tunnel state managed by Tauri.
pub struct TunnelState {
    pub handle: Mutex<Option<TunnelHandle>>,
    /// Fail-closed network gate — defaults to false.
    /// Rust won't reconnect until JS classifies network as safe.
    pub network_allowed: Arc<AtomicBool>,
    /// Pulsed when `network_allowed` changes — wakes the reconnect loop instantly.
    pub network_notify: Arc<Notify>,
    /// Credential manager — shared between commands and tunnel task.
    pub credential_manager: Arc<CredentialManager>,
}

impl TunnelState {
    /// Create tunnel state with file-based credential storage in the given app data directory.
    pub fn new(app_data_dir: PathBuf) -> Self {
        let store = Box::new(FileCredentialStore::new(app_data_dir));
        Self {
            handle: Mutex::new(None),
            network_allowed: Arc::new(AtomicBool::new(false)),
            network_notify: Arc::new(Notify::new()),
            credential_manager: Arc::new(CredentialManager::new(String::new(), store)),
        }
    }
}
