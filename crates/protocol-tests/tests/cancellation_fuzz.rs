use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures::stream::{self, StreamExt};
use rand::{rngs::SmallRng, Rng, SeedableRng};

use agent_shim_core::error::StreamError;
use agent_shim_core::ids::ResponseId;
use agent_shim_core::message::MessageRole;
use agent_shim_core::stream::{CanonicalStream, ContentBlockKind, StreamEvent};
use agent_shim_core::usage::StopReason;
use agent_shim_frontends::anthropic_messages::AnthropicMessages;
use agent_shim_frontends::openai_chat::OpenAiChat;
use agent_shim_frontends::{FrontendProtocol, FrontendResponse};

/// Wraps an inner stream and increments `counter` by 1 when dropped.
struct DropAware<S> {
    inner: S,
    counter: Arc<AtomicUsize>,
}

impl<S> Drop for DropAware<S> {
    fn drop(&mut self) {
        self.counter.fetch_add(1, Ordering::SeqCst);
    }
}

impl<S, I, E> futures::Stream for DropAware<S>
where
    S: futures::Stream<Item = Result<I, E>> + Unpin,
{
    type Item = Result<I, E>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

/// Build a canonical stream of 1000 text deltas preceded by the required
/// open/start events and followed by stop events.
fn make_long_stream(counter: Arc<AtomicUsize>) -> CanonicalStream {
    let mut events: Vec<Result<StreamEvent, StreamError>> = Vec::with_capacity(1010);

    events.push(Ok(StreamEvent::ResponseStart {
        id: ResponseId::new(),
        model: "test-model".into(),
        created_at_unix: 0,
    }));
    events.push(Ok(StreamEvent::MessageStart { role: MessageRole::Assistant }));
    events.push(Ok(StreamEvent::ContentBlockStart { index: 0, kind: ContentBlockKind::Text }));

    for i in 0u32..1000 {
        events.push(Ok(StreamEvent::TextDelta { index: 0, text: format!("chunk-{i}") }));
    }

    events.push(Ok(StreamEvent::ContentBlockStop { index: 0 }));
    events.push(Ok(StreamEvent::MessageStop {
        stop_reason: StopReason::EndTurn,
        stop_sequence: None,
    }));
    events.push(Ok(StreamEvent::ResponseStop { usage: None }));

    let inner = DropAware { inner: stream::iter(events), counter };
    Box::pin(inner)
}

/// Collect at most `limit` bytes from a byte stream, then drop the stream.
async fn collect_up_to(
    mut stream: futures_util::stream::BoxStream<'static, Result<Bytes, agent_shim_frontends::FrontendError>>,
    limit: usize,
) {
    let mut collected = 0usize;
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(b) => {
                collected += b.len();
                if collected >= limit {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    // Explicit drop to propagate cancellation upstream
    drop(stream);
}

#[tokio::test]
async fn cancellation_fuzz_anthropic() {
    let mut rng = SmallRng::seed_from_u64(0xdeadbeef_cafebabe);

    for _iteration in 0..50 {
        let counter = Arc::new(AtomicUsize::new(0));

        let upstream = make_long_stream(Arc::clone(&counter));
        let frontend = AnthropicMessages::new();
        let response = frontend.encode_stream(upstream);

        let encoded_stream = match response {
            FrontendResponse::Stream { stream, .. } => stream,
            FrontendResponse::Unary { .. } => panic!("expected stream from encode_stream"),
        };

        // Pick a random byte offset between 0 and 8192
        let cutoff: usize = rng.gen_range(0..8192);
        collect_up_to(encoded_stream, cutoff).await;

        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "upstream DropAware was not dropped after cancellation at offset {cutoff}"
        );
    }
}

#[tokio::test]
async fn cancellation_fuzz_openai() {
    let mut rng = SmallRng::seed_from_u64(0xbaddecaf_feedface);

    for _iteration in 0..50 {
        let counter = Arc::new(AtomicUsize::new(0));

        let upstream = make_long_stream(Arc::clone(&counter));
        let frontend = OpenAiChat::new();
        let response = frontend.encode_stream(upstream);

        let encoded_stream = match response {
            FrontendResponse::Stream { stream, .. } => stream,
            FrontendResponse::Unary { .. } => panic!("expected stream from encode_stream"),
        };

        let cutoff: usize = rng.gen_range(0..8192);
        collect_up_to(encoded_stream, cutoff).await;

        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "upstream DropAware was not dropped after cancellation at offset {cutoff}"
        );
    }
}
