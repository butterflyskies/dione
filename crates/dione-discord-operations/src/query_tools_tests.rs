//! Query owner argument compatibility and registered execution.
use super::*;
use camino::Utf8PathBuf;
use dione_mcp_tools::ToolRegistry;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

fn observed_message(attachments: Value) -> Value {
    json!({"id":"9001","channel_id":"42","author":{"id":"80","username":"fixture","discriminator":"0","avatar":null,"bot":false},"content":"observed","timestamp":"2026-08-15T09:00:00+00:00","edited_timestamp":null,"tts":false,"mention_everyone":false,"mentions":[],"mention_roles":[],"attachments":attachments,"embeds":[],"pinned":false,"type":0})
}

#[tokio::test]
async fn registered_queries_parse_execute_project_and_reload_policy() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let attachment_url = format!("{base}/bytes");
    let requests = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let captured = requests.clone();
    let server = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = tokio::io::BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).await.unwrap() == 0 || header == "\r\n" {
                    break;
                }
            }
            captured.lock().unwrap().push(line.trim().to_owned());
            let path = line.split_whitespace().nth(1).unwrap();
            let message = observed_message(json!([]));
            let body = if path == "/bytes" {
                "attachment bytes".to_owned()
            } else if path.starts_with("/api/v10/guilds/7/messages/search?") {
                json!({"messages":[[message]]}).to_string()
            } else if path == "/api/v10/channels/42/messages/8" {
                {
                    let mut message = observed_message(
                        json!([{"id":"1","filename":"captured.txt","size":16,"url":attachment_url,"proxy_url":attachment_url,"content_type":"text/plain"}]),
                    );
                    message["id"] = json!("8");
                    message.to_string()
                }
            } else if path == "/api/v10/channels/42/messages/9001" {
                message.to_string()
            } else if path == "/api/v10/channels/42/pins"
                || path.starts_with("/api/v10/channels/42/messages?")
            {
                json!([message]).to_string()
            } else {
                panic!("unexpected fixture request: {line}")
            };
            reader.get_mut().write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let state_dir = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).unwrap();
    tokio::fs::write(
        state_dir.join("config.toml"),
        "[delivery]\nevidence_markers_enabled = false\n\n[[channels]]\nid = \"42\"\n",
    )
    .await
    .unwrap();
    let (_, error) = dione_config::reload_config(&state_dir).await;
    assert!(error.is_none(), "{error:?}");
    let services = Arc::new(ReplyServices::new(
        Arc::new(
            serenity::http::HttpBuilder::new("fake")
                .proxy(&base)
                .ratelimiter_disabled(true)
                .build(),
        ),
        dione_discord_state::new_state(),
        state_dir.clone(),
        Arc::new(dione_outbound_consent::consent::ConsentGate::new(
            &state_dir,
        )),
        Arc::new(dione_discord_runtime::IngressLedger::new()),
        None,
    ));
    let registry = ToolRegistry::new(
        &crate::test_support::reserved_tool_names(&dione_claude::connect(
            dione_harness::ProtocolOutput::new(tokio::io::stdout()),
            dione_codex::TOOL_NAMES,
        )),
        registrations_with_search(
            services,
            dione_contracts::ModuleInstanceId::new(
                dione_contracts::ProviderId::new("discord").unwrap(),
                dione_contracts::ModuleSlotId::new("primary").unwrap(),
            ),
            search::SearchTransport::with_endpoint(format!("{base}/api/v10").parse().unwrap())
                .unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    for name in [
        "fetch_messages",
        "fetch_new_since",
        "fetch_pins",
        "download_attachment",
        "get_message",
        "search_messages",
    ] {
        let error = registry
            .find(name)
            .unwrap()
            .call(&json!({"channel_id":"bad","guild_id":"bad"}))
            .await
            .unwrap_err();
        assert!(matches!(error.kind, ToolCallErrorKind::OwnerInput(_)));
    }
    assert!(
        registry
            .find("fetch_messages")
            .unwrap()
            .call(&json!({"channel_id":"42","before":null}))
            .await
            .is_err()
    );
    assert!(
        registry
            .find("search_messages")
            .unwrap()
            .call(&json!({"guild_id":"7","content":"needle","limit":"bad"}))
            .await
            .is_err()
    );
    assert_eq!(
        registry
            .find("search_messages")
            .unwrap()
            .call(&json!({"guild_id":"7"}))
            .await
            .unwrap(),
        json!({"error":"at least one search filter is required (content, author_id, mentions, has, before, after, or channel_id)"})
    );
    assert!(requests.lock().unwrap().is_empty());
    let expected = json!({"id":"9001","author":"fixture","author_id":"80","content":"observed","timestamp":"2026-08-15T09:00:00+00:00","attachments":[]});
    for (name, args, result) in [
        (
            "fetch_messages",
            json!({"channel_id":"42","limit":"ignored"}),
            json!({"messages":[expected.clone()]}),
        ),
        (
            "fetch_new_since",
            json!({"channel_id":"42","after_message_id":"7","limit":0}),
            json!({"messages":[expected.clone()],"count":1,"has_more":true}),
        ),
        (
            "fetch_pins",
            json!({"channel_id":"42"}),
            json!({"messages":[expected.clone()],"count":1}),
        ),
        (
            "get_message",
            json!({"channel_id":"42","message_id":"9001"}),
            expected.clone(),
        ),
    ] {
        assert_eq!(
            registry.find(name).unwrap().call(&args).await.unwrap(),
            result,
            "{name}"
        );
    }
    let download = registry
        .find("download_attachment")
        .unwrap()
        .call(&json!({"channel_id":"42","message_id":"8"}))
        .await
        .unwrap();
    assert_eq!(
        download,
        json!({"saved":[state_dir.join("inbox/captured.txt")]})
    );
    assert_eq!(
        tokio::fs::read(state_dir.join("inbox/captured.txt"))
            .await
            .unwrap(),
        b"attachment bytes"
    );
    let mut searched_message = expected;
    searched_message["channel_id"] = json!("42");
    assert_eq!(
        registry
            .find("search_messages")
            .unwrap()
            .call(&json!({"guild_id":"7","content":"needle"}))
            .await
            .unwrap(),
        json!({"messages":[searched_message],"count":1,"offset":0,"has_more":false})
    );
    let observed = requests.lock().unwrap().clone();
    assert_eq!(observed.len(), 7);
    assert!(observed[0].contains("limit=20"));
    assert!(observed[1].contains("limit=1"));
    assert!(observed[1].contains("after=7"));
    assert!(observed[6].contains("content=needle"));
    tokio::fs::write(
        state_dir.join("config.toml"),
        "channels = []\n[delivery]\nevidence_markers_enabled = false\n",
    )
    .await
    .unwrap();
    let (_, error) = dione_config::reload_config(&state_dir).await;
    assert!(error.is_none(), "{error:?}");
    let denied = registry
        .find("get_message")
        .unwrap()
        .call(&json!({"channel_id":"42","message_id":"9001"}))
        .await
        .unwrap();
    assert!(denied["error"].as_str().unwrap().contains("42"));
    assert_eq!(requests.lock().unwrap().len(), 7);
    assert_eq!(
        registry
            .find("search_messages")
            .unwrap()
            .call(&json!({"guild_id":"7","content":"needle"}))
            .await
            .unwrap(),
        json!({"messages":[],"count":0,"offset":0,"has_more":false})
    );
    assert_eq!(requests.lock().unwrap().len(), 8);
    server.abort();
}

#[tokio::test]
async fn registered_queries_reject_wrong_observations_before_receipts_or_downloads() {
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        wrong_observation_exchange(),
    )
    .await
    .expect("registered query observation fixture timed out");
}

