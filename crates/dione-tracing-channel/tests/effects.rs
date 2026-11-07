use dione_discord_events::NotificationEvent;
use dione_tracing_channel::{TraceLevelController, TracingChannelLayer, spawn_forwarder};
use std::{
    io,
    sync::{Arc, Mutex},
};
use tokio::{
    sync::mpsc,
    time::{Duration, timeout},
};
use tracing::{Level, subscriber::with_default};
use tracing_subscriber::{EnvFilter, prelude::*, reload};

#[derive(Clone, Default)]
struct StderrBuffer(Arc<Mutex<Vec<u8>>>);
impl io::Write for StderrBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for StderrBuffer {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}
impl StderrBuffer {
    fn drain(&self) -> Vec<String> {
        let bytes = std::mem::take(&mut *self.0.lock().unwrap());
        String::from_utf8(bytes)
            .unwrap()
            .lines()
            .map(|line| line.trim().to_owned())
            .collect()
    }
}

fn trace(message: &str) -> NotificationEvent {
    NotificationEvent::Trace {
        level: Level::INFO,
        target: "forward-test".to_owned(),
        message: message.to_owned(),
        fields: vec![("source".to_owned(), "fixture".to_owned())],
    }
}

fn assert_trace(event: NotificationEvent, expected: (Level, &str, &str, &[(&str, &str)])) {
    let NotificationEvent::Trace {
        level,
        target,
        message,
        fields,
    } = event
    else {
        panic!("expected trace");
    };
    assert_eq!(
        (level, target.as_str(), message.as_str()),
        (expected.0, expected.1, expected.2)
    );
    assert_eq!(
        fields,
        expected
            .3
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect::<Vec<_>>()
    );
}

#[test]
fn actual_events_preserve_metadata_message_and_ordered_fields() {
    let (layer, mut rx) = TracingChannelLayer::new();
    with_default(tracing_subscriber::registry().with(layer), || {
        tracing::event!(target: "literal_target", Level::WARN, first = "plain", second = ?vec![2, 3], message = "literal message");
        tracing::event!(target: "absent_message", Level::INFO, number = 42);
        tracing::event!(target: "debug_message", Level::DEBUG, message = ?vec!["a", "b"], tail = "last");
    });
    assert_trace(
        rx.try_recv().unwrap(),
        (
            Level::WARN,
            "literal_target",
            "literal message",
            &[("first", "plain"), ("second", "[2, 3]")],
        ),
    );
    assert_trace(
        rx.try_recv().unwrap(),
        (Level::INFO, "absent_message", "", &[("number", "42")]),
    );
    assert_trace(
        rx.try_recv().unwrap(),
        (
            Level::DEBUG,
            "debug_message",
            "[\"a\", \"b\"]",
            &[("tail", "last")],
        ),
    );
    assert!(rx.try_recv().is_err());
}

#[test]
fn full_capture_queue_drops_extra_event_without_blocking_or_reordering() {
    let (layer, mut rx) = TracingChannelLayer::new();
    with_default(tracing_subscriber::registry().with(layer), || {
        for number in 0..257 {
            tracing::info!(target: "bounded", number, "queued");
        }
    });
    for number in 0..256 {
        let NotificationEvent::Trace {
            message, fields, ..
        } = rx.try_recv().unwrap()
        else {
            panic!("trace");
        };
        assert_eq!(message, "queued");
        assert_eq!(fields, vec![("number".to_owned(), number.to_string())]);
    }
    assert!(rx.try_recv().is_err());
    let (layer, rx) = TracingChannelLayer::new();
    drop(rx);
    with_default(tracing_subscriber::registry().with(layer), || {
        tracing::error!("closed capture")
    });
}

