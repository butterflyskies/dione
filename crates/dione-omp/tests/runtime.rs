use camino::Utf8Path;
use dione_harness::{AttentionPolicy, ProtocolOutput};
use dione_omp::{OmpSession, connect};
use serde_json::json;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn public_control_preserves_strict_arguments_and_fenced_wire_results() {
    let directory = tempfile::tempdir().unwrap();
    let path = Utf8Path::from_path(directory.path()).unwrap();
    let session = OmpSession::open(path).await.unwrap();
    let output = ProtocolOutput::new(tokio::io::sink());
    let connection = connect(output.clone(), Some(session.control()), &["app-only"]);
    assert_eq!(connection.attention_policy(), AttentionPolicy::Direct);
    let running = session.start(output, CancellationToken::new());
    let protocol = connection.protocol();
    protocol.stdio_initialized().await.unwrap();
    for args in [
        json!({"session_id":"bad space"}),
        json!({"session_id":"valid", "extra":true}),
    ] {
        let error = protocol
            .private_control("teams_bind_session", &args)
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(error.to_string(), "invalid private control arguments");
    }
    let bound = protocol
        .private_control("teams_bind_session", &json!({"session_id":"saved-one"}))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(bound["bound"], true);
    assert_eq!(bound["session_id"], "saved-one");
    let binding = bound["binding_id"].as_str().unwrap();
    let active = protocol
        .private_control(
            "teams_check_session",
            &json!({"session_id":"saved-one", "binding_id":binding}),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(active, json!({"state":"active"}));
    let error = protocol
        .call("app-only", &json!({}))
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "app-only is only available in Codex mode"
    );
    assert!(protocol.call("other", &json!({})).await.is_none());
    protocol.stdio_disconnected().await.unwrap();
    let unavailable = protocol
        .private_control(
            "teams_check_session",
            &json!({"session_id":"saved-one", "binding_id":binding}),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(unavailable, json!({"state":"unavailable"}));
    running.shutdown().await.unwrap();
    drop(connection);
    drop(OmpSession::open(path).await.unwrap());
}
