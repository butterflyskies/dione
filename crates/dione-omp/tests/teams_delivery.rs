use camino::Utf8Path;
use dione_harness::ProtocolOutput;
use dione_messages::timestamp::Timestamp;
use dione_omp::{OmpSession, OmpSessionError, OmpSessionErrorKind, connect, teams_delivery};
use dione_provider_delivery::{
    ProviderEventKey, ReplyHandle, TeamsAdmissionOutcome, TeamsDeliveryErrorKind,
    TeamsResidentEvent,
};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_util::sync::CancellationToken;

fn event(id: &str) -> TeamsResidentEvent {
    TeamsResidentEvent {
        provider_key: ProviderEventKey::teams(
            "tenant".parse().unwrap(),
            "bot".parse().unwrap(),
            "conversation".parse().unwrap(),
            id.parse().unwrap(),
        ),
        content: "actual admission payload".into(),
        message_id: id.parse().unwrap(),
        conversation_id: "conversation".parse().unwrap(),
        channel_id: "msteams".into(),
        user_id: "actor".parse().unwrap(),
        bot_id: "bot".parse().unwrap(),
        tenant_id: "tenant".parse().unwrap(),
        reply_handle: ReplyHandle::parse("teams-00000000-0000-4000-8000-000000000001").unwrap(),
        timestamp: Timestamp::now(),
    }
}
#[tokio::test]
async fn public_factory_uses_real_binding_and_preserves_commit_duplicate_and_identity() {
    let directory = tempfile::tempdir().unwrap();
    let path = Utf8Path::from_path(directory.path()).unwrap();
    let session = OmpSession::open(path).await.unwrap();
    let delivery = teams_delivery(session.admission());
    let (writer, reader) = tokio::io::duplex(16384);
    let output = ProtocolOutput::new(writer);
    let connection = connect(output.clone(), Some(session.control()), &[]);
    let running = session.start(output, CancellationToken::new());
    let error = delivery.deliver(event("first")).await.err().unwrap();
    assert!(
        matches!(error.kind,TeamsDeliveryErrorKind::RouteUnavailable(source) if matches!(source.downcast_ref::<OmpSessionError>().unwrap().kind,OmpSessionErrorKind::Unavailable))
    );
    let protocol = connection.protocol();
    protocol.stdio_initialized().await.unwrap();
    let bound = protocol
        .private_control(
            "teams_bind_session",
            &json!({"session_id":"actual-session"}),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(bound["bound"], true);
    let mut invalid = event("invalid");
    invalid.message_id = "different".parse().unwrap();
    let error = delivery.deliver(invalid).await.err().unwrap();
    assert!(
        matches!(error.kind,TeamsDeliveryErrorKind::RouteUnavailable(source) if matches!(source.downcast_ref::<OmpSessionError>().unwrap().kind,OmpSessionErrorKind::EventIdentity))
    );
    let mut invalid_shape = event("bad-shape");
    invalid_shape.channel_id.clear();
    let error = delivery.deliver(invalid_shape).await.err().unwrap();
    assert!(
        matches!(error.kind,TeamsDeliveryErrorKind::RouteUnavailable(source) if matches!(source.downcast_ref::<OmpSessionError>().unwrap().kind,OmpSessionErrorKind::EventShape))
    );
    assert!(matches!(
        delivery.deliver(event("first")).await.unwrap(),
        TeamsAdmissionOutcome::Committed
    ));
    assert!(matches!(
        delivery.deliver(event("first")).await.unwrap(),
        TeamsAdmissionOutcome::Duplicate
    ));
    let mut lines = BufReader::new(reader).lines();
    let line = tokio::time::timeout(std::time::Duration::from_secs(5), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let value: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(value["method"], "notifications/dione/event");
    assert_eq!(value["params"]["content"], "actual admission payload");
    let meta = &value["params"]["meta"];
    for (key, expected) in [
        ("provider", "teams"),
        ("message_id", "first"),
        ("conversation_id", "conversation"),
        ("channel_id", "msteams"),
        ("user_id", "actor"),
        ("bot_id", "bot"),
        ("tenant_id", "tenant"),
        ("mcp_session_id", "actual-session"),
    ] {
        assert_eq!(meta[key], expected);
    }
    assert_eq!(meta["binding_id"], bound["binding_id"]);
    // A subsequent distinct event must be the next frame: duplicate admission cannot insert a second copy.
    assert!(matches!(
        delivery.deliver(event("second")).await.unwrap(),
        TeamsAdmissionOutcome::Committed
    ));
    let line = tokio::time::timeout(std::time::Duration::from_secs(5), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let second: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(second["params"]["meta"]["message_id"], "second");
    running.shutdown().await.unwrap();
    drop(connection);
    drop(OmpSession::open(path).await.unwrap());
}
