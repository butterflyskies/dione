//! Root composition through the public resident runner, without owner test helpers.
use std::{os::unix::fs::PermissionsExt, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::{Mutex, mpsc},
};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn root_mcp_session_resident_reports_application_version_and_drains() {
    let directory = tempfile::TempDir::new().unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let state_dir = camino::Utf8PathBuf::from_path_buf(directory.path().to_owned()).unwrap();
    let authority = dione_contracts::ModuleInstanceId::new(
        dione_contracts::ProviderId::new("discord").unwrap(),
        dione_contracts::ModuleSlotId::new("primary").unwrap(),
    );
    let queue = dione_codex::CodexEventQueue::load(
        &state_dir,
        dione_codex::LegacyDiscordScope::new(authority.clone()),
    )
    .unwrap();
    let (binding, _binding_rx) = tokio::sync::watch::channel(None);
    let harness = dione_codex::connect(queue, binding);
    let server = dione_mcp_session::DioneServer::new(
        env!("CARGO_PKG_VERSION"),
        dione_discord_state::new_state(),
        Arc::new(Mutex::new(dione_access_requests::AccessQueue::load(
            &state_dir,
        ))),
        Arc::new(serenity::http::Http::new("fixture")),
        state_dir.clone(),
        mpsc::channel(1).0,
        dione_tracing_channel::TraceLevelController::noop(),
        harness,
        Arc::new(dione_outbound_consent::consent::ConsentGate::new(
            &state_dir,
        )),
        Arc::new(dione_discord_runtime::IngressLedger::new()),
        authority,
        None,
    )
    .await
    .unwrap();
    let socket = state_dir.join("control.sock");
    let cancel = CancellationToken::new();
    let listener = tokio::spawn(dione_mcp_session::run_resident_control(
        Arc::new(server),
        socket.clone(),
        cancel.clone(),
    ));
    let stream = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(stream) = tokio::net::UnixStream::connect(socket.as_std_path()).await {
                break stream;
            }
            assert!(
                !listener.is_finished(),
                "resident runner exited before bind"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("public resident runner binds");
    let (reader, mut writer) = stream.into_split();
    let request = serde_json::json!({"jsonrpc":"2.0","id":73,"method":"initialize","params":{"protocolVersion":"2025-06-18"}});
    writer
        .write_all(format!("{request}\n").as_bytes())
        .await
        .unwrap();
    let mut lines = BufReader::new(reader).lines();
    let line = tokio::time::timeout(Duration::from_secs(2), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let response: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(response["id"], 73);
    assert_eq!(response["result"]["serverInfo"]["name"], "dione");
    assert_eq!(
        response["result"]["serverInfo"]["version"],
        env!("CARGO_PKG_VERSION")
    );
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(2), listener)
        .await
        .expect("accepted requests drain")
        .expect("runner joins")
        .expect("runner exits");
    assert!(!socket.exists(), "public runner removes its drained socket");
}
