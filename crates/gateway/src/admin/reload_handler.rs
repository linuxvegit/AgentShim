//! POST /admin/reload — Plan 04 P04 T3.
//!
//! Receives reload requests over HTTP and hands them off to the
//! reload-applying task via the mpsc channel held on
//! [`AppState::core::reload_tx`]. Awaits the oneshot response and maps
//! the [`ReloadOutcome`] to an HTTP status + structured JSON body
//! (spec §5.6).

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde_json::json;
use tokio::sync::oneshot;

use crate::reload_trigger::{ReloadOutcome, ReloadRequest, ReloadSource};
use crate::state::AppState;

pub async fn reload(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: bytes::Bytes,
) -> axum::response::Response {
    let source = if body.is_empty() {
        // No body → re-read from --config path. Servers started without
        // --config cannot reload from disk and must be reloaded with a
        // body (or via SIGHUP, which is also Path-mode).
        match state.core.config_path.clone() {
            Some(path) => ReloadSource::Path(path),
            None => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({
                        "ok": false,
                        "errors": [
                            "server started without --config; cannot reload from disk"
                        ]
                    })),
                )
                    .into_response();
            }
        }
    } else {
        // Body present → YAML payload reload. Require an explicit
        // Content-Type so a stray POST with JSON body doesn't get
        // silently parsed as YAML.
        let ct = headers
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !ct.starts_with("application/yaml") && !ct.starts_with("text/yaml") {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "ok": false,
                    "errors": [format!(
                        "expected Content-Type: application/yaml, got: {ct}"
                    )]
                })),
            )
                .into_response();
        }
        match std::str::from_utf8(&body) {
            Ok(s) => ReloadSource::Yaml(s.to_string()),
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"ok": false, "errors": ["body is not valid UTF-8"]})),
                )
                    .into_response();
            }
        }
    };

    let (tx, rx) = oneshot::channel();
    if state
        .core
        .reload_tx
        .send(ReloadRequest {
            source,
            respond_to: tx,
        })
        .await
        .is_err()
    {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"ok": false, "errors": ["reload task not running"]})),
        )
            .into_response();
    }

    match rx.await {
        Ok(ReloadOutcome::Ok(diff)) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "applied": {
                    "routes_total": diff.routes_total,
                    "routes_added": diff.routes_added,
                    "routes_removed": diff.routes_removed,
                    "routes_modified": diff.routes_modified,
                    "policies_changed": diff.policies_changed,
                    "auth_keys_added": diff.auth_keys_added,
                    "auth_keys_removed": diff.auth_keys_removed,
                },
                "warnings": diff.warnings,
            })),
        )
            .into_response(),
        Ok(ReloadOutcome::ValidationError(msg)) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "errors": [msg]})),
        )
            .into_response(),
        Ok(ReloadOutcome::ImmutableField(msg)) => (
            StatusCode::FORBIDDEN,
            Json(json!({"ok": false, "errors": [msg]})),
        )
            .into_response(),
        Ok(ReloadOutcome::Parse(msg)) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "errors": [format!("YAML parse error: {msg}")]})),
        )
            .into_response(),
        Ok(ReloadOutcome::PluginValidation(msg)) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "errors": [format!("plugin validation error: {msg}")]})),
        )
            .into_response(),
        Ok(ReloadOutcome::Io(msg)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"ok": false, "errors": [format!("IO error: {msg}")]})),
        )
            .into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"ok": false, "errors": ["reload task dropped response channel"]})),
        )
            .into_response(),
    }
}
