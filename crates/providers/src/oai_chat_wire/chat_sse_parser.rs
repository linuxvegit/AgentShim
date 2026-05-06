//! Parse an SSE byte-stream from OpenAI into a CanonicalStream.

use std::collections::HashSet;

use eventsource_stream::Eventsource;
use futures::StreamExt;
use futures_core::Stream;

use agent_shim_core::{
    ContentBlockKind, MessageRole, ResponseId, StopReason, StreamError, StreamEvent, ToolCallId,
    Usage,
};

pub(crate) fn parse<S>(byte_stream: S) -> agent_shim_core::CanonicalStream
where
    S: Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
{
    let sse_stream = byte_stream.eventsource();

    let mut emitted_response_start = false;
    let mut emitted_message_start = false;
    let mut text_block_open = false;
    let mut open_tool_blocks: HashSet<u32> = HashSet::new();
    let mut response_id = String::new();
    let mut response_model = String::new();
    // Tracks whether the upstream has signalled end-of-stream (`[DONE]` sentinel
    // or a `finish_reason`). Many OpenAI-compatible providers (notably GitHub
    // Copilot via its CloudFlare-fronted endpoint) close the underlying
    // connection abruptly after `[DONE]`, which surfaces as a benign reqwest
    // "error decoding response body" on the body stream. Once we've already
    // emitted a clean termination we drop those transport errors instead of
    // propagating them as `StreamError::Upstream` and noisy WARN logs.
    let mut stream_completed = false;
    // [DEBUG-sse1] context for diagnosing why a transport error survives the
    // `stream_completed` guard. Tracks how much SSE traffic the parser saw and
    // what the last meaningful state was when the body stream broke. Kept as
    // lasting diagnostic — when an upstream regresses, this gives operators
    // enough context to triage without re-instrumenting.
    let mut event_count: u64 = 0;
    let mut total_data_bytes: u64 = 0;
    let mut last_event_kind: &'static str = "none";
    let mut last_finish_reason: Option<String> = None;

    let event_stream = sse_stream.flat_map(move |result| {
        let events: Vec<Result<StreamEvent, StreamError>> = match result {
            Err(e) => {
                if stream_completed {
                    tracing::debug!(
                        error = %e,
                        "ignoring transport error after upstream end-of-stream"
                    );
                    Vec::new()
                } else {
                    tracing::warn!(
                        error = %e,
                        debug_tag = "DEBUG-sse1",
                        event_count,
                        total_data_bytes,
                        last_event_kind,
                        last_finish_reason = ?last_finish_reason,
                        emitted_response_start,
                        emitted_message_start,
                        text_block_open,
                        open_tool_blocks = open_tool_blocks.len(),
                        response_id = %response_id,
                        response_model = %response_model,
                        "SSE stream error"
                    );
                    vec![Err(StreamError::Upstream(e.to_string()))]
                }
            }
            Ok(event) => {
                event_count += 1;
                total_data_bytes += event.data.len() as u64;
                tracing::debug!(event_type = %event.event, data_len = event.data.len(), "SSE event received");
                if event.data == "[DONE]" {
                    last_event_kind = "done";
                    let mut evts = Vec::new();
                    // Close any open text block
                    if text_block_open {
                        evts.push(Ok(StreamEvent::ContentBlockStop { index: 0 }));
                        text_block_open = false;
                    }
                    // Close any open tool blocks
                    for idx in open_tool_blocks.drain() {
                        evts.push(Ok(StreamEvent::ToolCallStop { index: idx }));
                        evts.push(Ok(StreamEvent::ContentBlockStop { index: idx }));
                    }
                    evts.push(Ok(StreamEvent::ResponseStop { usage: None }));
                    stream_completed = true;
                    evts
                } else {
                    match parse_chunk(
                        &event.data,
                        &mut emitted_response_start,
                        &mut emitted_message_start,
                        &mut text_block_open,
                        &mut open_tool_blocks,
                        &mut response_id,
                        &mut response_model,
                        &mut stream_completed,
                        &mut last_event_kind,
                        &mut last_finish_reason,
                    ) {
                        Ok(evts) => evts.into_iter().map(Ok).collect(),
                        Err(e) => {
                            last_event_kind = "decode_error";
                            vec![Err(StreamError::Decode(e))]
                        }
                    }
                }
            }
        };
        futures::stream::iter(events)
    });

    Box::pin(event_stream)
}

