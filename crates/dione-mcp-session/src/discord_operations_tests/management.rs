//! Actual root management registration and reloaded authority.
use dione_access_requests::AccessQueue;
use dione_config::{ChannelConfig, Config, LoadedConfig};
use dione_discord_runtime::IngressLedger;
use dione_discord_state::{State, new_state};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    time::timeout,
};
struct FixtureContext {
    http: Arc<serenity::http::Http>,
    state: State,
    _config: Arc<LoadedConfig>,
    ingress_ledger: Arc<IngressLedger>,
}
#[derive(Debug, PartialEq)]
struct Request {
    line: String,
    body: Value,
}

fn config_with_channel(channel_id: u64) -> LoadedConfig {
    let mut raw = Config::default();
    raw.channels.push(ChannelConfig {
        id: channel_id.to_string(),
        ..Default::default()
    });
    LoadedConfig::try_from_raw(raw).expect("valid test configuration")
}
fn ctx(config: LoadedConfig) -> FixtureContext {
    FixtureContext {
        http: Arc::new(serenity::http::Http::new("fake")),
        state: new_state(),
        _config: Arc::new(config),
        ingress_ledger: Arc::new(dione_discord_runtime::IngressLedger::new()),
    }
}
async fn serve_fixture(
    listener: TcpListener,
    responses: Vec<(&'static str, Value)>,
) -> Vec<Request> {
    timeout(Duration::from_secs(5), async move {
            let mut requests = Vec::new();
            for (status, body) in responses {
                let (stream, _) = listener.accept().await.unwrap();
                let mut reader = tokio::io::BufReader::new(stream);
                let mut line = String::new();
                assert_ne!(
                    reader.read_line(&mut line).await.unwrap(),
                    0,
                    "request line ended at EOF"
                );
                let mut length = 0;
                loop {
                    let mut header = String::new();
                    assert_ne!(
                        reader.read_line(&mut header).await.unwrap(),
                        0,
                        "request headers ended at EOF"
                    );
                    if header == "\r\n" {
                        break;
                    }
                    if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:")
                    {
                        length = value.trim().parse::<usize>().unwrap();
                    }
                }
                let mut bytes = vec![0; length];
                reader.read_exact(&mut bytes).await.unwrap();
                requests.push(Request {
                    line: line.trim_end().to_owned(),
                    body: if bytes.is_empty() {
                        Value::Null
                    } else {
                        serde_json::from_slice(&bytes).unwrap()
                    },
                });
                let body = if status == "204 No Content" {
                    String::new()
                } else {
                    body.to_string()
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                reader
                    .get_mut()
                    .write_all(response.as_bytes())
                    .await
                    .unwrap();
            }
            requests
        })
        .await
        .expect("fake Discord exchange deadline")
}
async fn fixture(
    responses: Vec<(&'static str, Value)>,
) -> (FixtureContext, tokio::task::JoinHandle<Vec<Request>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(serve_fixture(listener, responses));
    let mut ctx = ctx(config_with_channel(42));
    ctx.http = Arc::new(
        serenity::http::HttpBuilder::new("fake")
            .proxy(format!("http://{address}"))
            .ratelimiter_disabled(true)
            .build(),
    );
    (ctx, server)
}
fn thread_response() -> Value {
    json!({"id":"200", "guild_id":"7", "type":11, "name":"Discord returned name", "parent_id":"999", "position":0, "permission_overwrites":[], "nsfw":false})
}
#[tokio::test]
async fn registered_management_preserves_http_and_reloads_authority() {
    let (ctx, peer) = fixture(vec![
        ("204 No Content", Value::Null),
        ("204 No Content", Value::Null),
        ("204 No Content", Value::Null),
        ("200 OK", thread_response()),
        ("200 OK", thread_response()),
    ])
    .await;
    let dir = tempfile::TempDir::new().unwrap();
    let path = camino::Utf8PathBuf::from_path_buf(dir.path().to_owned()).unwrap();
    ctx.state.write().await.note_sent(9001);
    let server = crate::mcp::server::DioneServer::new(
        crate::TEST_APPLICATION_VERSION,
        ctx.state.clone(),
        Arc::new(tokio::sync::Mutex::new(AccessQueue::load(&path))),
        ctx.http.clone(),
        path.clone(),
        tokio::sync::mpsc::channel(1).0,
        dione_tracing_channel::TraceLevelController::noop(),
        dione_claude::connect(
            dione_harness::ProtocolOutput::new(tokio::io::stdout()),
            dione_codex::TOOL_NAMES,
        ),
        Arc::new(dione_outbound_consent::consent::ConsentGate::new(&path)),
        ctx.ingress_ledger.clone(),
        dione_contracts::ModuleInstanceId::new(
            dione_contracts::ProviderId::new("discord").unwrap(),
            dione_contracts::ModuleSlotId::new("primary").unwrap(),
        ),
        None,
    )
    .await
    .unwrap();
    for name in [
        "pin_message",
        "unpin_message",
        "delete_message",
        "create_thread",
    ] {
        let invalid = crate::mcp::dispatch::call_tool(
            &server,
            name,
            json!({"channel_id":"bad", "message_id":"9001", "name":"thread"}),
        )
        .await
        .unwrap_err();
        assert_eq!(invalid, "invalid channel_id: not a valid u64");
        let denied = crate::mcp::dispatch::call_tool(
            &server,
            name,
            json!({"channel_id":"42", "message_id":"9001", "name":"thread"}),
        )
        .await
        .unwrap();
        assert_eq!(
            denied,
            json!({"content":[{"type":"text","text":"{\"error\":\"channel not in allowlist\"}"}],"isError":true})
        );
    }
    std::fs::write(path.join("config.toml"), "[[channels]]\nid = \"42\"\n").unwrap();
    assert!(dione_config::reload_config(&path).await.1.is_none());
    for name in ["pin_message", "unpin_message", "delete_message"] {
        let result = crate::mcp::dispatch::call_tool(
            &server,
            name,
            json!({"channel_id":42,"message_id":"9001","extra":true}),
        )
        .await
        .unwrap();
        assert_eq!(
            result,
            json!({"content":[{"type":"text","text":"{\"ok\":true}"}]})
        );
    }
    for message in [json!({}), json!("9001")] {
        let result = crate::mcp::dispatch::call_tool(
            &server,
            "create_thread",
            json!({"channel_id":"42","message_id":message,"name":"","extra":true}),
        )
        .await
        .unwrap();
        let wire: Value =
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(
            wire,
            json!({"ok":true,"thread_id":"200","name":"Discord returned name"})
        );
    }
    assert_eq!(
        ctx.state.read().await.thread_parents.get(&200),
        Some(&Some(42))
    );
    std::fs::write(path.join("config.toml"), "").unwrap();
    assert!(dione_config::reload_config(&path).await.1.is_none());
    let denied = crate::mcp::dispatch::call_tool(
        &server,
        "pin_message",
        json!({"channel_id":42,"message_id":9001}),
    )
    .await
    .unwrap();
    assert_eq!(
        denied,
        json!({"content":[{"type":"text","text":"{\"error\":\"channel not in allowlist\"}"}],"isError":true})
    );
    assert_eq!(
        peer.await.unwrap(),
        vec![
            Request {
                line: "PUT /api/v10/channels/42/pins/9001 HTTP/1.1".into(),
                body: Value::Null
            },
            Request {
                line: "DELETE /api/v10/channels/42/pins/9001 HTTP/1.1".into(),
                body: Value::Null
            },
            Request {
                line: "DELETE /api/v10/channels/42/messages/9001 HTTP/1.1".into(),
                body: Value::Null
            },
            Request {
                line: "POST /api/v10/channels/42/threads HTTP/1.1".into(),
                body: json!({"name":"","type":11})
            },
            Request {
                line: "POST /api/v10/channels/42/messages/9001/threads HTTP/1.1".into(),
                body: json!({"name":"","type":11})
            },
        ]
    );
}
