pub mod anthropic_messages;
pub mod openai_chat;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use thiserror::Error;

use agent_shim_frontends::FrontendError;
use agent_shim_providers::ProviderError;
use agent_shim_router::RouteError;

#[derive(Debug, Error)]
pub enum HandlerError {
    #[error("route error: {0}")]
    Route(#[from] RouteError),
    #[error("provider error: {0}")]
    Provider(#[from] ProviderError),
    #[error("frontend error: {0}")]
    Frontend(#[from] FrontendError),
    #[error("body read error: {0}")]
    BodyRead(String),
}

impl IntoResponse for HandlerError {
    fn into_response(self) -> Response {
        let status = match &self {
            HandlerError::Route(RouteError::NoRoute { .. }) => StatusCode::NOT_FOUND,
            HandlerError::Provider(ProviderError::Upstream { status, .. }) => {
                StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_GATEWAY)
            }
            HandlerError::Provider(ProviderError::UnknownProvider(_)) => StatusCode::BAD_GATEWAY,
            HandlerError::Provider(ProviderError::Network(_)) => StatusCode::BAD_GATEWAY,
            HandlerError::Provider(ProviderError::CapabilityMismatch(_)) => {
                StatusCode::UNPROCESSABLE_ENTITY
            }
            HandlerError::Frontend(FrontendError::InvalidBody(_)) => StatusCode::BAD_REQUEST,
            HandlerError::Frontend(FrontendError::Unsupported(_)) => {
                StatusCode::UNPROCESSABLE_ENTITY
            }
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };

        let body = serde_json::json!({ "error": { "message": self.to_string() } });
        (status, axum::Json(body)).into_response()
    }
}
