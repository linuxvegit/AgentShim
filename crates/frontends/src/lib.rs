#![forbid(unsafe_code)]

pub mod anthropic_messages;
pub mod openai_chat;
pub mod openai_responses;
pub mod sse;

use agent_shim_core::{
    request::CanonicalRequest, response::CanonicalResponse, stream::CanonicalStream,
    target::FrontendKind,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum FrontendError {
    #[error("invalid request body: {0}")]
    InvalidBody(String),
    #[error("unsupported feature: {0}")]
    Unsupported(String),
    #[error("encoding failure: {0}")]
    Encode(String),
    #[error("decoding failure: {0}")]
    Decode(String),
}

pub enum FrontendResponse {
    Unary {
        content_type: String,
        body: bytes::Bytes,
    },
    Stream {
        content_type: String,
        stream: futures_util::stream::BoxStream<'static, Result<bytes::Bytes, FrontendError>>,
    },
}

#[async_trait::async_trait]
pub trait FrontendProtocol: Send + Sync {
    fn kind(&self) -> FrontendKind;
    fn decode_request(&self, body: &[u8]) -> Result<CanonicalRequest, FrontendError>;
    fn encode_unary(&self, response: CanonicalResponse) -> Result<FrontendResponse, FrontendError>;
    fn encode_stream(&self, stream: CanonicalStream) -> FrontendResponse;
}
