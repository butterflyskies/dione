//! Actual root registry integration for the extracted Discord operation owner.
use camino::Utf8PathBuf;
use dione_access_requests::AccessQueue;
use dione_discord_runtime::IngressLedger;
use dione_discord_state::new_state;
use dione_outbound_consent::consent::ConsentGate;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

pub(super) async fn fake_discord_http() -> (
    Arc<serenity::http::Http>,
    Arc<std::sync::Mutex<Vec<(String, String)>>>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake Discord API");
    let address = listener.local_addr().expect("fake Discord API address");
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = Arc::clone(&requests);
    let server = tokio::spawn(async move {
        // Each posted message gets its own id (9001, 9002, ...), so a
        // test can tell one chunk's id from another's.
        let mut next_post_id = 9001u64;
        let mut rejected_known_once = false;
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                let Ok(read) = stream.read(&mut buffer).await else {
                    return;
                };
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n")
                else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|value| value.trim().parse::<usize>().ok())
                    })
                    .unwrap_or_default();
                if request.len() >= header_end + 4 + content_length {
                    break;
                }
            }

            let request_text = String::from_utf8_lossy(&request);
            let request_line = request_text.lines().next().unwrap_or_default();
            let path = request_line.split_whitespace().nth(1).unwrap_or_default();
            let body = request_text
                .split_once("\r\n\r\n")
                .map_or("", |(_, body)| body);
            captured
                .lock()
                .expect("request capture lock")
                .push((path.to_owned(), body.to_owned()));

            let (status, response_body) = if path.ends_with("/users/@me/channels")
                && serde_json::from_str::<Value>(body).unwrap()["recipient_id"] == "78"
            {
                (
                    "403 Forbidden",
                    json!({"code": 50007, "message": "Cannot send messages to this user"})
                        .to_string(),
                )
            } else if path.ends_with("/users/@me/channels") {
                (
                    "200 OK",
                    json!({
                        "id": "4242",
                        "last_message_id": null,
                        "last_pin_timestamp": null,
                        "type": 1,
                        "recipients": [{
                            "id": "77",
                            "username": "recipient",
                            "global_name": null,
                            "avatar": null,
                            "discriminator": "0",
                            "public_flags": 0,
                            "bot": false
                        }]
                    })
                    .to_string(),
                )
            } else if path.ends_with("/typing") {
                ("204 No Content", String::new())
            } else if request_line.starts_with("GET ")
                    // 7001 is a source message whose id no posted reply
                    // shares (posted ids start at 9001).
                    && [7001, 9001, 9004, 9006, 9007, 9008]
                        .iter()
                        .any(|id| path.ends_with(&format!("/channels/42/messages/{id}")))
            {
                let id: u64 = path.rsplit('/').next().unwrap().parse().unwrap();
                let mut message = wire_message(
                    id,
                    "explicit read",
                    "2026-08-15T09:00:00.000000+00:00",
                    json!([]),
                );
                message["channel_id"] = json!("42");
                if id != 9008 {
                    message["guild_id"] = json!("500");
                }
                if id == 9004 {
                    message["author"]["bot"] = json!(true);
                }
                if id == 9007 {
                    message["message_reference"] =
                        json!({"message_id":"9006", "channel_id":"42", "guild_id":"500"});
                }
                ("200 OK", message.to_string())
            } else if request_line.starts_with("GET ") && path.ends_with("/channels/42") {
                (
                    "200 OK",
                    json!({"id":"42", "type":0, "guild_id":"500",
                        "position":0, "permission_overwrites":[], "name":"fixture",
                        "nsfw":false, "parent_id":null, "topic":null,
                        "last_message_id":null})
                    .to_string(),
                )
            } else if request_line.starts_with("GET ") && path.ends_with("/channels/42/messages/8")
            {
                (
                    "404 Not Found",
                    json!({ "message": "Unknown Message", "code": 10008 }).to_string(),
                )
            } else if request_line.starts_with("GET ")
                && path.ends_with("/channels/42/messages/9003")
            {
                (
                    "403 Forbidden",
                    json!({ "message": "Missing Access", "code": 50001 }).to_string(),
                )
            } else if request_line.starts_with("GET ") && path.contains("/messages/") {
                ("200 OK", {
                    let mut message = wire_message(
                        9001,
                        "explicit read",
                        "2026-08-15T09:00:00.000000+00:00",
                        json!([]),
                    );
                    if path.ends_with("/messages/9015") {
                        message["flags"] = json!(
                            (MessageFlags::SUPPRESS_NOTIFICATIONS | MessageFlags::SUPPRESS_EMBEDS)
                                .bits()
                                | MESSAGE_FLAG_NOT_IN_SDK
                        );
                    }
                    if path.ends_with("/messages/9016") {
                        message["flags"] = json!("invalid");
                    }
                    message.to_string()
                })
            } else if request_line.starts_with("GET ") && path.contains("/messages?") {
                let mut message = wire_message(
                    9002,
                    "incidental fetch",
                    "2026-08-15T09:00:01.000000+00:00",
                    json!([]),
                );
                message["channel_id"] = json!("42");
                ("200 OK", json!([message]).to_string())
            } else if request_line.starts_with("PUT ") && path.contains("/reactions/") {
                ("204 No Content", String::new())
            } else if request_line.starts_with("POST ")
                && path.ends_with("/messages")
                && body.contains("known-no-effect")
                && !rejected_known_once
            {
                rejected_known_once = true;
                (
                    "403 Forbidden",
                    json!({"message":"Missing Permissions","code":50013}).to_string(),
                )
            } else if request_line.starts_with("POST ")
                && path.ends_with("/messages")
                && body.contains("force-pending")
            {
                std::future::pending::<(&str, String)>().await
            } else if request_line.starts_with("POST ")
                && path.ends_with("/messages")
                && body.contains("force-uncertain")
            {
                ("200 OK", "{\"id\":\"9999\"}".to_owned())
            } else if request_line.starts_with("POST ")
                && path.ends_with("/messages")
                && body.contains("force-delivery-failure")
            {
                (
                    "403 Forbidden",
                    json!({ "message": "Missing Permissions", "code": 50013 }).to_string(),
                )
            } else if request_line.starts_with("PATCH ") && path.contains("/messages/") {
                let body: Value = serde_json::from_str(body).expect("edit body");
                (
                    "200 OK",
                    wire_message(
                        9001,
                        body["content"].as_str().unwrap_or_default(),
                        "2026-08-15T09:00:00.000000+00:00",
                        json!([]),
                    )
                    .to_string(),
                )
            } else if request_line.starts_with("POST ") && path.ends_with("/messages") {
                let content = serde_json::from_str::<Value>(body)
                    .ok()
                    .and_then(|body| body["content"].as_str().map(str::to_owned))
                    .unwrap_or_default();
                let id = next_post_id;
                next_post_id += 1;
                (
                    "200 OK",
                    wire_message(id, &content, "2026-08-15T09:00:00.000000+00:00", json!([]))
                        .to_string(),
                )
            } else {
                ("404 Not Found", "{}".to_owned())
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                response_body.len()
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("write fake Discord response");
        }
    });
    let http = serenity::http::HttpBuilder::new("fake")
        .proxy(format!("http://{address}"))
        .ratelimiter_disabled(true)
        .build();
    (Arc::new(http), requests, server)
}
fn posted_contents(requests: &std::sync::Mutex<Vec<(String, String)>>) -> Vec<String> {
    requests
        .lock()
        .expect("request capture lock")
        .iter()
        .filter(|(path, _)| path.ends_with("/messages"))
        .filter_map(|(_, body)| {
            serde_json::from_str::<Value>(body).ok()?["content"]
                .as_str()
                .map(str::to_owned)
        })
        .collect()
}
#[tokio::test]
async fn registered_reply_reloads_discovery_and_policy_and_preserves_options() {
    let dir = tempfile::tempdir().unwrap();
    let path = Utf8PathBuf::from_path_buf(dir.path().to_owned()).unwrap();
    std::fs::write(
        path.join("config.toml"),
        "[delivery]\nevidence_markers_enabled = false\n",
    )
    .unwrap();
    assert!(dione_config::reload_config(&path).await.1.is_none());
    let (http, requests, peer) = fake_discord_http().await;
    let (notifications, _rx) = tokio::sync::mpsc::channel(4);
    let server = crate::mcp::server::DioneServer::new(
        new_state(),
        Arc::new(tokio::sync::Mutex::new(AccessQueue::load(&path))),
        http,
        path.clone(),
        notifications,
        dione_tracing_channel::TraceLevelController::noop(),
        dione_claude::connect(
            dione_harness::ProtocolOutput::new(tokio::io::stdout()),
            dione_codex::TOOL_NAMES,
        ),
        Arc::new(ConsentGate::new(&path)),
        Arc::new(IngressLedger::new()),
        dione_contracts::ModuleInstanceId::new(
            dione_contracts::ProviderId::new("discord").unwrap(),
            dione_contracts::ModuleSlotId::new("primary").unwrap(),
        ),
        None,
    )
    .await
    .unwrap()
    .with_module_tools(Vec::new())
    .unwrap();
    let list = json!({"jsonrpc":"2.0","id":1,"method":"tools/list"});
    // Exercise the live registry and registered reply against the same reloaded pin.
    for (pin, marker, expected) in [
        (
            "v2",
            "[🔍=v2:<role>:<token>]",
            "pinned [🔍=v2:claim:AAAAAAAAAAE]",
        ),
        ("v3", "[🔍=v3:<role>:<token>]", "pinned [🔍=v3:claim:AQ]"),
    ] {
        std::fs::write(path.join("config.toml"), format!("[[channels]]\nid = \"42\"\nrequire_mention = false\n[delivery]\nevidence_markers_enabled = true\nevidence_marker_version = \"{pin}\"\n")).unwrap();
        assert!(dione_config::reload_config(&path).await.1.is_none());
        let listed = crate::mcp::server::test_helpers::dispatch_request(&server, list.clone())
            .await
            .unwrap();
        for name in ["reply", "send_dm"] {
            let tool = listed["result"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|tool| tool["name"] == name)
                .unwrap();
            for field in ["claim_handles", "citation_handles"] {
                assert!(
                    tool["inputSchema"]["properties"][field]["description"]
                        .as_str()
                        .unwrap()
                        .contains(marker)
                );
            }
            assert!(
                tool["inputSchema"]["properties"]
                    .get("evidence_marker_version")
                    .is_none()
            );
        }
        let result = crate::mcp::server::test_helpers::dispatch_request(&server, json!({"jsonrpc":"2.0","id":99,"method":"tools/call","params":{"name":"reply","arguments":{"channel_id":"42","content":"pinned","claim_handles":["1"]}}})).await.unwrap();
        assert!(result["result"].get("isError").is_none(), "{result}");
        let bodies = posted_contents(&requests);
        assert_eq!(bodies.last().unwrap(), expected);
    }
    requests.lock().unwrap().clear();
    std::fs::write(
        path.join("config.toml"),
        "[delivery]\nevidence_markers_enabled = false\n",
    )
    .unwrap();
    assert!(dione_config::reload_config(&path).await.1.is_none());
    let initial = crate::mcp::server::test_helpers::dispatch_request(&server, list.clone())
        .await
        .unwrap();
    let reply = initial["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "reply")
        .unwrap();
    assert!(
        reply["inputSchema"]["properties"]
            .get("claim_handles")
            .is_none()
    );
    assert_eq!(
        initial["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|tool| tool["name"] == "reply")
            .count(),
        1
    );
    let call = |arguments| json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"reply","arguments":arguments}});
    let disabled = crate::mcp::server::test_helpers::dispatch_request(
        &server,
        call(json!({"channel_id":"42","content":"hello","claim_handles":[]})),
    )
    .await
    .unwrap();
    assert_eq!(
        disabled["error"]["message"],
        "claim_handles and citation_handles require evidence_markers_enabled"
    );
    let denied = crate::mcp::server::test_helpers::dispatch_request(
        &server,
        call(json!({"channel_id":"42","content":"hello"})),
    )
    .await
    .unwrap();
    let denied: Value =
        serde_json::from_str(denied["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert!(
        denied["error"]
            .as_str()
            .unwrap()
            .contains("not a permitted outbound target")
    );
    assert!(requests.lock().unwrap().is_empty());

    std::fs::write(path.join("config.toml"), "[[channels]]\nid = \"42\"\nrequire_mention = false\n[delivery]\nevidence_markers_enabled = true\n").unwrap();
    assert!(dione_config::reload_config(&path).await.1.is_none());
    let enabled = crate::mcp::server::test_helpers::dispatch_request(&server, list)
        .await
        .unwrap();
    let reply = enabled["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "reply")
        .unwrap();
    assert_eq!(
        reply["inputSchema"]["properties"]["claim_handles"]["type"],
        "array"
    );
    assert_eq!(
        reply["inputSchema"]["properties"]["citation_handles"]["type"],
        "array"
    );
    let accepted = crate::mcp::server::test_helpers::dispatch_request(&server, call(json!({"channel_id":42,"content":"hello","reply_to_message_id":"7001","suppress_ping":true,"suppress_embeds":true,"no_rly_hooks":[],"claim_handles":["1"]}))).await.unwrap();
    assert!(accepted["result"].get("isError").is_none(), "{accepted}");
    let accepted: Value =
        serde_json::from_str(accepted["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(accepted["message_ids"], json!([9003]));
    assert_eq!(accepted["ok"], true);
    assert_eq!(accepted["claim_locators"].as_array().unwrap().len(), 1);
    let sent: Vec<Value> = requests
        .lock()
        .unwrap()
        .iter()
        .filter(|(route, body)| route.ends_with("/channels/42/messages") && !body.is_empty())
        .map(|(_, body)| serde_json::from_str(body).unwrap())
        .collect();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["message_reference"]["message_id"], "7001");
    assert_eq!(sent[0]["allowed_mentions"]["replied_user"], false);
    assert_eq!(sent[0]["flags"], 4);
    assert!(
        sent[0]["content"]
            .as_str()
            .unwrap()
            .starts_with("hello [🔍=v3:claim:")
    );

    std::fs::write(path.join("config.toml"), "[[channels]]\nid = \"42\"\nrequire_mention = false\n[contradictionary]\nenabled = true\n[[contradictionary.entries]]\npattern = \"blocked\"\naction = \"block\"\nmatch_mode = \"word\"\n").unwrap();
    assert!(dione_config::reload_config(&path).await.1.is_none());
    let before_hold = requests.lock().unwrap().len();
    let held = crate::mcp::server::test_helpers::dispatch_request(
        &server,
        call(json!({"channel_id":"42","content":"blocked"})),
    )
    .await
    .unwrap();
    assert_eq!(held["result"]["isError"], true);
    let held: Value =
        serde_json::from_str(held["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    let handle = held["held"]["handle"]
        .as_str()
        .expect("reply issued a held capability");
    assert_eq!(requests.lock().unwrap().len(), before_hold);
    let release = json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"no_rly","arguments":{"handle":handle}}});
    let released = crate::mcp::server::test_helpers::dispatch_request(&server, release.clone())
        .await
        .unwrap();
    assert!(released["result"].get("isError").is_none(), "{released}");
    let released: Value =
        serde_json::from_str(released["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(released["ok"], true);
    assert_eq!(released["released"], handle);
    assert_eq!(released["message_ids"], json!([9004]));
    let release_count = requests.lock().unwrap().len();
    let duplicate = crate::mcp::server::test_helpers::dispatch_request(&server, release)
        .await
        .unwrap();
    assert_eq!(duplicate["result"]["isError"], true);
    assert_eq!(requests.lock().unwrap().len(), release_count);

    let before_denial = requests.lock().unwrap().len();
    std::fs::write(
        path.join("config.toml"),
        "[delivery]\nevidence_markers_enabled = false\n",
    )
    .unwrap();
    assert!(dione_config::reload_config(&path).await.1.is_none());
    let denied = crate::mcp::server::test_helpers::dispatch_request(
        &server,
        call(json!({"channel_id":"42","content":"after revocation"})),
    )
    .await
    .unwrap();
    assert_eq!(denied["result"]["isError"], true);
    assert_eq!(requests.lock().unwrap().len(), before_denial);
    peer.abort();
}
