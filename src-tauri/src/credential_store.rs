use std::path::PathBuf;

use tunnel_core::credentials::{CredentialStore, StoredCredentials};

/// File-based credential store — persists credentials as JSON in the app data directory.
/// No OS keychain = no password prompts on macOS (the keyring crate with ad-hoc signing
/// triggers "Astral wants to use your confidential information" on every build).
///
/// Security: The file is in the per-user app data directory (~/Library/Application Support/
/// com.astral.desktop/ on macOS), protected by macOS file permissions (mode 0600).
/// This is the same security model used by Chrome, Firefox, VS Code, and most
/// Electron/Tauri apps for refresh tokens and session data.
///
/// TODO: When production builds use proper Apple Developer ID signing, migrate back to
/// OS Keychain for stronger secret protection (Apple recommendation).
pub struct FileCredentialStore {
    path: PathBuf,
}

impl FileCredentialStore {
    pub fn new(app_data_dir: PathBuf) -> Self {
        Self {
            path: app_data_dir.join("credentials.json"),
        }
    }
}

impl CredentialStore for FileCredentialStore {
    fn load(&self) -> Result<StoredCredentials, String> {
        let data =
            std::fs::read_to_string(&self.path).map_err(|e| format!("read credentials: {e}"))?;
        serde_json::from_str(&data).map_err(|e| format!("parse credentials: {e}"))
    }

    fn save(&self, creds: &StoredCredentials) -> Result<(), String> {
        // Ensure parent directory exists
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("create credentials dir: {e}"))?;
        }
        let data = serde_json::to_string_pretty(creds)
            .map_err(|e| format!("serialize credentials: {e}"))?;

        // Write atomically via temp file to prevent corruption on crash
        let tmp_path = self.path.with_extension("json.tmp");
        std::fs::write(&tmp_path, &data).map_err(|e| format!("write credentials tmp: {e}"))?;

        // Restrict to user-only read/write (mode 0600) on Unix
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            std::fs::set_permissions(&tmp_path, perms)
                .map_err(|e| format!("set credentials permissions: {e}"))?;
        }

        // Atomic rename
        std::fs::rename(&tmp_path, &self.path).map_err(|e| format!("rename credentials: {e}"))
    }

    fn delete(&self) -> Result<(), String> {
        if self.path.exists() {
            std::fs::remove_file(&self.path).map_err(|e| format!("delete credentials: {e}"))
        } else {
            Ok(())
        }
    }

    fn exists(&self) -> bool {
        self.path.exists()
    }
}
