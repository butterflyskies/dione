//! Claude channel delivery over the shared MCP writer.
use super::{
    AttentionPolicy, HarnessConnection, HarnessDelivery, HarnessError, ProtocolOutput,
    codex::{ToolArgumentError, recognizes},
};
use crate::{
    core::{MessageRef, delivery::Delivery, events::PermissionDecision},
    mcp::harness_wire::HarnessProtocol,
};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
pub(super) const ATTENTION_POLICY: AttentionPolicy = AttentionPolicy::Direct;

pub(crate) fn notification(delivery: &Delivery) -> Value {
    match super::presentation::project(delivery) {
        super::presentation::Presentation::Channel(channel) => {
            let meta: serde_json::Map<String, Value> = channel
                .meta
                .into_iter()
                .map(|(key, value)| {
                    let value = match value {
                        super::presentation::MetadataValue::Text(value) => value,
                        super::presentation::MetadataValue::Flag(value) => value.to_string(),
                        super::presentation::MetadataValue::Locators(value) => {
                            json!(value).to_string()
                        }
                    };
                    (key.to_owned(), Value::String(value))
                })
                .collect();
            json!({"jsonrpc":"2.0","method":"notifications/claude/channel","params":{"content":channel.content,"meta":meta}})
        }
        super::presentation::Presentation::Permission { request, decision } => {
            json!({"jsonrpc":"2.0","method":"notifications/claude/channel/permission","params":{"request_id":request,"behavior":match decision {PermissionDecision::Granted=>"allow",PermissionDecision::Denied=>"deny"}}})
        }
    }
}

struct ClaudeAdapter {
    output: ProtocolOutput,
}
impl HarnessConnection {
    /// Connects Claude channel notifications and responses to one stdout writer.
    pub fn claude() -> Self {
        Self::claude_with_output(ProtocolOutput::new(tokio::io::stdout()))
    }
    pub(crate) fn claude_with_output(output: ProtocolOutput) -> Self {
        Self::from_adapter(
            ClaudeAdapter {
                output: output.clone(),
            },
            output,
        )
    }
}
impl HarnessDelivery for ClaudeAdapter {
    fn attention_policy(&self) -> AttentionPolicy {
        ATTENTION_POLICY
    }
    fn deliver<'a>(&'a self, value: &'a Delivery) -> BoxFuture<'a, Result<(), HarnessError>> {
        Box::pin(async move {
            self.output
                .write(&notification(value))
                .await
                .map_err(|error| {
                    if error.completion_is_unknown() {
                        HarnessError::completion_unknown(error)
                    } else {
                        HarnessError::source(error)
                    }
                })?;
            Ok(())
        })
    }
    fn invalidate_attention(&self, _source: MessageRef) -> BoxFuture<'_, Result<(), HarnessError>> {
        Box::pin(async { Ok(()) })
    }
}
impl HarnessProtocol for ClaudeAdapter {
    fn capabilities(&self) -> Value {
        json!({"tools": {}, "experimental": {"claude/channel": {}, "claude/channel/permission": {}}})
    }
    fn tools(&self) -> Vec<Value> {
        Vec::new()
    }
    fn resident_control(&self) -> bool {
        false
    }
    fn permission_relay(&self) -> bool {
        true
    }
    fn call<'a>(
        &'a self,
        name: &'a str,
        _args: &'a Value,
    ) -> BoxFuture<'a, Option<Result<Value, HarnessError>>> {
        Box::pin(async move {
            recognizes(name).then(|| {
                Err(HarnessError::source(ToolArgumentError(format!(
                    "{name} is only available in Codex mode"
                ))))
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io,
        pin::Pin,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        task::{Context, Poll},
    };
    use tokio::io::{AsyncBufReadExt, AsyncWrite, BufReader};

    #[tokio::test]
    async fn channel_metadata_uses_strings_without_changing_content() {
        let (writer, reader) = tokio::io::duplex(4096);
        let connection = HarnessConnection::claude_with_output(ProtocolOutput::new(writer));
        let content = "original text\n\"quoted\" <source> 🛰";
        let delivery = crate::harness::test_support::text(content);
        connection.deliver(&delivery).await.unwrap();
        let mut lines = BufReader::new(reader).lines();
        let delivered: Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(
            delivered,
            json!({
                "jsonrpc":"2.0","method":"notifications/claude/channel",
                "params":{"content":content,"meta":{"level":"INFO","target":"fixture","type":"trace"}}
            })
        );
    }

    struct FailedWriter {
        flushes: Arc<AtomicUsize>,
    }
    impl AsyncWrite for FailedWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _ctx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "closed consumer",
            )))
        }
        fn poll_flush(self: Pin<&mut Self>, _ctx: &mut Context<'_>) -> Poll<io::Result<()>> {
            self.flushes.fetch_add(1, Ordering::SeqCst);
            Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "closed consumer",
            )))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _ctx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn stdout_failure_is_reported_and_poisoned() {
        let flushes = Arc::new(AtomicUsize::new(0));
        let connection = HarnessConnection::claude_with_output(ProtocolOutput::new(FailedWriter {
            flushes: flushes.clone(),
        }));
        assert!(
            connection
                .deliver(&crate::harness::test_support::text("hello"))
                .await
                .is_err()
        );
        assert!(
            connection
                .output()
                .write(&json!({"response": "hello"}))
                .await
                .is_err()
        );
        assert_eq!(flushes.load(Ordering::SeqCst), 0);
        let (writer, reader) = tokio::io::duplex(1024);
        let connection = HarnessConnection::claude_with_output(ProtocolOutput::new(writer));
        connection
            .deliver(&crate::harness::test_support::text("hello"))
            .await
            .unwrap();
        let mut lines = BufReader::new(reader).lines();
        assert_eq!(
            lines.next_line().await.unwrap().unwrap(),
            r#"{"jsonrpc":"2.0","method":"notifications/claude/channel","params":{"content":"hello","meta":{"level":"INFO","target":"fixture","type":"trace"}}}"#
        );
    }

    #[tokio::test]
    async fn stdout_response_and_notification_share_complete_lines() {
        let (writer, reader) = tokio::io::duplex(32);
        let connection = HarnessConnection::claude_with_output(ProtocolOutput::new(writer));
        let response = json!({"jsonrpc": "2.0", "id": 7, "result": {"message": "response"}});
        let notification = crate::harness::test_support::text("notification");
        let read = async {
            let mut lines = BufReader::new(reader).lines();
            let mut values = Vec::new();
            for _ in 0..2 {
                values.push(
                    serde_json::from_str::<Value>(&lines.next_line().await.unwrap().unwrap())
                        .unwrap(),
                );
            }
            values
        };
        let (response_result, delivery, values) = tokio::join!(
            connection.output().write(&response),
            connection.deliver(&notification),
            read
        );
        response_result.unwrap();
        delivery.unwrap();
        assert_eq!(values.len(), 2);
        assert!(
            values.contains(&json!({"jsonrpc": "2.0", "id": 7, "result": {"message": "response"}}))
        );
        assert!(values.contains(&json!({"jsonrpc":"2.0","method":"notifications/claude/channel","params":{"content":"notification","meta":{"level":"INFO","target":"fixture","type":"trace"}}})));
    }
}
