//! Private real MCP loopback for the moved combined Bell/Attention case.
use axum::response::IntoResponse;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

pub(crate) struct McpFixture {
    pub(crate) calls: Arc<AtomicUsize>,
    pub(crate) endpoint: String,
    shutdown: tokio_util::sync::CancellationToken,
    server: tokio::task::JoinHandle<()>,
}
impl McpFixture {
    pub(crate) async fn value(value: Value) -> Self {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let shutdown = tokio_util::sync::CancellationToken::new();
        let router = axum::Router::new().route("/mcp", axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
            let calls = observed.clone();
            let value = value.clone();
            async move {
                let Some(id) = body.get("id") else {
                    return axum::http::StatusCode::ACCEPTED.into_response();
                };
                let result = match body["method"].as_str().unwrap() {
                    "initialize" => json!({"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"bell-fixture","version":"1"}}),
                    "tools/call" => {
                        assert_eq!(body["params"]["name"], "recall");
                        // Retain the original fixture's argument type checks.
                        assert!(body["params"]["arguments"]["query"].as_str().is_some());
                        assert!(body["params"]["arguments"]["scope"].as_str().is_some());
                        assert!(body["params"]["arguments"]["limit"].as_u64().is_some());
                        calls.fetch_add(1, Ordering::SeqCst);
                        json!({"content":[{"type":"text","text":value.to_string()}],"isError":false})
                    }
                    other => panic!("unexpected fixture method {other}"),
                };
                axum::Json(json!({"jsonrpc":"2.0","id":id,"result":result})).into_response()
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
        let stopped = shutdown.clone();
        let server = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(stopped.cancelled_owned())
                .await
                .unwrap();
        });
        Self {
            calls,
            endpoint,
            shutdown,
            server,
        }
    }
}
impl Drop for McpFixture {
    fn drop(&mut self) {
        self.shutdown.cancel();
        self.server.abort();
    }
}
