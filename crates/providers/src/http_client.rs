//! Shared `reqwest::Client` builder for upstream provider HTTP calls.
//!
//! All providers go through `build(read_timeout)` so the gateway has one
//! consistent HTTP transport policy. Two non-defaults are applied:
//!
//! - **`connect_timeout = 10s`** — fixed cap on how long we wait for a TCP
//!   and TLS handshake to a provider. Shorter than a default `timeout`
//!   would give us, but never the wrong call for a healthy upstream.
//!
//! - **`read_timeout = <per-provider>`** — maximum gap between two body
//!   reads. We deliberately do **NOT** set `reqwest::ClientBuilder::timeout`
//!   (which is a total request budget): LLM streaming responses can take
//!   minutes for long tool-call arguments, and a total budget kills
//!   otherwise-healthy streams that just happen to be slow. Read gap is
//!   the right primitive for streaming.
//!
//! The one extra capability is **opt-in TLS key logging** for offline pcap
//! decryption when debugging upstream SSE issues. If the `SSLKEYLOGFILE`
//! environment variable is set at the time `build` is called, we
//! construct a `rustls::ClientConfig` with `KeyLogFile::new()` plumbed in
//! and hand it to reqwest via `use_preconfigured_tls`. rustls then writes
//! `CLIENT_RANDOM` lines to that file, which Wireshark / tshark can use to
//! decrypt captured TLS traffic.
//!
//! Why this is needed: reqwest's default rustls `ClientConfig` does **not**
//! honor `SSLKEYLOGFILE` on its own — the env var is only meaningful if
//! `ClientConfig.key_log` has been set. Setting it requires owning the
//! `ClientConfig`, which means we have to build it ourselves and pass it
//! through `use_preconfigured_tls`.
//!
//! Roots come from `webpki-roots` (Mozilla CA bundle baked into the binary)
//! to keep the implementation cross-platform and free of OS-trust-store
//! plumbing.
//!
//! When `SSLKEYLOGFILE` is **unset** we never construct the alternate config,
//! so production behavior is identical to the default rustls config reqwest
//! would build itself.

use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use crate::ProviderError;

/// Connect timeout for upstream provider HTTP calls. Fixed at 10s — if a
/// TCP/TLS handshake to the provider takes longer than that, something is
/// genuinely broken on the network path. Not configurable because no caller
/// has a legitimate reason to wait longer just to *establish* a connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Build the shared HTTP client used by all upstream providers.
///
/// `read_timeout` is the **maximum gap between successive reads** from the
/// response body. It is NOT a total request timeout: a streaming SSE
/// response can run for minutes or hours as long as the upstream emits at
/// least one byte every `read_timeout` seconds. This matches the natural
/// shape of LLM streaming, where a long tool-call response can take
/// 2-10 minutes of model time but the gap between successive deltas is
/// typically under a second.
///
/// Historically this helper applied `reqwest::ClientBuilder::timeout`, a
/// total-request budget that includes body read time. With Claude streaming
/// long tool-call arguments through Copilot, that 120s ceiling killed
/// otherwise-healthy streams mid-flight and surfaced as the
/// "error decoding response body" WARN. Switching to `read_timeout`
/// (per-read gap) is the correct primitive for streaming workloads.
///
/// When `SSLKEYLOGFILE` is set, the returned client logs TLS session keys
/// for offline decryption — see the module docs.
pub(crate) fn build(read_timeout: Duration) -> Result<reqwest::Client, ProviderError> {
    build_with_keylog(read_timeout, std::env::var_os("SSLKEYLOGFILE").is_some())
}

/// Like `build`, but takes the keylog flag explicitly. Lets tests exercise
/// both branches without mutating the process env (which is racy under
/// parallel test runners like nextest).
fn build_with_keylog(
    read_timeout: Duration,
    keylog: bool,
) -> Result<reqwest::Client, ProviderError> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(read_timeout);

    if keylog {
        match build_keylogging_tls_config() {
            Ok(config) => {
                tracing::info!(
                    "SSLKEYLOGFILE set — TLS key logging enabled for upstream HTTP client"
                );
                builder = builder.use_preconfigured_tls(config);
            }
            Err(e) => {
                // Fall through to default rustls ClientConfig. Failing here
                // would silently disable upstreams just because someone left
                // SSLKEYLOGFILE set, which is much worse than missing keylog
                // output.
                tracing::warn!(
                    error = %e,
                    "failed to build keylogging TLS config; falling back to default"
                );
            }
        }
    }

    builder
        .build()
        .map_err(|e| ProviderError::Network(e.to_string()))
}

/// Build a `rustls::ClientConfig` whose `key_log` writes `CLIENT_RANDOM`
/// lines to whatever path `SSLKEYLOGFILE` points at. Roots come from
/// `webpki-roots`. Same crypto provider (`ring`) reqwest's `rustls-tls`
/// feature uses, so behavior matches what we'd get without this branch.
fn build_keylogging_tls_config() -> Result<rustls::ClientConfig, String> {
    install_default_crypto_provider();

    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    let mut config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();

    // ALPN must match what reqwest's own default ClientConfig advertises.
    // The workspace builds reqwest with `default-features = false` and only
    // re-enables `rustls-tls`/`json`/`stream`/`gzip`, which leaves the
    // `http2` feature OFF. If we advertise `h2` here rustls will happily
    // negotiate it, then hyper-util panics with `http2 feature is not
    // enabled` the first time it tries to use the stream. Sticking to
    // `http/1.1` matches the actual transport the binary can speak.
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    config.key_log = Arc::new(rustls::KeyLogFile::new());

    Ok(config)
}

/// Ensure rustls has a default `CryptoProvider` installed. We force
/// `ring` because reqwest 0.12's `rustls-tls` feature pulls it in already
/// (no extra crate cost) and it matches what the rest of the binary uses.
///
/// Calling `install_default` more than once would panic, so we gate it on
/// a `OnceLock`. Errors from `install_default` (i.e. another provider was
/// installed first) are ignored — that's fine, we just use whatever is
/// already there.
fn install_default_crypto_provider() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_succeeds_without_keylog() {
        let result = build_with_keylog(Duration::from_secs(30), false);
        assert!(result.is_ok(), "default build path must succeed: {result:?}");
    }

    #[test]
    fn build_succeeds_with_keylog() {
        // No env mutation — the keylog flag is passed explicitly so this
        // test is safe to run in parallel.
        let result = build_with_keylog(Duration::from_secs(30), true);
        assert!(
            result.is_ok(),
            "keylogging build path must succeed: {result:?}"
        );
    }
}
