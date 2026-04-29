pub mod decode;
pub mod encode_stream;
pub mod encode_unary;
pub mod mapping;
pub mod wire;

use std::time::Duration;

use agent_shim_core::{
    request::CanonicalRequest, response::CanonicalResponse, stream::CanonicalStream,
    target::FrontendKind,
};
use futures_util::StreamExt;

use crate::{FrontendError, FrontendProtocol, FrontendResponse};

pub struct AnthropicMessages {
    pub keepalive: Option<Duration>,
}

impl AnthropicMessages {
    pub fn new() -> Self {
        Self { keepalive: None }
    }

    pub fn with_keepalive(interval: Duration) -> Self {
        Self {
            keepalive: Some(interval),
        }
    }
}

impl Default for AnthropicMessages {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl FrontendProtocol for AnthropicMessages {
    fn kind(&self) -> FrontendKind {
        FrontendKind::AnthropicMessages
    }

    fn decode_request(&self, body: &[u8]) -> Result<CanonicalRequest, FrontendError> {
        decode::decode(body)
    }

    fn encode_unary(&self, response: CanonicalResponse) -> Result<FrontendResponse, FrontendError> {
        let body = encode_unary::encode(response)?;
        Ok(FrontendResponse::Unary {
            content_type: "application/json".into(),
            body,
        })
    }

    fn encode_stream(&self, stream: CanonicalStream) -> FrontendResponse {
        let sse_stream = encode_stream::encode(stream, self.keepalive);
        FrontendResponse::Stream {
            content_type: "text/event-stream".into(),
            stream: sse_stream.boxed(),
        }
    }
}
