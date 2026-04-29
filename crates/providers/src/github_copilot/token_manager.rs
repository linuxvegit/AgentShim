use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};

use super::{
    credential_store::{self, StoredCredentials},
    endpoint::TokenExchangeResponse,
};
use crate::ProviderError;

#[derive(Debug, Clone)]
pub struct CopilotToken {
    pub token: String,
    pub api_base: String,
    pub expires_at_unix: i64,
}

enum ActorMsg {
    Get(oneshot::Sender<Result<CopilotToken, ProviderError>>),
    Invalidate,
}

/// Cloneable handle to the token manager actor.
#[derive(Clone)]
pub struct CopilotTokenManager {
    tx: Arc<mpsc::Sender<ActorMsg>>,
}

impl CopilotTokenManager {
    pub fn new(http: reqwest::Client, creds: StoredCredentials, base_url: String) -> Self {
        let (tx, rx) = mpsc::channel(16);
        tokio::spawn(actor(
            rx,
            http,
            CredentialSource::Preloaded(creds),
            base_url,
        ));
        Self { tx: Arc::new(tx) }
    }

    /// Create a token manager that loads credentials lazily from disk on first request.
    pub fn new_lazy(http: reqwest::Client, credential_path: PathBuf) -> Self {
        let (tx, rx) = mpsc::channel(16);
        tokio::spawn(actor(
            rx,
            http,
            CredentialSource::Lazy(credential_path),
            "https://api.github.com".to_string(),
        ));
        Self { tx: Arc::new(tx) }
    }

    pub async fn get(&self) -> Result<CopilotToken, ProviderError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(ActorMsg::Get(reply_tx))
            .await
            .map_err(|_| ProviderError::Network("token manager shut down".to_string()))?;
        reply_rx
            .await
            .map_err(|_| ProviderError::Network("token actor dropped reply".to_string()))?
    }

    pub async fn invalidate(&self) {
        let _ = self.tx.send(ActorMsg::Invalidate).await;
    }
}

enum CredentialSource {
    Preloaded(StoredCredentials),
    Lazy(PathBuf),
}

impl CredentialSource {
    fn load(&self) -> Result<StoredCredentials, ProviderError> {
        match self {
            Self::Preloaded(c) => Ok(c.clone()),
            Self::Lazy(path) => credential_store::load(path),
        }
    }
}

async fn actor(
    mut rx: mpsc::Receiver<ActorMsg>,
    http: reqwest::Client,
    cred_source: CredentialSource,
    base_url: String,
) {
    let mut cached: Option<CopilotToken> = None;

    while let Some(msg) = rx.recv().await {
        match msg {
            ActorMsg::Invalidate => {
                debug!("CopilotTokenManager: cache invalidated");
                cached = None;
            }
            ActorMsg::Get(reply) => {
                let now = chrono::Utc::now().timestamp();
                if let Some(ref t) = cached {
                    if t.expires_at_unix - now > 60 {
                        let _ = reply.send(Ok(t.clone()));
                        continue;
                    }
                }
                let creds = match cred_source.load() {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = reply.send(Err(ProviderError::Network(format!(
                            "load credentials: {e} — run `agent-shim copilot login` first"
                        ))));
                        continue;
                    }
                };
                let result = exchange_with_base(&http, &creds, &base_url).await;
                match result {
                    Ok(ref t) => {
                        cached = Some(t.clone());
                        let _ = reply.send(Ok(t.clone()));
                    }
                    Err(e) => {
                        cached = None;
                        warn!("CopilotTokenManager: token exchange failed: {e}");
                        let _ = reply.send(Err(e));
                    }
                }
            }
        }
    }
}

/// Exchange a GitHub OAuth token for a short-lived Copilot API token.
/// Public so tests can call it with a mockito base URL.
pub async fn exchange_with_base(
    http: &reqwest::Client,
    creds: &StoredCredentials,
    base_url: &str,
) -> Result<CopilotToken, ProviderError> {
    let url = format!(
        "{}/copilot_internal/v2/token",
        base_url.trim_end_matches('/')
    );
    let resp = http
        .get(&url)
        .header(
            reqwest::header::AUTHORIZATION,
            format!("token {}", creds.github_oauth_token),
        )
        .header(reqwest::header::USER_AGENT, super::headers::USER_AGENT)
        .header("Editor-Version", super::headers::EDITOR_VERSION)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| ProviderError::Network(e.to_string()))?;

    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(ProviderError::Upstream {
            status: 401,
            body: "unauthorized – re-run `copilot login`".to_string(),
        });
    }
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(ProviderError::Upstream {
            status: status.as_u16(),
            body,
        });
    }

    let exchange: TokenExchangeResponse = resp
        .json()
        .await
        .map_err(|e| ProviderError::Decode(format!("token response: {e}")))?;

    Ok(CopilotToken {
        token: exchange.token,
        api_base: exchange.endpoints.api,
        expires_at_unix: exchange.expires_at,
    })
}
