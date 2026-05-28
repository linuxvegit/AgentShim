//! POST /admin/discover — re-run upstream model discovery without a
//! full config reload.
//!
//! Status: **stub returning 501 Not Implemented.** Atomic in-place
//! replacement of `AppCore::resolver` (and therefore its embedded
//! `ModelIndex`) requires moving the resolver behind an `ArcSwap` or
//! introducing interior mutability inside `ModelResolver`. Both options
//! reach across the gateway/router boundary and would noticeably grow
//! this task's diff.
//!
//! Operators who need fresh discovery today have two existing knobs:
//! - `POST /admin/reload` (existing) re-runs full startup, which
//!   includes re-running `BackendProvider::list_models` per provider.
//! - `kill -HUP` (existing) does the same.
//!
//! TODO: Plan a follow-up that moves `AppCore::resolver` behind
//! `ArcSwap<ModelResolver>` (or wraps the `ModelIndex` inside the
//! resolver), then replace this 501 body with a real implementation that
//! re-runs discovery and swaps in a fresh resolver while leaving the
//! static route table intact.

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde_json::json;

use crate::state::AppState;

pub async fn handle(State(_state): State<AppState>) -> impl IntoResponse {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({
            "error": {
                "message": "/admin/discover is not yet implemented — use POST /admin/reload \
                            (or SIGHUP) to re-run model discovery as part of a full config reload.",
                "type": "not_implemented_error",
                "code": "discover_unimplemented",
            }
        })),
    )
}
