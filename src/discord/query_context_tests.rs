//! Registered context reads against synthetic Discord HTTP responses.
use super::*;
use crate::{
    core::{ModuleInstanceId, ModuleSlotId, ProviderId},
    harness::HarnessConnection,
    mcp::tool_registry::ToolRegistry,
};
use camino::Utf8PathBuf;
use std::{collections::VecDeque, sync::Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

fn fixtures() -> Value {
    serde_json::from_str(include_str!("../../tests/fixtures/context/v2.json")).unwrap()
}

fn message() -> Value {
    json!({
        "id":"1002", "channel_id":"100",
        "author":{"id":"200","username":"Fixture Author","discriminator":"0","avatar":null,"bot":false},
        "content":"Synthetic history fixture.",
        "timestamp":"2026-01-02T05:04:05.123456789+02:00",
        "edited_timestamp":null, "tts":false, "mention_everyone":false,
        "mentions":[], "mention_roles":[], "attachments":[], "embeds":[],
        "pinned":false, "type":0
    })
}

fn exact_message() -> Value {
    let mut value = message();
    value["content"] = json!("Synthetic exact-read fixture.");
    value["guild_id"] = json!("500");
    value["type"] = json!(19);
    value["edited_timestamp"] = json!("2026-01-01T22:05:06.987654321-05:00");
    value["message_reference"] = json!({"type":0,"channel_id":"100","message_id":"1001","guild_id":"500","fail_if_not_exists":true});
    let mut referenced = message();
    referenced["id"] = json!("1001");
    referenced["guild_id"] = json!("500");
    referenced["author"]["id"] = json!("201");
    referenced["content"] = json!("RECURSIVE_BODY_MUST_NOT_ESCAPE");
    value["referenced_message"] = referenced;
    value["attachments"] = json!([{
        "id":"300", "filename":"sample.ogg", "description":"Synthetic audio metadata",
        "height":null, "width":null, "proxy_url":"https://media.example.invalid/sample.ogg",
        "size":4096, "url":"https://files.example.invalid/sample.ogg", "content_type":"audio/ogg",
        "ephemeral":false, "duration_secs":1.25, "waveform":"AAH+/w=="
    }]);
    value
}

struct Fixture {
    registry: ToolRegistry,
    state_dir: Utf8PathBuf,
    requests: Arc<Mutex<Vec<String>>>,
    server: tokio::task::JoinHandle<()>,
    _directory: tempfile::TempDir,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl Fixture {
    async fn new(responses: Vec<(u16, String)>, slot: &str) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let mut responses = VecDeque::from(responses);
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
                let (status, body) = responses.pop_front().expect("unexpected provider request");
                reader.get_mut().write_all(format!(
                    "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()
                ).as_bytes()).await.unwrap();
            }
        });
        let directory = tempfile::tempdir().unwrap();
        let state_dir = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).unwrap();
        tokio::fs::write(
            state_dir.join("config.toml"),
            "[delivery]\nevidence_markers_enabled = false\n\n[[channels]]\nid = \"100\"\n",
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
        let source = ModuleInstanceId::new(
            ProviderId::new("discord").unwrap(),
            ModuleSlotId::new(slot).unwrap(),
        );
        let registry = ToolRegistry::new(
            &HarnessConnection::claude(),
            registrations(services, source).unwrap(),
        )
        .unwrap();
        Self {
            registry,
            state_dir,
            requests,
            server,
            _directory: directory,
        }
    }

    async fn call(&self, tool: &str, arguments: &Value) -> Value {
        self.registry
            .find(tool)
            .unwrap()
            .call(arguments)
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn context_exact_registered_read_matches_frozen_metadata_contract() {
    let fixture = Fixture::new(vec![(200, exact_message().to_string())], "primary").await;
    let contract = fixtures();
    let actual = fixture
        .call(
            "get_message",
            &contract["exact_success"]["request"]["arguments"],
        )
        .await;
    assert_eq!(actual, contract["exact_success"]["expected"]);
    assert!(
        !actual
            .to_string()
            .contains("RECURSIVE_BODY_MUST_NOT_ESCAPE")
    );
    assert_eq!(
        *fixture.requests.lock().unwrap(),
        ["GET /api/v10/channels/100/messages/1002 HTTP/1.1"]
    );
}

#[tokio::test]
async fn context_versions_reject_before_provider_io() {
    let fixture = Fixture::new(vec![], "primary").await;
    for version in [
        Value::Null,
        json!(true),
        json!("2"),
        json!(2.0),
        json!(0),
        json!(3),
        json!([]),
        json!({}),
    ] {
        for tool in ["get_message", "fetch_messages"] {
            let error = fixture
                .registry
                .find(tool)
                .unwrap()
                .call(&json!({"channel_id":"100","message_id":"1002","response_version":version}))
                .await
                .unwrap_err();
            assert!(
                matches!(error.kind, ToolCallErrorKind::OwnerInput(_)),
                "{tool}: {version}"
            );
        }
    }
    assert!(fixture.requests.lock().unwrap().is_empty());
}

#[test]
fn context_discovery_versions_only_the_two_read_tools() {
    let tools: Vec<Value> = definitions().collect();
    assert_eq!(tools.len(), 6);
    for definition in tools {
        let version = &definition["inputSchema"]["properties"]["response_version"];
        if matches!(
            definition["name"].as_str(),
            Some("get_message" | "fetch_messages")
        ) {
            assert_eq!(version["type"], "integer");
            assert_eq!(version["enum"], json!([1, 2]));
        } else {
            assert!(version.is_null());
        }
    }
}

#[tokio::test]
async fn context_reference_cases_preserve_explicit_and_resolved_coordinates() {
    let contract = fixtures();
    let cases = contract["reference_cases"].as_array().unwrap();
    let responses = cases
        .iter()
        .map(|case| {
            let input = &case["input"];
            let mut raw = message();
            raw["type"] = input["message_type"].clone();
            let mut reference = input["native_reference"].clone();
            reference["type"] = reference.as_object_mut().unwrap().remove("kind").unwrap();
            raw["message_reference"] = reference;
            let resolved = &input["resolved_reference"];
            if !resolved.is_null() {
                let mut target = message();
                target["id"] = resolved["message_id"].clone();
                target["channel_id"] = resolved["channel_id"].clone();
                target["guild_id"] = resolved["guild_id"].clone();
                target["author"]["id"] = resolved["author_id"].clone();
                raw["referenced_message"] = target;
            }
            (200, raw.to_string())
        })
        .collect();
    let fixture = Fixture::new(responses, "primary").await;
    for case in cases {
        let actual = fixture
            .call(
                "get_message",
                &json!({"channel_id":"100","message_id":"1002","response_version":2}),
            )
            .await;
        assert_eq!(
            actual["message"]["relation"], case["expected"],
            "{}",
            case["name"]
        );
    }
}

#[tokio::test]
async fn context_conflicting_cursors_fail_without_http() {
    let fixture = Fixture::new(vec![], "primary").await;
    assert_eq!(
        fixture
            .call(
                "fetch_messages",
                &json!({"channel_id":"100","before":"1010","after":"999","response_version":2})
            )
            .await,
        fixtures()["errors"]["invalid_query"]
    );
    assert!(fixture.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn context_pages_match_frozen_before_and_empty_recent_contracts() {
    let fixture = Fixture::new(
        vec![
            (200, json!([message()]).to_string()),
            (200, "[]".to_owned()),
        ],
        "primary",
    )
    .await;
    let contract = fixtures();
    for name in ["page_success", "empty_recent_page"] {
        assert_eq!(
            fixture
                .call("fetch_messages", &contract[name]["request"]["arguments"])
                .await,
            contract[name]["expected"]
        );
    }
    let requests = fixture.requests.lock().unwrap();
    assert!(requests[0].starts_with("GET /api/v10/channels/100/messages?"));
    assert!(requests[0].contains("before=1010"));
    assert!(requests[0].contains("limit=1"));
    assert!(requests[1].contains("limit=20"));
    assert!(!requests[1].contains("before="));
    assert!(!requests[1].contains("after="));
}

#[tokio::test]
async fn context_after_page_reports_requested_anchor_and_actual_ordered_bounds() {
    let mut newer = message();
    newer["id"] = json!("1003");
    let fixture = Fixture::new(
        vec![(200, json!([newer, message()]).to_string())],
        "secondary",
    )
    .await;
    let actual = fixture
        .call(
            "fetch_messages",
            &json!({"channel_id":"100","after":"999","limit":3,"response_version":2}),
        )
        .await;
    assert_eq!(actual["page"]["query"]["direction"], "after");
    assert_eq!(
        actual["page"]["query"]["requested_anchor"]["message"],
        json!({"kind":"numeric","value":"999"})
    );
    assert_eq!(actual["page"]["query"]["effective_limit"], 3);
    assert_eq!(actual["page"]["count"], 2);
    assert_eq!(actual["page"]["has_more_hint"], false);
    assert_eq!(
        actual["page"]["oldest_observed"]["message"]["value"],
        "1002"
    );
    assert_eq!(
        actual["page"]["newest_observed"]["message"]["value"],
        "1003"
    );
    assert_eq!(
        actual["messages"][0]["reference"]["message"]["value"],
        "1002"
    );
    assert_eq!(
        actual["messages"][1]["reference"]["message"]["value"],
        "1003"
    );
    assert_eq!(
        actual["messages"][0]["reference"]["source"]["slot"],
        "secondary"
    );
    assert_eq!(
        actual["messages"][0]["sender"]["reference"]["source"]["slot"],
        "secondary"
    );
    assert!(fixture.requests.lock().unwrap()[0].contains("after=999"));
}

#[tokio::test]
async fn context_each_direction_reports_empty_short_and_full_page_hints() {
    for direction in ["recent", "before", "after"] {
        for count in 0..=2 {
            let values: Vec<Value> = (0..count)
                .rev()
                .map(|index| {
                    let mut raw = message();
                    raw["id"] = json!((1002 + index).to_string());
                    raw
                })
                .collect();
            let fixture = Fixture::new(vec![(200, json!(values).to_string())], "primary").await;
            let mut arguments = json!({"channel_id":"100","limit":2,"response_version":2});
            if direction != "recent" {
                arguments[direction] = json!("1000");
            }
            let actual = fixture.call("fetch_messages", &arguments).await;
            assert_eq!(actual["page"]["query"]["direction"], direction);
            assert_eq!(actual["page"]["count"], count);
            assert_eq!(actual["page"]["has_more_hint"], count == 2);
            assert_eq!(actual["messages"].as_array().unwrap().len(), count);
            if count == 0 {
                assert!(actual["page"]["oldest_observed"].is_null());
                assert!(actual["page"]["newest_observed"].is_null());
            } else {
                assert_eq!(
                    actual["page"]["oldest_observed"]["message"]["value"],
                    "1002"
                );
                assert_eq!(
                    actual["page"]["newest_observed"]["message"]["value"],
                    (1001 + count).to_string()
                );
            }
            let requests = fixture.requests.lock().unwrap();
            if direction == "recent" {
                assert!(!requests[0].contains("before="));
                assert!(!requests[0].contains("after="));
                assert!(actual["page"]["query"]["requested_anchor"].is_null());
            } else {
                assert!(requests[0].contains(&format!("{direction}=1000")));
                assert_eq!(
                    actual["page"]["query"]["requested_anchor"]["message"]["value"],
                    "1000"
                );
            }
        }
    }
}

#[tokio::test]
async fn context_errors_are_safe_and_policy_reload_prevents_http() {
    let hostile =
        json!({"code":50001,"message":"PRIVATE_PROVIDER_CONTENT https://private.invalid/token"})
            .to_string();
    let fixture = Fixture::new(
        vec![
            (403, hostile.clone()),
            (404, hostile),
            (200, "PRIVATE_MALFORMED_RESPONSE".to_owned()),
        ],
        "primary",
    )
    .await;
    let contract = fixtures();
    let arguments = json!({"channel_id":"100","message_id":"1002","response_version":2});
    for code in [
        "provider_access_denied",
        "provider_not_found",
        "provider_error",
    ] {
        assert_eq!(
            fixture.call("get_message", &arguments).await,
            contract["errors"][code]
        );
    }
    tokio::fs::write(
        fixture.state_dir.join("config.toml"),
        "channels = []\n[delivery]\nevidence_markers_enabled = false\n",
    )
    .await
    .unwrap();
    let (_, error) = crate::config::reload_config(&fixture.state_dir).await;
    assert!(error.is_none(), "{error:?}");
    for tool in ["get_message", "fetch_messages"] {
        assert_eq!(
            fixture.call(tool, &arguments).await,
            contract["errors"]["local_access_denied"]
        );
    }
    assert_eq!(fixture.requests.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn context_rejects_wrong_exact_target_and_cross_channel_page() {
    let mut wrong = message();
    wrong["channel_id"] = json!("101");
    let fixture = Fixture::new(
        vec![
            (200, wrong.to_string()),
            (200, json!([message(), wrong]).to_string()),
        ],
        "primary",
    )
    .await;
    for tool in ["get_message", "fetch_messages"] {
        assert_eq!(
            fixture
                .call(
                    tool,
                    &json!({"channel_id":"100","message_id":"1002","response_version":2})
                )
                .await,
            fixtures()["errors"]["invalid_observation"]
        );
    }
}

#[tokio::test]
async fn context_opt_in_preserves_omitted_and_explicit_legacy_responses() {
    let fixture = Fixture::new(
        vec![
            (200, message().to_string()),
            (200, message().to_string()),
            (200, json!([message()]).to_string()),
            (200, json!([message()]).to_string()),
        ],
        "primary",
    )
    .await;
    for tool in ["get_message", "fetch_messages"] {
        let mut arguments = json!({"channel_id":"100","message_id":"1002"});
        let omitted = fixture.call(tool, &arguments).await;
        arguments["response_version"] = json!(1);
        assert_eq!(fixture.call(tool, &arguments).await, omitted);
        assert!(omitted.get("response_version").is_none());
        let observation = if tool == "get_message" {
            &omitted
        } else {
            &omitted["messages"][0]
        };
        assert_eq!(observation["id"], "1002");
        assert_eq!(observation["content"], "Synthetic history fixture.");
        assert!(observation.get("native").is_none());
    }
}

#[tokio::test]
async fn context_reference_availability_does_not_invent_missing_coordinates() {
    let mut resolved_only = exact_message();
    resolved_only
        .as_object_mut()
        .unwrap()
        .remove("message_reference");
    let mut incomplete = exact_message();
    incomplete["message_reference"]
        .as_object_mut()
        .unwrap()
        .remove("message_id");
    let mut guild_conflict = exact_message();
    guild_conflict["referenced_message"]["guild_id"] = json!("501");
    let mut missing_guild = exact_message();
    missing_guild["referenced_message"]
        .as_object_mut()
        .unwrap()
        .remove("guild_id");
    let fixture = Fixture::new(
        vec![resolved_only, incomplete, guild_conflict, missing_guild]
            .into_iter()
            .map(|value| (200, value.to_string()))
            .collect(),
        "primary",
    )
    .await;
    for (consistency, target_known) in [
        ("resolved_only", false),
        ("insufficient_evidence", false),
        ("conflicts", true),
        ("matches", true),
    ] {
        let result = fixture
            .call(
                "get_message",
                &json!({"channel_id":"100","message_id":"1002","response_version":2}),
            )
            .await;
        let relation = &result["message"]["relation"]["value"];
        assert_eq!(relation["consistency"], consistency);
        assert_eq!(
            relation["target"]["status"],
            if target_known { "known" } else { "unavailable" }
        );
        assert_eq!(
            relation["resolved"]["value"]["sender"]["actor"]["value"],
            "201"
        );
        if !target_known {
            assert_eq!(relation["target"]["reason"], "not_reported");
        }
        if consistency == "resolved_only" {
            assert_eq!(relation["kind"], "unknown");
        }
    }
}

#[tokio::test]
async fn context_attachment_order_and_empty_waveform_remain_observed() {
    let mut raw = exact_message();
    let mut second = raw["attachments"][0].clone();
    second["id"] = json!("301");
    second["filename"] = json!("empty.ogg");
    second["waveform"] = json!("");
    second.as_object_mut().unwrap().remove("content_type");
    raw["attachments"].as_array_mut().unwrap().push(second);
    let fixture = Fixture::new(vec![(200, raw.to_string())], "primary").await;
    let result = fixture
        .call(
            "get_message",
            &json!({"channel_id":"100","message_id":"1002","response_version":2}),
        )
        .await;
    let native = &result["message"]["native"]["attachments"];
    assert_eq!(native[0]["id"], "300");
    assert_eq!(native[1]["id"], "301");
    assert_eq!(
        native[1]["waveform"],
        json!({"encoding":"base64","value":""})
    );
    assert_eq!(
        result["message"]["attachments"]["value"][1]["media_type"],
        json!({"status":"unavailable","reason":"not_reported"})
    );
}
