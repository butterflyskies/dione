use dione_coordination::{
    ClaimOutcome, ClaimProgress, CoordinationConfig, CoordinationError, CoordinationEvent,
    Coordinator,
};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
};

fn client(addr: String) -> Coordinator {
    Coordinator::new(
        CoordinationConfig {
            addr,
            lease_ms: 240_000,
            connect_timeout_ms: 2_000,
            fail_open: true,
        },
        "222",
    )
}

async fn hello(socket: TcpStream) -> BufReader<TcpStream> {
    let mut peer = BufReader::new(socket);
    let mut line = String::new();
    peer.read_line(&mut line).await.unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&line).unwrap(),
        json!({"msg":"hello","bot_id":"222"})
    );
    peer.get_mut().write_all(b"{\"ok\":true}\n").await.unwrap();
    peer
}

#[tokio::test]
async fn checked_wait_closes_connection_but_ordinary_wait_delivers_push() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let coordinator = client(listener.local_addr().unwrap().to_string());
    let server = tokio::spawn(async move {
        let mut checked = hello(listener.accept().await.unwrap().0).await;
        let mut line = String::new();
        checked.read_line(&mut line).await.unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&line).unwrap(),
            json!({"msg":"claim","channel_id":"42","message_id":"1001"})
        );
        checked
            .get_mut()
            .write_all(b"{\"status\":\"wait\",\"ahead\":[\"111\"],\"lease_ms\":240000}\n")
            .await
            .unwrap();
        line.clear();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), checked.read_line(&mut line))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        let mut ordinary = hello(listener.accept().await.unwrap().0).await;
        ordinary.read_line(&mut line).await.unwrap();
        ordinary.get_mut().write_all(b"{\"status\":\"wait\",\"ahead\":[\"111\"],\"lease_ms\":240000}\n{\"event\":\"done\",\"message_id\":\"1002\",\"bot_id\":\"111\",\"reply_message_id\":\"2001\"}\n").await.unwrap();
    });
    let (outcome, progress) = coordinator
        .claim_with_progress("42", "1001")
        .await
        .into_parts();
    assert_eq!(progress, ClaimProgress::Written);
    assert_eq!(
        outcome,
        ClaimOutcome::Wait {
            ahead: vec!["111".to_owned()],
            lease_ms: 240_000
        }
    );
    let mut events = coordinator.subscribe();
    assert!(matches!(
        coordinator.claim("42", "1002").await,
        ClaimOutcome::Wait { .. }
    ));
    let event = tokio::time::timeout(Duration::from_secs(1), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        event,
        CoordinationEvent::Done {
            channel_id: 42.into(),
            message_id: 1002.into(),
            bot_id: 111.into(),
            reply_message_id: 2001.into()
        }
    );
    server.await.unwrap();
}

#[tokio::test]
async fn checked_results_distinguish_not_sent_from_written_unavailable() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let coordinator = client(listener.local_addr().unwrap().to_string());
    let (outcome, progress) = coordinator
        .claim_with_progress("0", "1001")
        .await
        .into_parts();
    assert_eq!(progress, ClaimProgress::NotSent);
    assert_eq!(
        outcome,
        ClaimOutcome::Unavailable {
            reason: "protocol: invalid claim channel identity".to_owned(),
            claim_sent: false
        }
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(20), listener.accept())
            .await
            .is_err()
    );
    let server = tokio::spawn(async move {
        let mut peer = hello(listener.accept().await.unwrap().0).await;
        let mut line = String::new();
        peer.read_line(&mut line).await.unwrap();
        peer.get_mut()
            .write_all(b"{\"status\":\"unsupported\"}\n")
            .await
            .unwrap();
    });
    let (outcome, progress) = coordinator
        .claim_with_progress("42", "1001")
        .await
        .into_parts();
    assert_eq!(progress, ClaimProgress::Written);
    assert_eq!(
        outcome,
        ClaimOutcome::Unavailable {
            reason: "protocol: unexpected claim status".to_owned(),
            claim_sent: true
        }
    );
    server.await.unwrap();
}

#[tokio::test]
async fn checked_release_requires_positive_acknowledgment_with_typed_error() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let coordinator = client(listener.local_addr().unwrap().to_string());
    let server = tokio::spawn(async move {
        for ack in [false, true] {
            let mut peer = hello(listener.accept().await.unwrap().0).await;
            let mut line = String::new();
            peer.read_line(&mut line).await.unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&line).unwrap(),
                json!({"msg":"release","message_id":"1001"})
            );
            peer.get_mut()
                .write_all(format!("{}\n", json!({"ok":ack})).as_bytes())
                .await
                .unwrap();
        }
    });
    let error: CoordinationError = coordinator.release_checked("1001").await.unwrap_err();
    assert!(
        matches!(&error, CoordinationError::Protocol(message) if message == "release not acknowledged")
    );
    assert_eq!(error.to_string(), "protocol: release not acknowledged");
    coordinator.release_checked("1001").await.unwrap();
    server.await.unwrap();
}