#[test]
fn real_reload_changes_only_its_destination_and_invalid_filters_preserve_state() {
    let stderr = StderrBuffer::default();
    let (channel, mut channel_rx) = TracingChannelLayer::new();
    let (stderr_filter, stderr_handle) = reload::Layer::new(EnvFilter::new("warn"));
    let (channel_filter, channel_handle) = reload::Layer::new(EnvFilter::new("off"));
    let subscriber = tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .without_time()
                .with_ansi(false)
                .with_target(false)
                .with_writer(stderr.clone())
                .with_filter(stderr_filter),
        )
        .with(channel.with_filter(channel_filter));
    let controller = TraceLevelController::new(stderr_handle, channel_handle);
    with_default(subscriber, || {
        let emit = || {
            tracing::info!(target: "reload_target", "info");
            tracing::warn!(target: "reload_target", "warn");
        };
        emit();
        assert_eq!(stderr.drain(), ["WARN warn"]);
        assert!(channel_rx.try_recv().is_err());
        controller.clone().set_channel_filter("info").unwrap();
        emit();
        assert_eq!(stderr.drain(), ["WARN warn"]);
        assert_trace(
            channel_rx.try_recv().unwrap(),
            (Level::INFO, "reload_target", "info", &[]),
        );
        assert_trace(
            channel_rx.try_recv().unwrap(),
            (Level::WARN, "reload_target", "warn", &[]),
        );
        assert!(
            controller
                .set_channel_filter("[broken")
                .unwrap_err()
                .starts_with("invalid filter: ")
        );
        assert!(
            controller
                .set_stderr_filter("[broken")
                .unwrap_err()
                .starts_with("invalid filter: ")
        );
        emit();
        assert_eq!(stderr.drain(), ["WARN warn"]);
        assert_trace(
            channel_rx.try_recv().unwrap(),
            (Level::INFO, "reload_target", "info", &[]),
        );
        assert_trace(
            channel_rx.try_recv().unwrap(),
            (Level::WARN, "reload_target", "warn", &[]),
        );
        controller.set_stderr_filter("off").unwrap();
        emit();
        assert!(stderr.drain().is_empty());
        assert_trace(
            channel_rx.try_recv().unwrap(),
            (Level::INFO, "reload_target", "info", &[]),
        );
        assert_trace(
            channel_rx.try_recv().unwrap(),
            (Level::WARN, "reload_target", "warn", &[]),
        );
        assert!(channel_rx.try_recv().is_err());
    });
    assert!(
        controller
            .set_stderr_filter("info")
            .unwrap_err()
            .starts_with("reload failed: ")
    );
    assert!(
        controller
            .set_channel_filter("info")
            .unwrap_err()
            .starts_with("reload failed: ")
    );
    let noop = TraceLevelController::noop();
    noop.set_stderr_filter("info").unwrap();
    noop.set_channel_filter("warn").unwrap();
    assert!(
        noop.set_channel_filter("[broken")
            .unwrap_err()
            .starts_with("invalid filter: ")
    );
}

#[tokio::test]
async fn forwarding_waits_for_capacity_then_drains_in_order_and_exits() {
    let (upstream, rx) = mpsc::channel(1);
    let (downstream, mut output) = mpsc::channel(1);
    downstream.send(trace("already queued")).await.unwrap();
    upstream.send(trace("first")).await.unwrap();
    let task = spawn_forwarder(rx, downstream);
    timeout(Duration::from_secs(2), async {
        while upstream.capacity() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    upstream.send(trace("second")).await.unwrap();
    tokio::task::yield_now().await;
    assert!(
        !task.is_finished(),
        "full downstream must retain pending first send"
    );
    assert!(matches!(
        upstream.try_send(trace("third")),
        Err(mpsc::error::TrySendError::Full(_))
    ));
    drop(upstream);
    for message in ["already queued", "first", "second"] {
        let event = timeout(Duration::from_secs(2), output.recv())
            .await
            .unwrap()
            .unwrap();
        assert_trace(
            event,
            (
                Level::INFO,
                "forward-test",
                message,
                &[("source", "fixture")],
            ),
        );
    }
    timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap();
    assert!(output.recv().await.is_none());
}

#[tokio::test]
async fn forwarding_exits_when_downstream_closes_with_upstream_still_open() {
    let (upstream, rx) = mpsc::channel(2);
    upstream.send(trace("first")).await.unwrap();
    upstream.send(trace("second")).await.unwrap();
    let (downstream, output) = mpsc::channel(1);
    drop(output);
    timeout(Duration::from_secs(2), spawn_forwarder(rx, downstream))
        .await
        .unwrap()
        .unwrap();
    assert!(upstream.is_closed());
}
