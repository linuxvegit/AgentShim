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

pub struct OpenAiResponses {
    pub keepalive: Option<Duration>,
    pub clock_override: Option<u64>,
}

impl OpenAiResponses {
    pub fn new() -> Self {
        Self {
            keepalive: None,
            clock_override: None,
        }
    }
}

impl Default for OpenAiResponses {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl FrontendProtocol for OpenAiResponses {
    fn kind(&self) -> FrontendKind {
        FrontendKind::OpenAiResponses
    }

    fn decode_request(&self, body: &[u8]) -> Result<CanonicalRequest, FrontendError> {
        decode::decode(body)
    }

    fn encode_unary(
        &self,
        response: CanonicalResponse,
    ) -> Result<FrontendResponse, FrontendError> {
        let body =
            encode_unary::encode_with_clock(response, self.clock_override)?;
        Ok(FrontendResponse::Unary {
            content_type: "application/json".into(),
            body,
        })
    }

    fn encode_stream(&self, stream: CanonicalStream) -> FrontendResponse {
        let sse_stream = encode_stream::encode(
            stream,
            self.keepalive,
            self.clock_override,
        );
        FrontendResponse::Stream {
            content_type: "text/event-stream".into(),
            stream: sse_stream.boxed(),
        }
    }
}
