use std::path::PathBuf;
use std::time::Duration;

use tracing::info;

use crate::ProviderError;
use super::{
    credential_store::{self, StoredCredentials},
    headers::{COPILOT_OAUTH_CLIENT_ID, COPILOT_OAUTH_SCOPE},
};

pub enum DeviceFlowOutcome {
    Success { path: PathBuf },
    Declined,
}

/// Run the GitHub OAuth device flow and save credentials to `credential_path`.
pub async fn login_device_flow(
    credential_path: PathBuf,
) -> Result<DeviceFlowOutcome, ProviderError> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| ProviderError::Network(e.to_string()))?;

    // Step 1: Request device + user codes.
    let device_resp = http
        .post("https://github.com/login/device/code")
        .header("Accept", "application/json")
        .form(&[
            ("client_id", COPILOT_OAUTH_CLIENT_ID),
            ("scope", COPILOT_OAUTH_SCOPE),
        ])
        .send()
        .await
        .map_err(|e| ProviderError::Network(e.to_string()))?;

    if !device_resp.status().is_success() {
        let status = device_resp.status().as_u16();
        let body = device_resp.text().await.unwrap_or_default();
        return Err(ProviderError::Upstream { status, body });
    }

    let device_json: serde_json::Value = device_resp
        .json()
        .await
        .map_err(|e| ProviderError::Decode(format!("device code response: {e}")))?;

    let device_code = device_json
        .get("device_code")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ProviderError::Decode("missing device_code".to_string()))?
        .to_string();
    let user_code = device_json
        .get("user_code")
        .and_then(|v| v.as_str())
        .unwrap_or("???")
        .to_string();
    let verification_uri = device_json
        .get("verification_uri")
        .and_then(|v| v.as_str())
        .unwrap_or("https://github.com/login/device")
        .to_string();
    let interval_secs = device_json
        .get("interval")
        .and_then(|v| v.as_u64())
        .unwrap_or(5);

    // Step 2: Display instructions.
    println!("Open the following URL and enter the code below:");
    println!("  URL:  {verification_uri}");
    println!("  Code: {user_code}");
    println!("Waiting for authorization…");

    // Step 3: Poll until approved or denied.
    loop {
        tokio::time::sleep(Duration::from_secs(interval_secs)).await;

        let poll_resp = http
            .post("https://github.com/login/oauth/access_token")
            .header("Accept", "application/json")
            .form(&[
                ("client_id", COPILOT_OAUTH_CLIENT_ID),
                ("device_code", device_code.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send()
            .await
            .map_err(|e| ProviderError::Network(e.to_string()))?;

        let poll_json: serde_json::Value = poll_resp
            .json()
            .await
            .map_err(|e| ProviderError::Decode(format!("poll response: {e}")))?;

        if let Some(token) = poll_json.get("access_token").and_then(|v| v.as_str()) {
            let creds = StoredCredentials {
                github_oauth_token: token.to_string(),
                created_at_unix: chrono::Utc::now().timestamp(),
            };
            credential_store::save(&credential_path, &creds)?;
            info!("Copilot credentials saved to {}", credential_path.display());
            return Ok(DeviceFlowOutcome::Success {
                path: credential_path,
            });
        }

        if let Some(err) = poll_json.get("error").and_then(|v| v.as_str()) {
            match err {
                "authorization_pending" | "slow_down" => continue,
                "access_denied" => return Ok(DeviceFlowOutcome::Declined),
                other => {
                    return Err(ProviderError::Upstream {
                        status: 400,
                        body: format!("device flow error: {other}"),
                    })
                }
            }
        }
    }
}
