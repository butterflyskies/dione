//! Query owner argument compatibility and registered execution.
use super::*;
use crate::{harness::HarnessConnection, mcp::tool_registry::ToolRegistry};
use camino::Utf8PathBuf;
use proptest::prelude::*;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

proptest! {
        /// The parsed limit always lands in Discord's accepted 1..=100 window
        /// (a limit of 0 would make `has_more: count == limit` hold vacuously
        /// on an empty page), and an absent limit yields the default.
        #[test]
        fn parse_limit_always_in_range(
            // Edge-weighted for the same reason as id_strategy: uniform u64
            // almost never lands on 0 (clamp-to-1 branch) or in 1..=100
            // (exact passthrough branch), leaving both untested.
            limit in proptest::option::of(prop_oneof![
                1 => Just(0u64),
                5 => 1u64..=100,
                4 => 101u64..,
            ]),
            default in 1u8..=100,
        ) {
            let args = match limit {
                Some(l) => json!({ "limit": l }),
                None => json!({}),
            };
            let parsed = parse_limit(&args, default);
            prop_assert!((1..=100).contains(&parsed));
            match limit {
                None => prop_assert_eq!(parsed, default),
                Some(l) => prop_assert_eq!(u64::from(parsed), l.clamp(1, 100)),
            }
        }
}
#[test]
fn strict_optional_id_absent_yields_none() {
    assert_eq!(
        parse_strict_optional_id(&json!({}), "id")
            .map(|o| o.map(|s| s.get()))
            .map_err(|error| error.to_string()),
        Ok(None)
    );
}

#[test]
fn strict_optional_id_valid_snowflake() {
    let result = parse_strict_optional_id(&json!({"id": "1234567890"}), "id");
    assert_eq!(
        result
            .map(|o| o.map(|s| s.get()))
            .map_err(|error| error.to_string()),
        Ok(Some(1234567890))
    );

    let result = parse_strict_optional_id(&json!({"id": 1234567890u64}), "id");
    assert_eq!(
        result
            .map(|o| o.map(|s| s.get()))
            .map_err(|error| error.to_string()),
        Ok(Some(1234567890))
    );
}

#[test]
fn strict_optional_id_rejects_null() {
    assert!(parse_strict_optional_id(&json!({"id": null}), "id").is_err());
}

#[test]
fn strict_optional_id_rejects_bool() {
    assert!(parse_strict_optional_id(&json!({"id": true}), "id").is_err());
}

#[test]
fn strict_optional_id_rejects_object() {
    assert!(parse_strict_optional_id(&json!({"id": {}}), "id").is_err());
}

#[test]
fn strict_optional_id_rejects_array() {
    assert!(parse_strict_optional_id(&json!({"id": []}), "id").is_err());
}

#[test]
fn strict_optional_id_rejects_empty_string() {
    assert!(parse_strict_optional_id(&json!({"id": ""}), "id").is_err());
}

#[test]
fn strict_optional_id_rejects_zero() {
    assert!(parse_strict_optional_id(&json!({"id": 0}), "id").is_err());
    assert!(parse_strict_optional_id(&json!({"id": "0"}), "id").is_err());
}

#[test]
fn strict_optional_id_rejects_non_numeric_string() {
    assert!(parse_strict_optional_id(&json!({"id": "abc"}), "id").is_err());
}
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
    let (_, error) = crate::config::reload_config(&state_dir).await;
    assert!(error.is_none(), "{error:?}");
    let services = Arc::new(ReplyServices::new(
        Arc::new(
            serenity::http::HttpBuilder::new("fake")
                .proxy(&base)
                .ratelimiter_disabled(true)
                .build(),
        ),
        crate::state::new_state(),
        state_dir.clone(),
        Arc::new(crate::no_rly::consent::ConsentGate::new(&state_dir)),
        Arc::new(crate::ingress_ledger::IngressLedger::new()),
        None,
    ));
    let registry = ToolRegistry::new(
        &HarnessConnection::claude(),
        registrations_with_search(
            services,
            crate::core::ModuleInstanceId::new(
                crate::core::ProviderId::new("discord").unwrap(),
                crate::core::ModuleSlotId::new("primary").unwrap(),
            ),
            search::SearchTransport::local(format!("{base}/api/v10")),
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
    let (_, error) = crate::config::reload_config(&state_dir).await;
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

#[test]
fn wrong_type_optional_limit_keeps_legacy_default() {
    for limit in [
        Value::Null,
        json!(true),
        json!("9"),
        json!(-1),
        json!(1.5),
        json!([]),
        json!({}),
    ] {
        assert_eq!(parse_limit(&json!({"limit":limit}), 20), 20);
    }
}

#[test]
fn malformed_search_retains_field_path_and_concrete_cause() {
    let error = search_wire::parse_request(
        serenity::model::id::GuildId::new(7),
        &json!({"limit":"bad"}),
    )
    .unwrap_err();
    match error.kind {
        SearchInputErrorKind::Deserialize(source) => {
            assert_eq!(source.path().to_string(), "limit");
            assert!(source.inner().is_data());
        }
        SearchInputErrorKind::Preparation(error) => {
            panic!("unexpected preparation failure: {error}")
        }
    }
    let cursor = parse_strict_optional_id(&json!({"before":"bad"}), "before").unwrap_err();
    assert_eq!(cursor.to_string(), "invalid before: not a valid u64");
    assert!(matches!(cursor.kind, CursorErrorKind::Decimal(_)));
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
    let (_, error) = crate::config::reload_config(&state_dir).await;
    assert!(error.is_none(), "{error:?}");
    let services = Arc::new(ReplyServices::new(
        Arc::new(
            serenity::http::HttpBuilder::new("fake")
                .proxy(&base)
                .ratelimiter_disabled(true)
                .build(),
        ),
        crate::state::new_state(),
        state_dir.clone(),
        Arc::new(crate::no_rly::consent::ConsentGate::new(&state_dir)),
        Arc::new(crate::ingress_ledger::IngressLedger::new()),
        None,
    ));
    let registry = ToolRegistry::new(
        &HarnessConnection::claude(),
        registrations_with_search(
            services,
            crate::core::ModuleInstanceId::new(
                crate::core::ProviderId::new("discord").unwrap(),
                crate::core::ModuleSlotId::new("primary").unwrap(),
            ),
            search::SearchTransport::local(format!("{base}/api/v10")),
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