async fn wrong_observation_exchange() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let response = Arc::new(std::sync::Mutex::new((
        "200 OK",
        observed_message(json!([])),
    )));
    let served = response.clone();
    let requests = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let captured = requests.clone();
    let peer = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = tokio::io::BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).await.unwrap() == 0 || header == "\r\n" {
                    break;
                }
            }
            captured.lock().unwrap().push(line.trim().to_owned());
            let (status, value) = served.lock().unwrap().clone();
            let body = value.to_string();
            reader.get_mut().write_all(format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let state_dir = Utf8PathBuf::from_path_buf(directory.path().to_owned()).unwrap();
    tokio::fs::write(state_dir.join("config.toml"), format!("[[channels]]\nid = \"42\"\n[vaelii]\nserver_url = \"{base}\"\nactor_term = \"Fixture\"\n")).await.unwrap();
    let (_, error) = dione_config::reload_config(&state_dir).await;
    assert!(error.is_none(), "{error:?}");
    let services = Arc::new(ReplyServices::new(
        Arc::new(
            serenity::http::HttpBuilder::new("fake")
                .proxy(&base)
                .ratelimiter_disabled(true)
                .build(),
        ),
        dione_discord_state::new_state(),
        state_dir.clone(),
        Arc::new(dione_outbound_consent::consent::ConsentGate::new(
            &state_dir,
        )),
        Arc::new(dione_discord_runtime::IngressLedger::new()),
        None,
    ));
    let registry = ToolRegistry::new(
        &crate::test_support::reserved_tool_names(&dione_claude::connect(
            dione_harness::ProtocolOutput::new(tokio::io::stdout()),
            dione_codex::TOOL_NAMES,
        )),
        registrations_with_search(
            services,
            dione_contracts::ModuleInstanceId::new(
                dione_contracts::ProviderId::new("discord").unwrap(),
                dione_contracts::ModuleSlotId::new("primary").unwrap(),
            ),
            search::SearchTransport::with_endpoint(format!("{base}/api/v10").parse().unwrap())
                .unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    for (channel, message) in [("42", "9002"), ("99", "9001")] {
        let url = format!("{base}/bytes");
        let mut observed = observed_message(
            json!([{"id":"1","filename":"private.txt","size":1,"url":url,"proxy_url":url}]),
        );
        observed["channel_id"] = json!(channel);
        observed["id"] = json!(message);
        *response.lock().unwrap() = ("200 OK", observed);
        for name in ["get_message", "download_attachment"] {
            requests.lock().unwrap().clear();
            let result = registry
                .find(name)
                .unwrap()
                .call(&json!({"channel_id":"42","message_id":"9001"}))
                .await
                .unwrap();
            assert_eq!(
                result,
                json!({"error":format!("invalid Discord observation for channel {channel}, message {message}: expected channel 42, message 9001")})
            );
            assert_eq!(
                *requests.lock().unwrap(),
                ["GET /api/v10/channels/42/messages/9001 HTTP/1.1"]
            );
            assert!(!state_dir.join("inbox").exists());
        }
    }
    let mut wrong = observed_message(json!([]));
    wrong["channel_id"] = json!("99");
    *response.lock().unwrap() = ("200 OK", json!([observed_message(json!([])), wrong]));
    for (name, args) in [
        ("fetch_messages", json!({"channel_id":"42","before":"9999"})),
        (
            "fetch_new_since",
            json!({"channel_id":"42","after_message_id":"7"}),
        ),
        ("fetch_pins", json!({"channel_id":"42"})),
    ] {
        let result = registry.find(name).unwrap().call(&args).await.unwrap();
        assert_eq!(
            result,
            json!({"error":"invalid Discord observation for channel 99, message 9001: expected page channel 42"})
        );
    }
    *response.lock().unwrap() = (
        "403 Forbidden",
        json!({"code":50001,"message":"Missing Access"}),
    );
    let denied = registry
        .find("get_message")
        .unwrap()
        .call(&json!({"channel_id":"42","message_id":"9001"}))
        .await
        .unwrap();
    assert!(
        denied["error"].as_str().unwrap().contains("Missing Access"),
        "{denied}"
    );
    assert!(denied.get("content").is_none());
    peer.abort();
}

#[tokio::test]
async fn registered_attachment_diagnostics_keep_capability_urls_private() {
    tokio::time::timeout(
        std::time::Duration::from_secs(120),
        attachment_diagnostics_exchange(),
    )
    .await
    .expect("registered attachment diagnostic fixture completed");
}

async fn attachment_diagnostics_exchange() {
    use tracing::instrument::WithSubscriber;
    use tracing_subscriber::{Layer, layer::SubscriberExt};
    #[derive(Clone)]
    struct Capture(Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    const PATH_CANARY: &str = "SYNTHETIC_ATTACHMENT_PATH_CANARY";
    const QUERY_CANARY: &str = "SYNTHETIC_ATTACHMENT_QUERY_CANARY";
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let closed_address = closed.local_addr().unwrap();
    drop(closed);
    let signed = |origin: &str, kind: &str| {
        format!("{origin}/{PATH_CANARY}/{kind}?signature={QUERY_CANARY}")
    };
    let attachments = json!([
        {"id":"1","filename":"status.txt","size":1,"url":signed(&base,"status"),"proxy_url":signed(&base,"status")},
        {"id":"2","filename":"oversize.txt","size":1,"url":signed(&base,"oversize"),"proxy_url":signed(&base,"oversize")},
        {"id":"3","filename":"connect.txt","size":1,"url":signed(&format!("http://{closed_address}"),"connect"),"proxy_url":signed(&format!("http://{closed_address}"),"connect")},
        {"id":"4","filename":"positive.txt","size":8,"url":format!("{base}/positive"),"proxy_url":format!("{base}/positive")}
    ]);
    let requests = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let captured_requests = requests.clone();
    let peer = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = tokio::io::BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).await.unwrap() == 0 || header == "\r\n" {
                    break;
                }
            }
            let path = line.split_whitespace().nth(1).unwrap().to_owned();
            captured_requests.lock().unwrap().push(path.clone());
            let response = if path.starts_with("/api/v10/channels/42/messages/") {
                let body = observed_message(attachments.clone()).to_string();
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
            } else if path == "/positive" {
                "HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\npositive"
                    .to_owned()
            } else if path.contains("/oversize?") {
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    25 * 1024 * 1024 + 1
                )
            } else if path.contains("/status?") {
                "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned()
            } else {
                panic!("unexpected synthetic fixture route");
            };
            reader
                .get_mut()
                .write_all(response.as_bytes())
                .await
                .unwrap();
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let state = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).unwrap();
    tokio::fs::write(state.join("config.toml"), "[[channels]]\nid = \"42\"\n")
        .await
        .unwrap();
    assert!(dione_config::reload_config(&state).await.1.is_none());
    let services = Arc::new(ReplyServices::new(
        Arc::new(
            serenity::http::HttpBuilder::new("fake")
                .proxy(&base)
                .ratelimiter_disabled(true)
                .build(),
        ),
        dione_discord_state::new_state(),
        state.clone(),
        Arc::new(dione_outbound_consent::consent::ConsentGate::new(&state)),
        Arc::new(dione_discord_runtime::IngressLedger::new()),
        None,
    ));
    let registry = ToolRegistry::new(
        &crate::test_support::reserved_tool_names(&dione_claude::connect(
            dione_harness::ProtocolOutput::new(tokio::io::stdout()),
            dione_codex::TOOL_NAMES,
        )),
        registrations_with_search(
            services,
            dione_contracts::ModuleInstanceId::new(
                dione_contracts::ProviderId::new("discord").unwrap(),
                dione_contracts::ModuleSlotId::new("primary").unwrap(),
            ),
            search::SearchTransport::with_endpoint(format!("{base}/api/v10").parse().unwrap())
                .unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    let ordinary = Capture(Default::default());
    let writer = ordinary.clone();
    let trace_path = state.join("synthetic-trace.log");
    let subscriber = tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .without_time()
                .with_ansi(false)
                .with_writer(move || writer.clone())
                .with_filter(tracing_subscriber::filter::LevelFilter::WARN),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .without_time()
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(
                    std::fs::File::create(&trace_path).unwrap(),
                ))
                .with_filter(tracing_subscriber::filter::LevelFilter::TRACE),
        );
    let tool = registry.find("download_attachment").unwrap();
    let result = tool
        .call(&json!({"channel_id":"42","message_id":"9001"}))
        .with_subscriber(subscriber)
        .await
        .unwrap();
    assert_eq!(result, json!({"saved":[state.join("inbox/positive.txt")]}));
    assert_eq!(
        tokio::fs::read(state.join("inbox/positive.txt"))
            .await
            .unwrap(),
        b"positive"
    );
    let logs = [
        String::from_utf8(ordinary.0.lock().unwrap().clone()).unwrap(),
        std::fs::read_to_string(trace_path).unwrap(),
    ];
    for log in logs {
        assert!(
            log.contains("failed to download attachment"),
            "ordinary and trace diagnostics must remain present"
        );
        assert!(
            !log.contains(PATH_CANARY),
            "attachment path capability must not enter diagnostics"
        );
        assert!(
            !log.contains(QUERY_CANARY),
            "attachment query capability must not enter diagnostics"
        );
        for category in ["http_status", "too_large", "connect"] {
            assert!(
                log.contains(category),
                "safe diagnostic category must remain useful"
            );
        }
        assert!(log.contains("500"), "safe HTTP status retained");
    }
    let seen = requests.lock().unwrap().clone();
    assert_eq!(
        seen.len(),
        4,
        "one metadata lookup and three reachable attachment requests"
    );
    assert!(seen.contains(&format!("/{PATH_CANARY}/status?signature={QUERY_CANARY}")));
    assert!(seen.contains(&format!("/{PATH_CANARY}/oversize?signature={QUERY_CANARY}")));
    // An exact-target mismatch must reject before following any capability URL.
    assert!(
        tool.call(&json!({"channel_id":"42","message_id":"9002"}))
            .await
            .unwrap()["error"]
            .is_string()
    );
    assert_eq!(requests.lock().unwrap().len(), 5);
    tokio::fs::write(state.join("config.toml"), "[[channels]]\nid = \"43\"\n")
        .await
        .unwrap();
    assert!(dione_config::reload_config(&state).await.1.is_none());
    assert!(
        tool.call(&json!({"channel_id":"42","message_id":"9001"}))
            .await
            .unwrap()["error"]
            .is_string()
    );
    assert_eq!(
        requests.lock().unwrap().len(),
        5,
        "revoked authorization prevents metadata or download requests"
    );
    peer.abort();
}
