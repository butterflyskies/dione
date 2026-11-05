use dione_discord_presence::{
    ActivityType, OnlineStatus, PresenceAcknowledgement, PresenceActivity, PresenceCapability,
    PresenceRequest, PresenceSetError, PresenceSink, run_presence_commands, wire_shared_presence,
};
use std::{
    error::Error,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc;

#[derive(Default)]
struct Recorder(Mutex<Vec<PresenceRequest>>);
impl PresenceSink for Recorder {
    fn set_presence(&self, request: &PresenceRequest) {
        self.0.lock().unwrap().push(request.clone());
    }
}

#[tokio::test]
async fn public_direct_replay_and_queue_preserve_requests_and_acknowledgements() {
    let (authority, direct) = wire_shared_presence();
    let first = PresenceRequest::new(
        OnlineStatus::Idle,
        Some(PresenceActivity::new(ActivityType::Custom, String::new())),
    );
    assert_eq!(
        direct.set(first.clone()).await.unwrap(),
        PresenceAcknowledgement::Applied
    );
    let before = direct.snapshot().await.unwrap();
    assert!(!before.sink_installed());
    assert_eq!(
        before.desired().unwrap().request().status(),
        OnlineStatus::Idle
    );
    assert_eq!(
        before
            .desired()
            .unwrap()
            .request()
            .activity()
            .unwrap()
            .name(),
        ""
    );
    let old = Arc::new(Recorder::default());
    authority.install(old.clone()).await;
    let new = Arc::new(Recorder::default());
    authority.install(new.clone()).await;
    assert_eq!(direct.snapshot().await.unwrap().desired(), before.desired());
    assert!(direct.snapshot().await.unwrap().sink_installed());
    let (tx, rx) = mpsc::channel(2);
    let queued = PresenceCapability::Queued(tx);
    let second = PresenceRequest::new(OnlineStatus::Dnd, None);
    assert_eq!(
        queued.set(second.clone()).await.unwrap(),
        PresenceAcknowledgement::Accepted
    );
    assert!(queued.snapshot().await.is_err());
    assert_eq!(direct.snapshot().await.unwrap().desired(), before.desired());
    drop(queued);
    run_presence_commands(authority, rx).await;
    assert_eq!(
        old.0.lock().unwrap().as_slice(),
        std::slice::from_ref(&first)
    );
    assert_eq!(*new.0.lock().unwrap(), [first, second.clone()]);
    assert_eq!(
        direct
            .snapshot()
            .await
            .unwrap()
            .desired()
            .unwrap()
            .request(),
        &second
    );
}

#[tokio::test]
async fn public_closed_and_unavailable_connections_preserve_typed_failures() {
    let (tx, rx) = mpsc::channel(1);
    drop(rx);
    let request = PresenceRequest::new(OnlineStatus::Invisible, None);
    let error = PresenceCapability::Queued(tx)
        .set(request.clone())
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "failed to send presence command: channel closed"
    );
    assert_eq!(error.source().unwrap().to_string(), "channel closed");
    match error {
        PresenceSetError::QueueClosed { source } => assert_eq!(source.0, request),
        PresenceSetError::Unavailable => panic!("closed queue remains a queue error"),
    }
    let unavailable = PresenceCapability::Unavailable;
    assert_eq!(
        unavailable.set(request).await.unwrap_err().to_string(),
        "presence updates require the discord gateway command channel"
    );
    assert_eq!(
        unavailable.snapshot().await.unwrap_err().to_string(),
        "presence state is not wired on this transport"
    );
}

#[test]
fn public_tokens_and_diagnostics_preserve_exact_spellings() {
    for (text, status) in [
        ("online", OnlineStatus::Online),
        ("idle", OnlineStatus::Idle),
        ("dnd", OnlineStatus::Dnd),
        ("invisible", OnlineStatus::Invisible),
    ] {
        assert_eq!(text.parse::<OnlineStatus>().unwrap(), status);
        assert_eq!(status.to_string(), text);
    }
    assert_eq!(
        OnlineStatus::ALL,
        &[
            OnlineStatus::Online,
            OnlineStatus::Idle,
            OnlineStatus::Dnd,
            OnlineStatus::Invisible
        ]
    );
    for (text, kind) in [
        ("playing", ActivityType::Playing),
        ("watching", ActivityType::Watching),
        ("listening", ActivityType::Listening),
        ("competing", ActivityType::Competing),
        ("custom", ActivityType::Custom),
    ] {
        assert_eq!(text.parse::<ActivityType>().unwrap(), kind);
        assert_eq!(kind.to_string(), text);
    }
    assert_eq!(
        ActivityType::ALL,
        &[
            ActivityType::Playing,
            ActivityType::Watching,
            ActivityType::Listening,
            ActivityType::Competing,
            ActivityType::Custom
        ]
    );
    assert_eq!(
        "ONLINE".parse::<OnlineStatus>().unwrap_err().to_string(),
        "invalid online_status: ONLINE; must be one of: online, idle, dnd, invisible"
    );
    assert_eq!(
        "Custom".parse::<ActivityType>().unwrap_err().to_string(),
        "invalid activity_type: Custom; must be one of: playing, watching, listening, competing, custom"
    );
}

struct BlockingRecorder {
    calls: Mutex<Vec<PresenceRequest>>,
    entered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
}
impl PresenceSink for BlockingRecorder {
    fn set_presence(&self, request: &PresenceRequest) {
        self.calls.lock().unwrap().push(request.clone());
        let entered = self.entered.lock().unwrap().take();
        if let Some(entered) = entered {
            entered.send(()).unwrap();
            self.release.lock().unwrap().recv().unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn public_mutations_wait_for_prior_delivery_while_snapshots_remain_readable() {
    let (authority, direct) = wire_shared_presence();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let sink = Arc::new(BlockingRecorder {
        calls: Mutex::new(Vec::new()),
        entered: Mutex::new(Some(entered_tx)),
        release: Mutex::new(release_rx),
    });
    authority.install(sink.clone()).await;
    let first = PresenceRequest::new(OnlineStatus::Idle, None);
    let second = PresenceRequest::new(OnlineStatus::Dnd, None);
    let first_writer = {
        let direct = direct.clone();
        let first = first.clone();
        tokio::spawn(async move { direct.set(first).await })
    };
    entered_rx.await.unwrap();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let mut second_writer = {
        let direct = direct.clone();
        let second = second.clone();
        tokio::spawn(async move {
            started_tx.send(()).unwrap();
            direct.set(second).await
        })
    };
    started_rx.await.unwrap();
    let pending =
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut second_writer).await;
    let snapshot = tokio::time::timeout(std::time::Duration::from_secs(2), direct.snapshot()).await;
    release_tx.send(()).unwrap();
    first_writer.await.unwrap().unwrap();
    if pending.is_err() {
        second_writer.await.unwrap().unwrap();
    }
    assert!(
        pending.is_err(),
        "second mutation must wait for the prior sink effect"
    );
    assert_eq!(
        snapshot.unwrap().unwrap().desired().unwrap().request(),
        &first
    );
    assert_eq!(*sink.calls.lock().unwrap(), [first, second.clone()]);
    assert_eq!(
        direct
            .snapshot()
            .await
            .unwrap()
            .desired()
            .unwrap()
            .request(),
        &second
    );
}
