use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::{BufMut, Bytes, BytesMut};
use futures_util::Stream;
use serde::Serialize;

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

/// Single-allocation variant of `event(name, &serde_json::to_string(value)?)`.
/// Writes the SSE frame header, serialises `value` directly into the same
/// `BytesMut`, then appends the terminator -- one buffer for the whole frame.
///
/// The legacy chain `serde_json::to_string(...)` -> `sse::event(name, &s)`
/// allocated twice per event: an intermediate `String` for the JSON output,
/// then a fresh `BytesMut` for the framed bytes. For a 100-event response
/// across three encoders that is ~200 throw-away allocs per request; this
/// helper cuts it to ~100.
///
/// Returns `None` when serialisation fails. Callers preserve the prior
/// `Option` shape (each frontend's `serialize_event` / `emit` matched on
/// `.ok()?` and silently dropped the event on encode error).
pub fn event_serialized<T: ?Sized + Serialize>(event_name: &str, value: &T) -> Option<Bytes> {
    // Pre-size for `"event: <name>\ndata: \n\n"` plus an estimate of the
    // JSON body. 128 B covers all small canonical events
    // (ResponseStart, MessageStop, etc.); larger payloads (TextDelta with
    // long text, tool-call arguments) extend without reallocating until
    // 2x growth.
    let mut buf = BytesMut::with_capacity(event_name.len() + 128);
    buf.put_slice(b"event: ");
    buf.put_slice(event_name.as_bytes());
    buf.put_slice(b"\ndata: ");
    if serde_json::to_writer(BytesMutWriter(&mut buf), value).is_err() {
        return None;
    }
    buf.put_slice(b"\n\n");
    Some(buf.freeze())
}

/// Single-allocation variant of `data_only(&serde_json::to_string(value)?)`.
/// Mirror of [`event_serialized`] for encoders that emit raw `data:` frames
/// without an `event:` header (OpenAI Chat's chunks, `[DONE]` companions).
///
/// Returns `Bytes::new()` -- an empty frame -- when serialisation fails.
/// Matches the legacy fallback (`serde_json::to_string(...).unwrap_or_default()`
/// followed by `sse::data_only("")`), which produced a degenerate but
/// non-empty SSE comment-like frame; here the empty-bytes path is cheaper
/// and the failure surface (serde encoding a type the caller built itself)
/// stays as unreachable in practice.
pub fn data_only_serialized<T: ?Sized + Serialize>(value: &T) -> Bytes {
    let mut buf = BytesMut::with_capacity(128);
    buf.put_slice(b"data: ");
    if serde_json::to_writer(BytesMutWriter(&mut buf), value).is_err() {
        return Bytes::new();
    }
    buf.put_slice(b"\n\n");
    buf.freeze()
}

/// `io::Write` adapter over `&mut BytesMut`. `bytes` provides a `Writer`
/// type via `BufMut::writer()` but that consumes the buffer by value; we
/// need a borrowing wrapper to keep ownership inside the helper above.
struct BytesMutWriter<'a>(&'a mut BytesMut);

impl std::io::Write for BytesMutWriter<'_> {
    fn write(&mut self, src: &[u8]) -> std::io::Result<usize> {
        self.0.put_slice(src);
        Ok(src.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
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
    use serde::Serialize;

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

    #[derive(Serialize)]
    struct Payload<'a> {
        kind: &'a str,
        value: u32,
    }

    /// `event_serialized` must produce byte-identical output to the legacy
    /// `event(name, &serde_json::to_string(value)?)` chain. Any deviation
    /// would surface as an SSE protocol bug on the wire.
    #[test]
    fn event_serialized_matches_legacy_chain() {
        let value = Payload {
            kind: "delta",
            value: 42,
        };
        let new = event_serialized("message_delta", &value).expect("serialise");
        let legacy = {
            let s = serde_json::to_string(&value).unwrap();
            event("message_delta", &s)
        };
        assert_eq!(new, legacy);
        assert_eq!(
            std::str::from_utf8(&new).unwrap(),
            "event: message_delta\ndata: {\"kind\":\"delta\",\"value\":42}\n\n"
        );
    }

    #[test]
    fn data_only_serialized_matches_legacy_chain() {
        let value = Payload {
            kind: "chunk",
            value: 7,
        };
        let new = data_only_serialized(&value);
        let legacy = {
            let s = serde_json::to_string(&value).unwrap();
            data_only(&s)
        };
        assert_eq!(new, legacy);
    }

    /// Payloads larger than the 128-byte initial capacity must extend the
    /// `BytesMut` correctly -- the framing terminator must still land at
    /// the very end of the JSON body.
    #[test]
    fn event_serialized_handles_payload_larger_than_initial_capacity() {
        #[derive(Serialize)]
        struct Big {
            big_string: String,
        }
        let value = Big {
            big_string: "x".repeat(1024),
        };
        let bytes = event_serialized("text_delta", &value).expect("serialise");
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.starts_with("event: text_delta\ndata: "));
        assert!(s.ends_with("\n\n"));
        // Round-trips through serde_json without truncation.
        let body = &s["event: text_delta\ndata: ".len()..s.len() - 2];
        let v: serde_json::Value = serde_json::from_str(body).expect("valid json");
        assert_eq!(v["big_string"].as_str().unwrap().len(), 1024);
    }
}
