//! Cancellation and completion-loss behavior of the production durable owner.

use super::*;
use crate::{
    core::{ModuleInstanceId, ModuleSlotId, ProviderId},
    durable::QueueErrorKind,
};
use camino::Utf8PathBuf;
use tempfile::TempDir;

fn scope() -> LegacyDiscordScope {
    LegacyDiscordScope::new(ModuleInstanceId::new(
        ProviderId::new("discord").unwrap(),
        ModuleSlotId::new("primary").unwrap(),
    ))
}

fn root(directory: &TempDir) -> Utf8PathBuf {
    Utf8PathBuf::from_path_buf(directory.path().to_owned()).unwrap()
}

#[tokio::test]
async fn cancelled_admission_finishes_owned_write_and_wakes_consumer() {
    let directory = TempDir::new().unwrap();
    let path = root(&directory);
    let queue = CodexEventQueue::open(&path, scope()).await.unwrap();
    let consumer = queue
        .register_consumer("reader".into(), DEFAULT_CONSUMER_TTL, true, true)
        .await
        .unwrap()
        .consumer_id;
    let changed = queue.test_changed();
    let notified = changed.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    let (reached, release) = queue.pause_owned_completion_for_test(false);
    let producer = queue.clone();
    let admission = tokio::spawn(async move {
        producer
            .enqueue(json!({"literal": "retained after cancellation"}))
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), reached)
        .await
        .unwrap()
        .unwrap();
    admission.abort();
    assert!(admission.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(2), notified)
        .await
        .expect("owned mutation wakes a pre-registered waiter despite caller cancellation");
    assert!(
        tokio::time::timeout(Duration::from_millis(20), queue.drain_owned())
            .await
            .is_err()
    );
    let reader = queue.clone();
    let reader_id = consumer.clone();
    let poll = tokio::spawn(async move {
        reader
            .next_event(&reader_id, Duration::from_secs(2), DEFAULT_LEASE)
            .await
    });
    release.send(()).unwrap();
    let event = poll.await.unwrap().unwrap().unwrap();
    assert_eq!(
        event.event,
        json!({"literal": "retained after cancellation"})
    );
    queue
        .acknowledge(&consumer, &event.delivery_token)
        .await
        .unwrap();
    queue.drain_owned().await;
    drop(queue);
    let reopened = CodexEventQueue::open(&path, scope()).await.unwrap();
    assert_eq!(reopened.status().await.queued, 0);
}

#[tokio::test]
async fn cancelled_binding_publishes_before_next_owned_binding() {
    let directory = TempDir::new().unwrap();
    let queue = CodexEventQueue::open(&root(&directory), scope())
        .await
        .unwrap();
    let (binding, mut watched) = tokio::sync::watch::channel(None);
    let controller = queue.binding_controller();
    let (wait, release) = queue.pause_binding_publish_for_test();
    let first_queue = controller.clone();
    let first_binding = binding.clone();
    let first = tokio::spawn(async move {
        first_queue
            .bind_live_thread_and_publish(
                Some(CodexThreadId::parse("first").unwrap()),
                &first_binding,
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), wait)
        .await
        .unwrap()
        .unwrap();
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    assert!(watched.borrow().is_none());
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), watched.changed())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        watched.borrow_and_update().as_ref().unwrap().as_str(),
        "first"
    );
    controller
        .bind_live_thread_and_publish(Some(CodexThreadId::parse("second").unwrap()), &binding)
        .await
        .unwrap();
    assert_eq!(watched.borrow().as_ref().unwrap().as_str(), "second");
    let persisted: Value = serde_json::from_slice(
        &tokio::fs::read(root(&directory).join("codex-inbox.json"))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(persisted["live_thread_id"], "second");
}

#[tokio::test]
async fn lost_owned_response_preserves_original_cause_and_retained_event() {
    let directory = TempDir::new().unwrap();
    let queue = CodexEventQueue::open(&root(&directory), scope())
        .await
        .unwrap();
    let (reached, release) = queue.pause_owned_completion_for_test(true);
    let producer = queue.clone();
    let admission = tokio::spawn(async move {
        producer
            .enqueue(json!({"literal": "unknown completion"}))
            .await
    });
    reached.await.unwrap();
    release.send(()).unwrap();
    let error = admission.await.unwrap().unwrap_err();
    assert!(
        matches!(error.kind, CodexQueueErrorKind::Durable(crate::durable::QueueError {kind:QueueErrorKind::CompletionUnknown(source),..}) if source.is_panic())
    );
    assert_eq!(queue.status().await.queued, 1);
    assert!(
        queue
            .enqueue(json!({"literal": "subsequent valid work"}))
            .await
            .unwrap()
    );
    assert_eq!(queue.status().await.queued, 2);
}

#[tokio::test]
async fn binding_control_preserves_public_primary_without_registering_a_resident() {
    let directory = TempDir::new().unwrap();
    let queue = CodexEventQueue::open(&root(&directory), scope())
        .await
        .unwrap();
    let controller = queue.binding_controller();
    assert!(queue.status().await.consumers.is_empty());
    let consumer = queue
        .register_consumer("public reader".into(), DEFAULT_CONSUMER_TTL, true, true)
        .await
        .unwrap()
        .consumer_id;
    let (binding, watched) = tokio::sync::watch::channel(None);
    controller
        .bind_live_thread_and_publish(Some(CodexThreadId::parse("thread-a").unwrap()), &binding)
        .await
        .unwrap();
    assert_eq!(watched.borrow().as_ref().unwrap().as_str(), "thread-a");
    let status = queue.status().await;
    assert_eq!(status.primary_consumer, Some(consumer.clone()));
    assert_eq!(status.consumers.len(), 1);
    queue
        .enqueue(json!({"literal":"public work"}))
        .await
        .unwrap();
    let leased = queue
        .next_event(&consumer, Duration::ZERO, DEFAULT_LEASE)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(leased.event, json!({"literal":"public work"}));
    let key = crate::delivery::ProviderEventKey::teams(
        "tenant".parse().unwrap(),
        "bot".parse().unwrap(),
        "conversation".parse().unwrap(),
        "activity".parse().unwrap(),
    );
    assert!(matches!(
        queue
            .enqueue_live(json!({"literal":"private work"}), key)
            .await
            .unwrap_err()
            .kind,
        LiveQueueErrorKind::ResidentUnavailable(_)
    ));
}
