use dione_harness::ProtocolOutput;
use dione_omp::{OmpSession, connect};
use serde_json::json;
use tokio_util::sync::CancellationToken;
fn main() {
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let path = camino::Utf8Path::from_path(directory.path()).unwrap();
        let session = OmpSession::open(path).await.unwrap();
        let output = ProtocolOutput::new(tokio::io::sink());
        let connection = connect(
            output.clone(),
            Some(session.control()),
            &["actual_app_tool"],
        );
        let running = session.start(output, CancellationToken::new());
        connection.protocol().stdio_initialized().await.unwrap();
        let bound = connection
            .protocol()
            .private_control("teams_bind_session", &json!({"session_id":"saved-one"}))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(bound["bound"], true);
        connection.protocol().stdio_disconnected().await.unwrap();
        running.shutdown().await.unwrap();
        drop(connection);
        let reopened = OmpSession::open(path).await.unwrap();
        drop(reopened);
    });
}
