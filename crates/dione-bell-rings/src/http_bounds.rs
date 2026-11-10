// SPDX-License-Identifier: Apache-2.0
// SSE event accounting is adapted from rmcp 3.9.0 client_side_sse.rs:
// https://github.com/modelcontextprotocol/rust-sdk/tree/rmcp-v3.9.0/crates/rmcp/src/transport/common
// The limiter is unchanged. A boxed stream/unfold replaces upstream pin projection;
// the failed state is checked before polling another network chunk.
use futures_util::{StreamExt, stream};
use rmcp::transport::{
    common::client_side_sse::BoxedSseResponse, streamable_http_client::StreamableHttpError,
};
use sse_stream::{Error as SseError, SseStream};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

pub(crate) const MAX_JSON_RESPONSE_SIZE: usize = 16 * 1024 * 1024;
pub(crate) const MAX_ERROR_RESPONSE_SIZE: usize = 64 * 1024;
pub(crate) const DEFAULT_MAX_SSE_EVENT_SIZE: usize = 16 * 1024 * 1024;

// Same streamed accounting as rmcp3.9 client_side_http::read_bounded_body.
// reqwest chunks are decompressed bytes; Content-Length is only an early guard.
pub(crate) async fn read_bounded_body(
    mut response: reqwest_mcp::Response,
    max_size: usize,
) -> Result<Vec<u8>, StreamableHttpError<reqwest_mcp::Error>> {
    if response
        .content_length()
        .is_some_and(|length| length > max_size as u64)
    {
        return Err(StreamableHttpError::ResponseBodyTooLarge { max_size });
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(StreamableHttpError::Client)?
    {
        if chunk.len() > max_size - body.len() {
            return Err(StreamableHttpError::ResponseBodyTooLarge { max_size });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[derive(Debug, Error)]
enum BoundedSseStreamError {
    #[error(transparent)]
    Source(Box<dyn std::error::Error + Send + Sync>),
    #[error("SSE event exceeded the maximum size of {max_size} bytes")]
    EventTooLarge { max_size: usize },
}

#[derive(Debug)]
struct SseEventSizeLimiter {
    max_size: usize,
    retained_size: usize,
    line_size: usize,
    line_is_comment: bool,
    previous_was_cr: bool,
}

impl SseEventSizeLimiter {
    fn new(max_size: usize) -> Self {
        Self {
            max_size,
            retained_size: 0,
            line_size: 0,
            line_is_comment: false,
            previous_was_cr: false,
        }
    }

    fn observe(&mut self, chunk: &[u8]) -> Result<(), ()> {
        for &byte in chunk {
            if self.previous_was_cr {
                self.previous_was_cr = false;
                if byte == b'\n' {
                    continue;
                }
            }

            match byte {
                b'\r' => {
                    self.finish_line()?;
                    self.previous_was_cr = true;
                }
                b'\n' => self.finish_line()?,
                _ => {
                    if self.line_size == 0 {
                        self.line_is_comment = byte == b':';
                    }
                    self.line_size = self.line_size.saturating_add(1);
                    self.check_limit()?;
                }
            }
        }
        Ok(())
    }

    fn finish_line(&mut self) -> Result<(), ()> {
        if self.line_size == 0 {
            self.retained_size = 0;
        } else if !self.line_is_comment {
            // The SSE parser inserts a newline when joining multiple data fields.
            self.retained_size = self
                .retained_size
                .saturating_add(self.line_size)
                .saturating_add(1);
        }
        self.line_size = 0;
        self.line_is_comment = false;
        self.check_limit()
    }

    fn check_limit(&self) -> Result<(), ()> {
        if self.retained_size.saturating_add(self.line_size) > self.max_size {
            Err(())
        } else {
            Ok(())
        }
    }
}

pub(crate) fn bounded_sse_response(
    response: reqwest_mcp::Response,
    max_event_size: usize,
    cancellation: CancellationToken,
) -> BoxedSseResponse {
    let chunks = response.bytes_stream().boxed();
    let stream = stream::unfold(
        (
            chunks,
            SseEventSizeLimiter::new(max_event_size),
            false,
            cancellation,
        ),
        |(mut chunks, mut limiter, failed, cancellation)| async move {
            if failed {
                return None;
            }
            match chunks.next().await {
                Some(Ok(chunk)) => match limiter.observe(&chunk) {
                    Ok(()) => Some((Ok(chunk), (chunks, limiter, false, cancellation))),
                    Err(()) => {
                        // Terminate this real transport, not merely this stream: the
                        // SDK cannot recognize a downstream private error type.
                        cancellation.cancel();
                        let error = BoundedSseStreamError::EventTooLarge {
                            max_size: limiter.max_size,
                        };
                        Some((Err(error), (chunks, limiter, true, cancellation)))
                    }
                },
                Some(Err(error)) => Some((
                    Err(BoundedSseStreamError::Source(Box::new(error))),
                    (chunks, limiter, true, cancellation),
                )),
                None => None,
            }
        },
    );
    SseStream::from_bytes_stream(stream)
        .map(|event| {
            event.map_err(|error| match error {
                // Keep the original reqwest body cause visible to existing consumers.
                SseError::Body(error) => match error.downcast::<BoundedSseStreamError>() {
                    Ok(error) => match *error {
                        BoundedSseStreamError::Source(source) => SseError::Body(source),
                        oversized => SseError::Body(Box::new(oversized)),
                    },
                    Err(error) => SseError::Body(error),
                },
                other => other,
            })
        })
        .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn response(raw: String) -> (reqwest_mcp::Response, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                socket.read_exact(&mut byte).await.unwrap();
                request.push(byte[0]);
                assert!(request.len() < 16_384);
            }
            socket.write_all(raw.as_bytes()).await.unwrap();
            // A reader that waits for EOF instead of rejecting overflow will stall.
            std::future::pending::<()>().await;
        });
        let response = reqwest_mcp::Client::new()
            .get(format!("http://{address}/SYNTHETIC_BOUND_CANARY"))
            .send()
            .await
            .unwrap();
        (response, server)
    }

    #[tokio::test]
    async fn declared_oversize_rejects_without_waiting_for_body() {
        let (response, server) =
            response("HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\n".into()).await;
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            read_bounded_body(response, 8),
        )
        .await
        .unwrap()
        .unwrap_err();
        server.abort();
        assert!(matches!(
            error,
            StreamableHttpError::ResponseBodyTooLarge { max_size: 8 }
        ));
        assert!(!format!("{error:?} {error}").contains("SYNTHETIC_BOUND_CANARY"));
    }

    #[tokio::test]
    async fn chunked_body_accepts_exact_limit_and_rejects_first_overflow() {
        for (limit, expected) in [(8, Some(b"abcdefgh".to_vec())), (7, None)] {
            let (response, server) = response("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nabcd\r\n4\r\nefgh\r\n0\r\n\r\n".into()).await;
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                read_bounded_body(response, limit),
            )
            .await
            .unwrap();
            server.abort();
            match expected {
                Some(bytes) => assert_eq!(result.unwrap(), bytes),
                None => assert!(matches!(
                    result,
                    Err(StreamableHttpError::ResponseBodyTooLarge { max_size: 7 })
                )),
            }
        }
    }

    #[tokio::test]
    async fn overflowing_chunk_does_not_wait_for_stream_termination() {
        let (response, server) = response(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n9\r\nabcdefghi\r\n".into(),
        )
        .await;
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            read_bounded_body(response, 8),
        )
        .await
        .unwrap()
        .unwrap_err();
        server.abort();
        assert!(matches!(
            error,
            StreamableHttpError::ResponseBodyTooLarge { max_size: 8 }
        ));
    }

    #[tokio::test]
    async fn sse_unterminated_event_rejects_then_stops_reading() {
        let (response, server) = response("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n10\r\ndata: abcdefghij\r\n".into()).await;
        let mut events = bounded_sse_response(response, 8, CancellationToken::new());
        let error = tokio::time::timeout(std::time::Duration::from_secs(5), events.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(format!("{error}").contains("maximum size of 8 bytes"));
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(5), events.next())
                .await
                .unwrap()
                .is_none()
        );
        server.abort();
    }

    #[test]
    fn sse_event_accounting_preserves_crlf_comments_and_multiline_data() {
        let mut limiter = SseEventSizeLimiter::new(8);
        assert!(limiter.observe(b"data:a\r").is_ok());
        assert!(limiter.observe(b"\n\r\n").is_ok());
        assert!(limiter.observe(b":comment\n").is_ok());
        assert!(limiter.observe(b"data:b\n").is_ok());
        assert!(
            limiter.observe(b"data:c").is_err(),
            "comments do not reset retained event data"
        );
    }
}
