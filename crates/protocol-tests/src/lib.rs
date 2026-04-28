use std::path::PathBuf;
use std::time::Duration;

use agent_shim_core::stream::{CanonicalStream, StreamEvent};
use bytes::Bytes;
use futures_util::{stream, StreamExt};

use agent_shim_frontends::FrontendError;

/// Resolve the path to a named fixture file under `fixtures/canonical/`.
pub fn fixture(name: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("fixtures");
    p.push("canonical");
    p.push(name);
    p
}

/// Read a JSONL fixture file and replay it as a `CanonicalStream`.
/// Each non-empty line must deserialize as a `StreamEvent`.
/// `per_event_delay` is currently ignored (kept for API compatibility).
pub fn replay_jsonl(path: PathBuf, _per_event_delay: Option<Duration>) -> CanonicalStream {
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read fixture {:?}: {}", path, e));

    let events: Vec<Result<StreamEvent, agent_shim_core::error::StreamError>> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            serde_json::from_str::<StreamEvent>(line)
                .map_err(|e| agent_shim_core::error::StreamError::Decode(e.to_string()))
        })
        .collect();

    Box::pin(stream::iter(events))
}

/// Collect all `Bytes` chunks from a boxed stream into a single contiguous `Bytes`.
pub async fn collect_sse(
    mut s: futures_util::stream::BoxStream<'static, Result<Bytes, FrontendError>>,
) -> Bytes {
    let mut out = Vec::new();
    while let Some(chunk) = s.next().await {
        match chunk {
            Ok(b) => out.extend_from_slice(&b),
            Err(e) => panic!("stream error: {}", e),
        }
    }
    Bytes::from(out)
}