#[allow(clippy::too_many_arguments)]
fn parse_chunk(
    data: &str,
    emitted_response_start: &mut bool,
    emitted_message_start: &mut bool,
    text_block_open: &mut bool,
    open_tool_blocks: &mut HashSet<u32>,
    response_id: &mut String,
    response_model: &mut String,
    stream_completed: &mut bool,
    last_event_kind: &mut &'static str,
    last_finish_reason: &mut Option<String>,
) -> Result<Vec<StreamEvent>, String> {
    let v: serde_json::Value =
        serde_json::from_str(data).map_err(|e| format!("json parse: {e}"))?;

    if let Some(err) = v.get("error") {
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("upstream error")
            .to_string();
        *last_event_kind = "upstream_error";
        return Ok(vec![StreamEvent::Error { message: msg }]);
    }

    *last_event_kind = "delta";

    let mut events = Vec::new();

    let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("unknown");
    let model = v.get("model").and_then(|x| x.as_str()).unwrap_or("unknown");
    let created = v.get("created").and_then(|x| x.as_u64()).unwrap_or(0);

    if !*emitted_response_start {
        *emitted_response_start = true;
        *response_id = id.to_string();
        *response_model = model.to_string();
        events.push(StreamEvent::ResponseStart {
            id: ResponseId(id.to_string()),
            model: model.to_string(),
            created_at_unix: created,
        });
    }

    let choices = match v.get("choices").and_then(|c| c.as_array()) {
        Some(c) => c,
        None => {
            if let Some(usage) = parse_usage(&v) {
                events.push(StreamEvent::UsageDelta { usage });
            }
            return Ok(events);
        }
    };

    for choice in choices {
        // role → MessageStart (once)
        if let Some(delta) = choice.get("delta") {
            if let Some(_role) = delta.get("role").and_then(|r| r.as_str()) {
                if !*emitted_message_start {
                    *emitted_message_start = true;
                    events.push(StreamEvent::MessageStart {
                        role: MessageRole::Assistant,
                    });
                }
            }

            // content text delta
            if let Some(text) = delta.get("content").and_then(|c| c.as_str()) {
                if !text.is_empty() {
                    if !*emitted_message_start {
                        *emitted_message_start = true;
                        events.push(StreamEvent::MessageStart {
                            role: MessageRole::Assistant,
                        });
                    }
                    if !*text_block_open {
                        *text_block_open = true;
                        events.push(StreamEvent::ContentBlockStart {
                            index: 0,
                            kind: ContentBlockKind::Text,
                        });
                    }
                    events.push(StreamEvent::TextDelta {
                        index: 0,
                        text: text.to_string(),
                    });
                }
            }

            // tool_calls deltas
            if let Some(tool_calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                if !*emitted_message_start {
                    *emitted_message_start = true;
                    events.push(StreamEvent::MessageStart {
                        role: MessageRole::Assistant,
                    });
                }
                // Close text block if open before tool calls start
                if *text_block_open {
                    *text_block_open = false;
                    events.push(StreamEvent::ContentBlockStop { index: 0 });
                }

                for tc in tool_calls {
                    let tc_index = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as u32;
                    // Offset tool indices by 1 since text uses index 0
                    let block_index = tc_index + 1;

                    if let Some(id) = tc.get("id").and_then(|i| i.as_str()) {
                        let name = tc
                            .get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(|n| n.as_str())
                            .unwrap_or("")
                            .to_string();
                        if !open_tool_blocks.contains(&block_index) {
                            open_tool_blocks.insert(block_index);
                            events.push(StreamEvent::ContentBlockStart {
                                index: block_index,
                                kind: ContentBlockKind::ToolCall,
                            });
                        }
                        events.push(StreamEvent::ToolCallStart {
                            index: block_index,
                            id: ToolCallId::from_provider(id),
                            name,
                        });
                    }

                    if let Some(args) = tc
                        .get("function")
                        .and_then(|f| f.get("arguments"))
                        .and_then(|a| a.as_str())
                    {
                        if !args.is_empty() {
                            events.push(StreamEvent::ToolCallArgumentsDelta {
                                index: block_index,
                                json_fragment: args.to_string(),
                            });
                        }
                    }
                }
            }
        }

        // finish_reason — close blocks and emit MessageStop
        if let Some(reason) = choice.get("finish_reason").and_then(|r| r.as_str()) {
            if !reason.is_empty() {
                *last_finish_reason = Some(reason.to_string());
                *last_event_kind = "finish";
                if *text_block_open {
                    *text_block_open = false;
                    events.push(StreamEvent::ContentBlockStop { index: 0 });
                }
                for idx in open_tool_blocks.drain() {
                    events.push(StreamEvent::ToolCallStop { index: idx });
                    events.push(StreamEvent::ContentBlockStop { index: idx });
                }
                events.push(StreamEvent::MessageStop {
                    stop_reason: StopReason::from_provider_string(reason),
                    stop_sequence: None,
                });
                // Some providers (e.g. Copilot) close the connection right
                // after the final choice without sending an explicit `[DONE]`
                // sentinel. Mark the stream complete here so the caller drops
                // the trailing transport error instead of WARNing on it.
                *stream_completed = true;
            }
        }
    }

    if let Some(usage) = v
        .get("usage")
        .filter(|u| !u.is_null())
        .and_then(|_| parse_usage(&v))
    {
        events.push(StreamEvent::UsageDelta { usage });
    }

    Ok(events)
}

