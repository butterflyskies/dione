//! Shared, bounded serialization for MCP responses and notifications.
use serde_json::Value;
use std::{io, sync::Arc, time::Duration};
use thiserror::Error;
use tokio::{
    io::{AsyncWrite, AsyncWriteExt},
    sync::Mutex,
    time::timeout,
};
use tokio_util::sync::CancellationToken;

const FRAME_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Error)]
#[error("failed to write protocol frame: {kind}")]
pub(crate) struct ProtocolOutputError {
    #[source]
    kind: ProtocolOutputErrorKind,
    attempt: WriteAttempt,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WriteAttempt {
    NotStarted,
    Started,
}

impl ProtocolOutputError {
    pub(crate) fn completion_is_unknown(&self) -> bool {
        match self.attempt {
            WriteAttempt::NotStarted => false,
            WriteAttempt::Started => true,
        }
    }
}

#[derive(Debug, Error)]
enum ProtocolOutputErrorKind {
    #[error("cannot serialize frame")]
    Serialize(#[source] serde_json::Error),
    #[error("stream I/O failed")]
    Io(#[source] io::Error),
    #[error("frame deadline elapsed")]
    Timeout,
    #[error("stream is poisoned")]
    Poisoned,
}

#[derive(Clone)]
pub(crate) struct ProtocolOutput {
    writer: Arc<Mutex<Box<dyn AsyncWrite + Send + Unpin>>>,
    poisoned: CancellationToken,
}

struct PendingFrame(Option<CancellationToken>);

impl Drop for PendingFrame {
    fn drop(&mut self) {
        if let Some(poisoned) = &self.0 {
            poisoned.cancel();
        }
    }
}

impl ProtocolOutput {
    pub(crate) fn new(writer: impl AsyncWrite + Send + Unpin + 'static) -> Self {
        Self {
            writer: Arc::new(Mutex::new(Box::new(writer))),
            poisoned: CancellationToken::new(),
        }
    }

    pub(crate) async fn failed(&self) {
        self.poisoned.cancelled().await;
    }

    pub(crate) async fn write(&self, value: &Value) -> Result<(), ProtocolOutputError> {
        if self.poisoned.is_cancelled() {
            return Err(ProtocolOutputError {
                kind: ProtocolOutputErrorKind::Poisoned,
                attempt: WriteAttempt::NotStarted,
            });
        }
        let mut line = serde_json::to_vec(value).map_err(|error| {
            self.poisoned.cancel();
            ProtocolOutputError {
                kind: ProtocolOutputErrorKind::Serialize(error),
                attempt: WriteAttempt::NotStarted,
            }
        })?;
        line.push(b'\n');
        let mut attempt = WriteAttempt::NotStarted;
        let write = async {
            let mut writer = self.writer.lock().await;
            if self.poisoned.is_cancelled() {
                return Err(ProtocolOutputErrorKind::Poisoned);
            }
            // The guard drops before the mutex so a truncated frame cannot be followed.
            let mut pending = PendingFrame(Some(self.poisoned.clone()));
            tokio::select! {
                biased;
                _ = self.poisoned.cancelled() => Err(ProtocolOutputErrorKind::Poisoned),
                result = async {
                    attempt = WriteAttempt::Started;
                    writer.write_all(&line).await?;
                    writer.flush().await
                } => {
                    result.map_err(ProtocolOutputErrorKind::Io)?;
                    if self.poisoned.is_cancelled() {
                        return Err(ProtocolOutputErrorKind::Poisoned);
                    }
                    pending.0 = None;
                    Ok(())
                }
            }
        };
        match timeout(FRAME_TIMEOUT, write).await {
            Ok(result) => result.map_err(|kind| ProtocolOutputError { kind, attempt }),
            Err(_) => {
                self.poisoned.cancel();
                Err(ProtocolOutputError {
                    kind: ProtocolOutputErrorKind::Timeout,
                    attempt,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::{HarnessConnection, test_support};
    use serde_json::json;
    use std::{
        pin::Pin,
        task::{Context, Poll},
    };
    use tokio::{
        io::{AsyncBufReadExt, AsyncReadExt, BufReader},
        time::{Instant, sleep},
    };

    enum WriterBehavior {
        BlockedWrite,
        BlockedFlush,
        FailedFlush,
    }

    struct TestWriter(WriterBehavior);

    impl AsyncWrite for TestWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            match self.0 {
                WriterBehavior::BlockedWrite => Poll::Pending,
                WriterBehavior::BlockedFlush | WriterBehavior::FailedFlush => {
                    Poll::Ready(Ok(bytes.len()))
                }
            }
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            match self.0 {
                WriterBehavior::BlockedWrite | WriterBehavior::BlockedFlush => Poll::Pending,
                WriterBehavior::FailedFlush => Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "flush failed",
                ))),
            }
        }

        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn adapters_distinguish_partial_write_timeout_from_unattempted_poison_failure() {
        for adapter in [
            HarnessConnection::claude_with_output,
            HarnessConnection::oh_my_pi_with_output,
        ] {
            let (writer, mut reader) = tokio::io::duplex(1);
            let connection = adapter(ProtocolOutput::new(writer));
            let error = connection
                .deliver(&test_support::text("hello"))
                .await
                .unwrap_err();
            assert!(error.completion_is_unknown());
            let mut first_byte = [0; 1];
            reader.read_exact(&mut first_byte).await.unwrap();
            assert_eq!(first_byte, [b'{']);
            let error = connection
                .deliver(&test_support::text("never written"))
                .await
                .unwrap_err();
            assert!(!error.completion_is_unknown());
        }
    }

    #[tokio::test]
    async fn failed_flush_is_unknown_and_preserves_its_cause() {
        let output = ProtocolOutput::new(TestWriter(WriterBehavior::FailedFlush));
        let error = output.write(&json!({"id":1})).await.unwrap_err();
        assert!(error.completion_is_unknown());
        match error.kind {
            ProtocolOutputErrorKind::Io(error) => {
                assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
                assert_eq!(error.to_string(), "flush failed");
            }
            other => panic!("expected flush failure, received {other:?}"),
        }
        let error = output.write(&json!({"id":2})).await.unwrap_err();
        assert!(!error.completion_is_unknown());
    }

    #[tokio::test]
    async fn concurrent_frames_remain_complete_json_lines() {
        let (writer, reader) = tokio::io::duplex(8);
        let output = ProtocolOutput::new(writer);
        let response = json!({"jsonrpc":"2.0","id":1,"result":{}});
        let discord = json!({"jsonrpc":"2.0","method":"notifications/dione/event","params":{"provider":"discord"}});
        let teams = json!({"jsonrpc":"2.0","method":"notifications/dione/event","params":{"provider":"teams"}});
        let read = async {
            let mut lines = BufReader::new(reader).lines();
            let mut received = Vec::new();
            for _ in 0..3 {
                received.push(
                    serde_json::from_str::<Value>(&lines.next_line().await.unwrap().unwrap())
                        .unwrap(),
                );
            }
            received
        };
        let (first, second, third, received) = tokio::join!(
            output.write(&response),
            output.write(&discord),
            output.write(&teams),
            read
        );
        first.unwrap();
        second.unwrap();
        third.unwrap();
        assert_eq!(received.len(), 3);
        assert!(received.contains(&response));
        assert!(received.contains(&discord));
        assert!(received.contains(&teams));
    }

    #[tokio::test(start_paused = true)]
    async fn blocked_write_has_bounded_deadline_and_poisons_stream() {
        assert_stalled_deadline(WriterBehavior::BlockedWrite).await;
    }

    #[tokio::test(start_paused = true)]
    async fn blocked_flush_has_bounded_deadline_and_poisons_stream() {
        assert_stalled_deadline(WriterBehavior::BlockedFlush).await;
    }

    async fn assert_stalled_deadline(at: WriterBehavior) {
        let output = ProtocolOutput::new(TestWriter(at));
        let start = Instant::now();
        let error = output.write(&json!({"id":1})).await.unwrap_err();
        assert!(error.completion_is_unknown());
        assert!(matches!(error.kind, ProtocolOutputErrorKind::Timeout));
        assert_eq!(start.elapsed(), Duration::from_secs(10));
        output.failed().await;
        assert!(matches!(
            output.write(&json!({"id":2})).await.unwrap_err().kind,
            ProtocolOutputErrorKind::Poisoned
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn blocked_mutex_has_bounded_deadline_and_poisons_stream() {
        let output = ProtocolOutput::new(tokio::io::sink());
        let guard = output.writer.lock().await;
        let start = Instant::now();
        let error = output.write(&json!({"id":1})).await.unwrap_err();
        assert!(!error.completion_is_unknown());
        assert!(matches!(error.kind, ProtocolOutputErrorKind::Timeout));
        assert_eq!(start.elapsed(), Duration::from_secs(10));
        drop(guard);
        assert!(matches!(
            output.write(&json!({"id":2})).await.unwrap_err().kind,
            ProtocolOutputErrorKind::Poisoned
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn mutex_and_flush_share_one_deadline() {
        let output = ProtocolOutput::new(TestWriter(WriterBehavior::BlockedFlush));
        let guard = output.writer.lock().await;
        let release = async {
            sleep(Duration::from_secs(6)).await;
            drop(guard);
        };
        let frame = json!({"id":1});
        let start = Instant::now();
        let (result, ()) = tokio::join!(output.write(&frame), release);
        assert!(matches!(
            result.unwrap_err().kind,
            ProtocolOutputErrorKind::Timeout
        ));
        assert_eq!(start.elapsed(), Duration::from_secs(10));
    }

    #[tokio::test]
    async fn broken_pipe_preserves_error_and_rejects_later_frames() {
        let (writer, reader) = tokio::io::duplex(8);
        drop(reader);
        let output = ProtocolOutput::new(writer);
        let error = output.write(&json!({"id":1})).await.unwrap_err();
        assert!(error.completion_is_unknown());
        match error.kind {
            ProtocolOutputErrorKind::Io(error) => {
                assert_eq!(error.kind(), io::ErrorKind::BrokenPipe)
            }
            other => panic!("expected broken pipe, received {other:?}"),
        }
        output.failed().await;
        assert!(matches!(
            output.write(&json!({"id":2})).await.unwrap_err().kind,
            ProtocolOutputErrorKind::Poisoned
        ));
    }

    #[tokio::test]
    async fn cancelling_partial_frame_poisons_before_another_writer_acquires_lock() {
        let (writer, mut reader) = tokio::io::duplex(1);
        let output = ProtocolOutput::new(writer);
        let frame = json!({"message":"cannot complete without reader"});
        let mut write = Box::pin(output.write(&frame));
        let mut first_byte = [0; 1];
        tokio::select! {
            result = &mut write => panic!("unexpected completion: {result:?}"),
            result = reader.read_exact(&mut first_byte) => { result.unwrap(); }
        }
        assert_eq!(first_byte, [b'{']);
        drop(write);
        output.failed().await;
        assert!(matches!(
            output
                .write(&json!({"second":true}))
                .await
                .unwrap_err()
                .kind,
            ProtocolOutputErrorKind::Poisoned
        ));
        drop(output);
        let mut tail = Vec::new();
        reader.read_to_end(&mut tail).await.unwrap();
        assert!(tail.len() <= 1);
        assert!(!tail.contains(&b'\n'));
    }

    #[tokio::test(start_paused = true)]
    async fn cancelling_mutex_waiter_preserves_healthy_stream() {
        let (writer, reader) = tokio::io::duplex(64);
        let output = ProtocolOutput::new(writer);
        let guard = output.writer.lock().await;
        let frame = json!({"cancelled":true});
        let mut write = Box::pin(output.write(&frame));
        tokio::select! {
            result = &mut write => panic!("unexpected completion: {result:?}"),
            () = sleep(Duration::from_secs(1)) => {}
        }
        drop(write);
        drop(guard);
        output.write(&json!({"accepted":true})).await.unwrap();
        let mut lines = BufReader::new(reader).lines();
        assert_eq!(
            lines.next_line().await.unwrap().unwrap(),
            r#"{"accepted":true}"#
        );
    }
}
