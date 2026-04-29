use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::ProviderError;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredCredentials {
    pub github_oauth_token: String,
    pub created_at_unix: i64,
}

/// Return the default path for the credential file.
pub fn default_path() -> Result<PathBuf, ProviderError> {
    let base = dirs::config_dir()
        .ok_or_else(|| ProviderError::Encode("could not determine config directory".to_string()))?;
    Ok(base.join("agent-shim").join("copilot-credentials.json"))
}

/// Load credentials from `path`.
pub fn load(path: &PathBuf) -> Result<StoredCredentials, ProviderError> {
    let data = std::fs::read_to_string(path)
        .map_err(|e| ProviderError::Encode(format!("read credentials: {e}")))?;
    serde_json::from_str(&data)
        .map_err(|e| ProviderError::Decode(format!("parse credentials: {e}")))
}

/// Save credentials to `path`, creating parent directories as needed.
/// Sets file permissions to 0600 on Unix.
pub fn save(path: &PathBuf, creds: &StoredCredentials) -> Result<(), ProviderError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| ProviderError::Encode(format!("create credential dir: {e}")))?;
    }
    let json = serde_json::to_string_pretty(creds)
        .map_err(|e| ProviderError::Encode(format!("serialize credentials: {e}")))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true).mode(0o600);
        use std::io::Write;
        let mut file = opts
            .open(path)
            .map_err(|e| ProviderError::Encode(format!("open credential file: {e}")))?;
        file.write_all(json.as_bytes())
            .map_err(|e| ProviderError::Encode(format!("write credentials: {e}")))?;
    }

    #[cfg(not(unix))]
    {
        std::fs::write(path, json)
            .map_err(|e| ProviderError::Encode(format!("write credentials: {e}")))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let dir = std::env::temp_dir().join(format!(
            "agentshim-cred-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("creds.json");

        let creds = StoredCredentials {
            github_oauth_token: "gho_test_token".to_string(),
            created_at_unix: 1_700_000_000,
        };

        save(&path, &creds).unwrap();
        let loaded = load(&path).unwrap();

        assert_eq!(loaded.github_oauth_token, creds.github_oauth_token);
        assert_eq!(loaded.created_at_unix, creds.created_at_unix);

        // Cleanup
        let _ = std::fs::remove_dir_all(dir);
    }
}