fn parse_usage(v: &serde_json::Value) -> Option<Usage> {
    let u = v.get("usage")?;
    Some(Usage {
        input_tokens: u
            .get("prompt_tokens")
            .and_then(|x| x.as_u64())
            .map(|x| x as u32),
        output_tokens: u
            .get("completion_tokens")
            .and_then(|x| x.as_u64())
            .map(|x| x as u32),
        cache_creation_input_tokens: u
            .get("cache_creation_input_tokens")
            .and_then(|x| x.as_u64())
            .map(|x| x as u32),
        cache_read_input_tokens: u
            .get("cache_read_input_tokens")
            .or_else(|| {
                u.get("prompt_tokens_details")
                    .and_then(|d| d.get("cached_tokens"))
            })
            .and_then(|x| x.as_u64())
            .map(|x| x as u32),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Serve a chunked HTTP/1.1 response that streams a single OpenAI-style
    /// chunk with `finish_reason=stop`, then `data: [DONE]`, and then drops the
    /// connection without sending the terminating `0\r\n\r\n` chunk. `reqwest`
    /// surfaces this as a body-decode transport error — exactly what we see
    /// from the GitHub Copilot upstream in production.
    #[tokio::test]
    async fn transport_error_after_done_is_dropped() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();

            // Drain the request headers up to the blank line so the client
            // doesn't see a connection reset mid-send.
            let mut buf = [0u8; 4096];
            let mut total = 0usize;
            loop {
                let n = socket.read(&mut buf[total..]).await.unwrap();
                if n == 0 {
                    break;
                }
                total += n;
                if buf[..total].windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
                if total == buf.len() {
                    break;
                }
            }

            let header = b"HTTP/1.1 200 OK\r\n\
                Content-Type: text/event-stream\r\n\
                Transfer-Encoding: chunked\r\n\
                \r\n";
            socket.write_all(header).await.unwrap();

            // Chunk 1: a final OpenAI-style delta with finish_reason=stop.
            let chunk1 = b"data: {\"id\":\"chatcmpl-1\",\"model\":\"copilot-test\",\"created\":1700000000,\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\n";
            let chunk1_frame = format!("{:x}\r\n", chunk1.len());
            socket.write_all(chunk1_frame.as_bytes()).await.unwrap();
            socket.write_all(chunk1).await.unwrap();
            socket.write_all(b"\r\n").await.unwrap();

            // Chunk 2: the [DONE] sentinel.
            let chunk2 = b"data: [DONE]\n\n";
            let chunk2_frame = format!("{:x}\r\n", chunk2.len());
            socket.write_all(chunk2_frame.as_bytes()).await.unwrap();
            socket.write_all(chunk2).await.unwrap();
            socket.write_all(b"\r\n").await.unwrap();
            socket.flush().await.unwrap();

            // Deliberately drop the connection BEFORE the `0\r\n\r\n` chunk
            // terminator. reqwest will surface this as a transport error on
            // the body stream — same shape as the warning we are silencing.
            drop(socket);
        });

        let url = format!("http://{}/", addr);
        let response = reqwest::Client::new()
            .get(&url)
            .send()
            .await
            .expect("request should succeed");

        let stream = response.bytes_stream();
        let canonical = parse(stream);

        let collected: Vec<Result<StreamEvent, StreamError>> = canonical.collect().await;

        // No upstream error should have been propagated downstream.
        let errs: Vec<_> = collected.iter().filter_map(|r| r.as_ref().err()).collect();
        assert!(
            errs.is_empty(),
            "expected no StreamError after [DONE], got: {:?}",
            errs
        );

        // The clean termination still produced a ResponseStop.
        let response_stops: usize = collected
            .iter()
            .filter(|r| matches!(r, Ok(StreamEvent::ResponseStop { .. })))
            .count();
        assert_eq!(
            response_stops, 1,
            "expected exactly one ResponseStop event, got {response_stops}"
        );
    }
}
