use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::{BufMut, Bytes, BytesMut};
use futures_util::Stream;

pub fn event(event_name: &str, data_json: &str) -> Bytes {
    let mut buf = BytesMut::with_capacity(event_name.len() + data_json.len() + 16);
    buf.put_slice(b"event: ");
    buf.put_slice(event_name.as_bytes());
    buf.put_slice(b"\ndata: ");
    buf.put_slice(data_json.as_bytes());
    buf.put_slice(b"\n\n");
    buf.freeze()
}

pub fn data_only(data_json: &str) -> Bytes {
    let mut buf = BytesMut::with_capacity(data_json.len() + 8);
    buf.put_slice(b"data: ");
    buf.put_slice(data_json.as_bytes());
    buf.put_slice(b"\n\n");
    buf.freeze()
}

pub fn comment(text: &str) -> Bytes {
    let mut buf = BytesMut::with_capacity(text.len() + 4);
    buf.put_slice(b": ");
    buf.put_slice(text.as_bytes());
    buf.put_slice(b"\n\n");
    buf.freeze()
}

/// Wraps an inner SSE byte stream with periodic keepalive comments.
///
/// The wrapper observes the inner stream's own `Poll::Ready(None)` to stop
/// pinging — so when the encoder terminates early (cancel, upstream error
/// before lifecycle close) the keepalive task stops immediately instead of
/// running until the next interval tick.
///
/// Used by all three frontend encoders (`anthropic_messages`, `openai_chat`,
/// `openai_responses`) — replaces three copies of the prior `Arc<AtomicBool>
/// done` + `select` + `Bytes::new()` sentinel + outer `scan()` shape.
pub struct KeepaliveStream<S> {
    inner: S,
    interval: tokio::time::Interval,
    inner_done: bool,
}

impl<S> KeepaliveStream<S> {
    pub fn new(inner: S, period: Duration) -> Self {
        Self {
            inner,
            interval: tokio::time::interval(period),
            inner_done: false,
        }
    }
}

impl<S, E> Stream for KeepaliveStream<S>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
{
    type Item = Result<Bytes, E>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        // Always prefer real bytes over pings.
        if !this.inner_done {
            match Pin::new(&mut this.inner).poll_next(cx) {
                Poll::Ready(Some(item)) => return Poll::Ready(Some(item)),
                Poll::Ready(None) => {
                    this.inner_done = true;
                    return Poll::Ready(None);
                }
                Poll::Pending => {}
            }
        } else {
            return Poll::Ready(None);
        }

        // Inner pending — emit a keepalive ping if the interval has fired.
        match this.interval.poll_tick(cx) {
            Poll::Ready(_) => Poll::Ready(Some(Ok(comment("ping")))),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_format() {
        let b = event("message_start", r#"{"a":1}"#);
        assert_eq!(
            std::str::from_utf8(&b).unwrap(),
            "event: message_start\ndata: {\"a\":1}\n\n"
        );
    }

    #[test]
    fn data_only_format() {
        let b = data_only(r#"{"x":2}"#);
        assert_eq!(std::str::from_utf8(&b).unwrap(), "data: {\"x\":2}\n\n");
    }

    #[test]
    fn comment_format() {
        let b = comment("ping");
        assert_eq!(std::str::from_utf8(&b).unwrap(), ": ping\n\n");
    }
}
