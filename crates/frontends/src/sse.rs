use bytes::{BufMut, Bytes, BytesMut};

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_format() {
        let b = event("message_start", r#"{"a":1}"#);
        assert_eq!(std::str::from_utf8(&b).unwrap(), "event: message_start\ndata: {\"a\":1}\n\n");
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
