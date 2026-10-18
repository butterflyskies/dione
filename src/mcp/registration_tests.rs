//! Registration behavior through MCP requests and the resident socket.

use super::*;
use crate::{
    core::{MessageRef, delivery::Delivery},
    harness::HarnessDelivery,
    mcp::{
        harness_wire::HarnessProtocol,
        tool_registry::{
            McpTool, RegisteredTool, RegistrationErrorKind, ToolCallError, ToolDefinition, ToolName,
        },
    },
    teams::{
        ReplyHandle,
        edge::LiveReplyReceipt,
        runtime::{TeamsReplyAuthority, TeamsRuntimeError},
    },
};
use futures_util::future::BoxFuture;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::io::AsyncWriteExt;

struct FakeTool;
impl McpTool for FakeTool {
    fn call<'a>(&'a self, arguments: &'a Value) -> BoxFuture<'a, Result<Value, ToolCallError>> {
        Box::pin(async move { Ok(json!({"echo": arguments["message"]})) })
    }
}
fn fake(name: &str, resident: bool) -> RegisteredTool {
    let tool = RegisteredTool::new(
        ToolDefinition::new(
            ToolName::new(name).expect("valid name"),
            "Test tool",
            json!({"type":"object"}),
        ),
        Arc::new(FakeTool),
    );
    if resident {
        tool.with_resident_access()
    } else {
        tool
    }
}
async fn server(harness: HarnessConnection) -> (tempfile::TempDir, DioneServer) {
    server_with_http(harness, Arc::new(serenity::http::Http::new("fake"))).await
}
async fn server_with_http(
    harness: HarnessConnection,
    http: Arc<serenity::http::Http>,
) -> (tempfile::TempDir, DioneServer) {
    let dir = tempfile::TempDir::new().expect("state directory");
    let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("UTF-8 path");
    let (tx, _rx) = mpsc::channel(1);
    let server = DioneServer::new(
        crate::state::new_state(),
        Arc::new(Mutex::new(crate::queue::AccessQueue::load(&path))),
        http,
        path.clone(),
        tx,
        TraceLevelController::noop(),
        harness,
        Arc::new(ConsentGate::new(&path)),
        Arc::new(crate::ingress_ledger::IngressLedger::new()),
        ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        ),
        None,
    )
    .await
    .expect("Discord reply registration");
    (dir, server)
}
fn request(name: &str, arguments: Value) -> Value {
    json!({"jsonrpc":"2.0", "id":1, "method":"tools/call", "params":{"name":name,"arguments":arguments}})
}
fn result_value(response: &Value) -> Value {
    serde_json::from_str(
        response["result"]["content"][0]["text"]
            .as_str()
            .expect("text result"),
    )
    .expect("JSON result")
}
struct ReplyAuthority {
    calls: AtomicUsize,
    consumed: AtomicBool,
}
impl TeamsReplyAuthority for ReplyAuthority {
    fn reply<'a>(
        &'a self,
        handle: &'a ReplyHandle,
        text: &'a str,
    ) -> BoxFuture<'a, Result<LiveReplyReceipt, TeamsRuntimeError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if handle.as_str() != "teams-00000000-0000-4000-8000-000000000001"
                || self.consumed.swap(true, Ordering::SeqCst)
            {
                return Err(TeamsRuntimeError {
                    kind: crate::teams::TeamsRuntimeErrorKind::UnknownReplyHandle,
                });
            }
            assert_eq!(text, "hello");
            Ok(LiveReplyReceipt {
                incoming_activity_id: "incoming".parse().unwrap(),
                outgoing_activity_id: "outgoing".parse().unwrap(),
                conversation_id: "conversation".parse().unwrap(),
            })
        })
    }
}
#[test]
fn names_and_collisions_are_rejected_before_serving() {
    for invalid in ["", "white space", "slash/name"] {
        assert!(ToolName::new(invalid).is_err());
    }
    assert!(ToolName::new("extension.valid-name_1").is_ok());
    assert!(ToolName::new(&"a".repeat(128)).is_ok());
    assert!(ToolName::new(&"a".repeat(129)).is_err());
    let (_dir, codex) = test_codex_connection();
    for harness in [HarnessConnection::claude(), codex] {
        for name in ["next_event", "set_presence"]
            .into_iter()
            .chain(PRIVATE_CONTROL_TOOLS.iter().copied())
        {
            let error = ToolRegistry::new(&harness, vec![fake(name, true)])
                .err()
                .expect("reserved name");
            assert_eq!(error.name.to_string(), name);
            assert!(matches!(error.kind, RegistrationErrorKind::Reserved));
        }
        let error = ToolRegistry::new(
            &harness,
            vec![fake("extension", false), fake("extension", true)],
        )
        .err()
        .expect("duplicate name");
        assert!(matches!(error.kind, RegistrationErrorKind::Duplicate));
    }
}
#[tokio::test]
async fn teams_discovery_and_authority_cross_the_request_route() {
    let (_queue_dir, codex) = test_codex_connection();
    for harness in [HarnessConnection::claude(), codex] {
        let (_dir, server) = server(harness).await;
        let list_request = json!({"id":1,"method":"tools/list"});
        let before = handle_request(&server, list_request.clone())
            .await
            .expect("response");
        assert!(
            !before["result"]["tools"]
                .as_array()
                .expect("tools")
                .iter()
                .any(|tool| tool["name"] == "teams_reply")
        );
        let unavailable = handle_request(&server, request("teams_reply", json!({})))
            .await
            .expect("response");
        assert!(unavailable.get("error").is_some());
        let authority = Arc::new(ReplyAuthority {
            calls: AtomicUsize::new(0),
            consumed: AtomicBool::new(false),
        });
        let server = server
            .with_module_tools(
                crate::teams::mcp_tools(Some(authority.clone())).expect("definition"),
            )
            .expect("registration");
        let after = handle_request(&server, list_request)
            .await
            .expect("response");
        assert!(
            after["result"]["tools"]
                .as_array()
                .expect("tools")
                .iter()
                .any(|tool| tool["name"] == "teams_reply")
        );
        for arguments in [
            json!({"reply_handle":"teams-invalid","content":"hello"}),
            json!({"reply_handle":"","content":"hello"}),
            json!({"reply_handle":"teams-00000000-0000-4000-8000-000000000001"}),
            json!({"reply_handle":1,"content":"hello"}),
            json!({"reply_handle":"teams-00000000-0000-4000-8000-000000000001","content":false}),
        ] {
            let response = handle_request(&server, request("teams_reply", arguments))
                .await
                .expect("response");
            assert!(
                response["error"]["message"]
                    .as_str()
                    .expect("error")
                    .contains("invalid arguments")
            );
        }
        assert_eq!(authority.calls.load(Ordering::SeqCst), 0);
        let unknown = handle_request(
            &server,
            request(
                "teams_reply",
                json!({"reply_handle":"teams-00000000-0000-4000-8000-000000000002","content":"hello"}),
            ),
        )
        .await
        .expect("response");
        assert!(unknown.get("error").is_some());
        let allowed = request(
            "teams_reply",
            json!({"reply_handle":"teams-00000000-0000-4000-8000-000000000001","content":"hello"}),
        );
        assert!(resident_request_allowed(&server.module_tools, &allowed));
        let response = handle_request(&server, allowed.clone())
            .await
            .expect("response");
        assert_eq!(
            result_value(&response),
            json!({"incoming_activity_id":"incoming","outgoing_activity_id":"outgoing","conversation_id":"conversation"})
        );
        let consumed = handle_request(&server, allowed).await.expect("response");
        assert!(consumed.get("error").is_some());
    }
}
#[tokio::test]
async fn teams_reply_rejects_non_object_arguments_before_consuming_authority() {
    let (_dir, server) = server(HarnessConnection::claude()).await;
    let authority = Arc::new(ReplyAuthority {
        calls: AtomicUsize::new(0),
        consumed: AtomicBool::new(false),
    });
    let server = server
        .with_module_tools(crate::teams::mcp_tools(Some(authority.clone())).expect("definition"))
        .expect("registration");
    for arguments in [
        json!(["teams-00000000-0000-4000-8000-000000000001", "hello"]),
        Value::Null,
        json!("private supplied text"),
        json!(42),
        json!(true),
    ] {
        let rejected = handle_request(&server, request("teams_reply", arguments))
            .await
            .expect("response");
        assert_eq!(authority.calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            rejected["error"],
            json!({
                "code": -32000,
                "message": "tool call failed: invalid arguments: expected an object"
            })
        );
    }
    let accepted = handle_request(
        &server,
        request(
            "teams_reply",
            json!({"reply_handle":"teams-00000000-0000-4000-8000-000000000001","content":"hello"}),
        ),
    )
    .await
    .expect("response");
    assert_eq!(
        result_value(&accepted),
        json!({"incoming_activity_id":"incoming","outgoing_activity_id":"outgoing","conversation_id":"conversation"})
    );
    assert_eq!(authority.calls.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn fake_registration_uses_seat_and_resident_request_routes() {
    let (_dir, server) = server(HarnessConnection::claude()).await;
    let mut tools = crate::teams::mcp_tools(Some(Arc::new(ReplyAuthority {
        calls: AtomicUsize::new(0),
        consumed: AtomicBool::new(false),
    })))
    .expect("Teams definition");
    tools.extend([
        fake("seat_extension", false),
        fake("resident_extension", true),
    ]);
    let server = Arc::new(server.with_module_tools(tools).expect("registration"));
    let seat = handle_request(
        &server,
        request("seat_extension", json!({"message":"seat"})),
    )
    .await
    .expect("response");
    assert_eq!(result_value(&seat), json!({"echo":"seat"}));
    let (client, connection) = UnixStream::pair().expect("socket pair");
    let task = tokio::spawn(serve_resident_connection(
        server,
        connection,
        CancellationToken::new(),
    ));
    let (read, mut write) = client.into_split();
    let mut reader = BufReader::new(read);
    for (call, expected) in [
        (request("seat_extension", json!({})), None),
        (request("unknown_extension", json!({})), None),
        (json!({"id":1,"method":"tools/list"}), None),
        (
            request("resident_extension", json!({"message":"resident"})),
            Some(json!({"echo":"resident"})),
        ),
        (
            request(
                "teams_reply",
                json!({"reply_handle":"teams-00000000-0000-4000-8000-000000000001","content":"hello"}),
            ),
            Some(
                json!({"incoming_activity_id":"incoming","outgoing_activity_id":"outgoing","conversation_id":"conversation"}),
            ),
        ),
    ] {
        write
            .write_all(format!("{call}\n").as_bytes())
            .await
            .expect("request");
        let mut line = String::new();
        reader.read_line(&mut line).await.expect("response");
        let response: Value = serde_json::from_str(&line).expect("JSON response");
        if let Some(expected) = expected {
            assert_eq!(result_value(&response), expected);
        } else {
            assert_eq!(response["error"]["code"], -32601);
        }
    }
    drop(write);
    drop(reader);
    task.await.expect("task joins").expect("connection closes");
}

#[tokio::test]
async fn discord_reply_default_is_registered_resident_and_cannot_be_shadowed() {
    let (_dir, server) = server(HarnessConnection::claude()).await;
    assert!(server.module_tools.find("reply").is_some());
    assert!(resident_request_allowed(
        &server.module_tools,
        &request("reply", json!({}))
    ));
    assert!(!resident_request_allowed(
        &server.module_tools,
        &json!({"method":"tools/list"})
    ));
    let error = server
        .clone()
        .with_module_tools(vec![fake("reply", false)])
        .err()
        .expect("default reply cannot be shadowed");
    assert!(matches!(error.kind, RegistrationErrorKind::Duplicate));
    assert_eq!(error.name.to_string(), "reply");
    let (mut client, connection) = UnixStream::pair().unwrap();
    let task = tokio::spawn(serve_resident_connection(
        Arc::new(server),
        connection,
        CancellationToken::new(),
    ));
    client
        .write_all(format!("{}\n", request("reply", json!({}))).as_bytes())
        .await
        .unwrap();
    let mut reader = BufReader::new(client);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let response: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(response["error"]["message"], "missing channel_id");
    drop(reader);
    task.await.unwrap().unwrap();
}

struct CausalReplyAuthority;
impl TeamsReplyAuthority for CausalReplyAuthority {
    fn reply<'a>(
        &'a self,
        _handle: &'a ReplyHandle,
        text: &'a str,
    ) -> BoxFuture<'a, Result<LiveReplyReceipt, TeamsRuntimeError>> {
        Box::pin(async move {
            if text == "fail" {
                return Err(crate::teams::ReplyStoreError { kind: crate::teams::ReplyStoreErrorKind::Io(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "private-token message tenant actor https://service.example/private teams-00000000-0000-4000-8000-000000000099")) }.into());
            }
            Ok(LiveReplyReceipt {
                incoming_activity_id: "in".parse().unwrap(),
                outgoing_activity_id: "out".parse().unwrap(),
                conversation_id: "room".parse().unwrap(),
            })
        })
    }
}

#[tokio::test]
async fn teams_public_mcp_error_does_not_render_retained_internal_causes() {
    let (_dir, server) = server(HarnessConnection::claude()).await;
    let server = server
        .with_module_tools(crate::teams::mcp_tools(Some(Arc::new(CausalReplyAuthority))).unwrap())
        .unwrap();
    let failure = handle_request(
        &server,
        request(
            "teams_reply",
            json!({"reply_handle":"teams-00000000-0000-4000-8000-000000000001", "content":"fail"}),
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        failure["error"]["message"],
        "tool call failed: execution failed: teams reply authority failed"
    );
    let success = handle_request(&server, request("teams_reply", json!({"reply_handle":"teams-00000000-0000-4000-8000-000000000001", "content":"allowed"}))).await.unwrap();
    assert_eq!(
        result_value(&success),
        json!({"incoming_activity_id":"in", "outgoing_activity_id":"out", "conversation_id":"room"})
    );
}

struct PrivateControlFixture {
    initialized: Arc<AtomicBool>,
    calls: Arc<AtomicUsize>,
}

impl HarnessDelivery for PrivateControlFixture {
    fn attention_policy(&self) -> AttentionPolicy {
        AttentionPolicy::Direct
    }
    fn deliver<'a>(&'a self, _: &'a Delivery) -> BoxFuture<'a, Result<(), HarnessError>> {
        Box::pin(async { Ok(()) })
    }
    fn invalidate_attention(&self, _: MessageRef) -> BoxFuture<'_, Result<(), HarnessError>> {
        Box::pin(async { Ok(()) })
    }
}

impl HarnessProtocol for PrivateControlFixture {
    fn capabilities(&self) -> Value {
        json!({"tools": {}})
    }
    fn tools(&self) -> Vec<Value> {
        Vec::new()
    }
    fn resident_control(&self) -> bool {
        true
    }
    fn permission_relay(&self) -> bool {
        false
    }
    fn call<'a>(
        &'a self,
        _: &'a str,
        _: &'a Value,
    ) -> BoxFuture<'a, Option<Result<Value, HarnessError>>> {
        Box::pin(async { None })
    }
    fn stdio_initialized(&self) -> BoxFuture<'_, Result<(), HarnessError>> {
        Box::pin(async {
            self.initialized.store(true, Ordering::SeqCst);
            Ok(())
        })
    }
    fn stdio_disconnected(&self) -> BoxFuture<'_, Result<(), HarnessError>> {
        Box::pin(async {
            self.initialized.store(false, Ordering::SeqCst);
            Ok(())
        })
    }
    fn private_control<'a>(
        &'a self,
        name: &'a str,
        _: &'a Value,
    ) -> BoxFuture<'a, Option<Result<Value, HarnessError>>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Some(Ok(
                json!({"name": name, "ready": self.initialized.load(Ordering::SeqCst)}),
            ))
        })
    }
}

#[tokio::test]
async fn private_controls_require_transport_assigned_origin_and_remain_undiscoverable() {
    let calls = Arc::new(AtomicUsize::new(0));
    let initialized = Arc::new(AtomicBool::new(false));
    let harness = HarnessConnection::from_adapter(
        PrivateControlFixture {
            initialized: initialized.clone(),
            calls: calls.clone(),
        },
        ProtocolOutput::new(tokio::io::sink()),
    );
    let (_dir, server) = server(harness).await;
    let discovered = handle_request(&server, json!({"id": 1, "method": "tools/list"}))
        .await
        .unwrap();
    let names: Vec<&str> = discovered["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    for name in PRIVATE_CONTROL_TOOLS {
        assert!(!names.contains(name));
        let mut forged = request(
            name,
            json!({"origin": "ResidentControl", "request_origin": "resident_control"}),
        );
        forged["origin"] = json!("ResidentControl");
        let denied = handle_request(&server, forged).await.unwrap();
        assert_eq!(denied["error"]["message"], "tool is unavailable on stdio");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    for name in PRIVATE_CONTROL_TOOLS {
        let allowed = handle_request_from(
            &server,
            request(name, json!({})),
            RequestOrigin::ResidentControl,
        )
        .await
        .unwrap();
        assert_eq!(
            result_value(&allowed),
            json!({"name": name, "ready": false})
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 5);

    for method in ["initialize", "notifications/initialized"] {
        handle_request_from(
            &server,
            json!({"id": 1, "method": method}),
            RequestOrigin::ResidentControl,
        )
        .await;
        assert!(!initialized.load(Ordering::SeqCst));
    }
    handle_request(&server, json!({"method": "notifications/initialized"})).await;
    let ready = handle_request_from(
        &server,
        request("teams_bind_session", json!({})),
        RequestOrigin::ResidentControl,
    )
    .await
    .unwrap();
    assert_eq!(
        result_value(&ready),
        json!({"name": "teams_bind_session", "ready": true})
    );
    server
        .harness
        .protocol()
        .stdio_disconnected()
        .await
        .unwrap();
    assert!(!initialized.load(Ordering::SeqCst));
}

#[tokio::test]
async fn private_controls_are_unavailable_without_harness_capability() {
    let (_dir, server) = server(HarnessConnection::claude()).await;
    for name in PRIVATE_CONTROL_TOOLS {
        let response = handle_request_from(
            &server,
            request(name, json!({})),
            RequestOrigin::ResidentControl,
        )
        .await
        .unwrap();
        assert_eq!(
            response["error"]["message"],
            "private control is unavailable for this harness"
        );
    }
}

#[tokio::test]
async fn stdio_eof_revokes_readiness_before_the_server_returns() {
    let initialized = Arc::new(AtomicBool::new(true));
    let harness = HarnessConnection::from_adapter(
        PrivateControlFixture {
            initialized: initialized.clone(),
            calls: Arc::new(AtomicUsize::new(0)),
        },
        ProtocolOutput::new(tokio::io::sink()),
    );
    let (_dir, server) = server(harness).await;
    let (_events, receiver) = mpsc::channel(1);
    let cancel = CancellationToken::new();
    tokio::time::timeout(
        Duration::from_secs(2),
        run_with_input(
            Arc::new(server),
            receiver,
            cancel.clone(),
            StdioExitPolicy::ShutdownProcess,
            std::io::Cursor::new(Vec::<u8>::new()),
        ),
    )
    .await
    .expect("stdio EOF must finish")
    .expect("stdio server exits");
    assert!(cancel.is_cancelled());
    assert!(!initialized.load(Ordering::SeqCst));
}

#[path = "teams_omp_tests.rs"]
mod teams_omp_tests;

#[tokio::test]
async fn discord_introspection_registration_preserves_discovery_and_resident_authority() {
    let (_dir, server) = server(HarnessConnection::claude()).await;
    let server = server
        .with_module_tools(vec![fake("extension", false)])
        .unwrap();
    let response = handle_request(&server, json!({"id":1,"method":"tools/list"}))
        .await
        .unwrap();
    let definitions = response["result"]["tools"].as_array().unwrap();
    for (name, required) in [
        ("list_guilds", json!(null)),
        ("list_channels", json!(["guild_id"])),
        ("get_channel", json!(["channel_id"])),
        ("get_user", json!(["user_id"])),
        ("get_member", json!(["guild_id", "user_id"])),
        ("list_roles", json!(["guild_id"])),
        ("list_emojis", json!(["guild_id"])),
        ("can_view_channel", json!(["channel_id", "user_ids"])),
    ] {
        let found: Vec<_> = definitions
            .iter()
            .filter(|definition| definition["name"] == name)
            .collect();
        assert_eq!(found.len(), 1, "{name} has exactly one definition");
        assert_eq!(found[0]["inputSchema"]["type"], "object");
        assert_eq!(found[0]["inputSchema"]["required"], required);
        assert!(
            server.module_tools.find(name).is_some(),
            "{name} is registered"
        );
        assert!(resident_request_allowed(
            &server.module_tools,
            &request(name, json!({}))
        ));
        assert!(!resident_request_allowed(
            &ToolRegistry::default(),
            &request(name, json!({}))
        ));
        let error = server
            .clone()
            .with_module_tools(vec![fake(name, false)])
            .err()
            .unwrap();
        assert_eq!(error.name.to_string(), name);
        assert!(matches!(error.kind, RegistrationErrorKind::Duplicate));
    }
    assert!(server.module_tools.find("extension").is_some());
    for name in ["send_dm", "get_presence", "approve_access"] {
        assert!(!resident_request_allowed(
            &server.module_tools,
            &request(name, json!({}))
        ));
    }
    assert!(!resident_request_allowed(
        &server.module_tools,
        &json!({"method":"tools/list"})
    ));
}

#[tokio::test]
async fn discord_introspection_resident_call_uses_registered_owner_without_discovery_access() {
    let (ctx, requests) = crate::discord::introspection_wire::tests::fixture(vec![(
        "200 OK",
        json!({"id":"91","username":"observed","discriminator":"0","bot":false}),
    )])
    .await;
    let (_dir, server) = server_with_http(HarnessConnection::claude(), ctx.http).await;
    let (client, connection) = UnixStream::pair().unwrap();
    let task = tokio::spawn(serve_resident_connection(
        Arc::new(server),
        connection,
        CancellationToken::new(),
    ));
    let mut reader = BufReader::new(client);
    for (request, expected) in [
        (
            request("get_user", json!({"user_id":"bad"})),
            json!({"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"invalid user_id: not a valid u64"}}),
        ),
        (
            request("get_user", json!({"user_id":"80"})),
            json!({"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"{\"bot\":false,\"discriminator\":null,\"id\":\"91\",\"name\":\"observed\"}"}]}}),
        ),
    ] {
        reader
            .get_mut()
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        assert_eq!(serde_json::from_str::<Value>(&line).unwrap(), expected);
    }
    reader
        .get_mut()
        .write_all(b"{\"id\":2,\"method\":\"tools/list\"}\n")
        .await
        .unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let denied: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(denied["error"]["code"], -32601);
    assert!(denied.get("result").is_none());
    drop(reader);
    task.await.unwrap().unwrap();
    assert_eq!(requests.await.unwrap(), ["GET /api/v10/users/80 HTTP/1.1"]);
}

#[tokio::test]
async fn discord_query_registration_preserves_discovery_and_resident_authority() {
    let (_dir, server) = server(HarnessConnection::claude()).await;
    let server = server
        .with_module_tools(vec![fake("extension", false)])
        .unwrap();
    let response = handle_request(&server, json!({"id":1,"method":"tools/list"}))
        .await
        .unwrap();
    let definitions = response["result"]["tools"].as_array().unwrap();
    for (name, required) in [
        ("fetch_messages", json!(["channel_id"])),
        ("fetch_new_since", json!(["channel_id", "after_message_id"])),
        ("fetch_pins", json!(["channel_id"])),
        ("download_attachment", json!(["channel_id", "message_id"])),
        ("get_message", json!(["channel_id", "message_id"])),
        ("search_messages", json!(["guild_id"])),
    ] {
        let found: Vec<_> = definitions
            .iter()
            .filter(|definition| definition["name"] == name)
            .collect();
        assert_eq!(found.len(), 1, "{name} has exactly one definition");
        assert_eq!(found[0]["inputSchema"]["type"], "object");
        assert_eq!(found[0]["inputSchema"]["required"], required);
        assert!(
            server.module_tools.find(name).is_some(),
            "{name} is registered"
        );
        assert!(resident_request_allowed(
            &server.module_tools,
            &request(name, json!({}))
        ));
        assert!(!resident_request_allowed(
            &ToolRegistry::default(),
            &request(name, json!({}))
        ));
        let error = server
            .clone()
            .with_module_tools(vec![fake(name, false)])
            .err()
            .unwrap();
        assert_eq!(error.name.to_string(), name);
        assert!(matches!(error.kind, RegistrationErrorKind::Duplicate));
    }
    assert!(server.module_tools.find("extension").is_some());
    for name in ["send_dm", "get_presence", "approve_access"] {
        assert!(!resident_request_allowed(
            &server.module_tools,
            &request(name, json!({}))
        ));
    }
    assert!(!resident_request_allowed(
        &server.module_tools,
        &json!({"method":"tools/list"})
    ));
}

#[tokio::test]
async fn discord_management_registration_preserves_discovery_and_resident_authority() {
    let (_dir, server) = server(HarnessConnection::claude()).await;
    let server = server
        .with_module_tools(vec![fake("extension", false)])
        .unwrap();
    let response = handle_request(&server, json!({"id":1,"method":"tools/list"}))
        .await
        .unwrap();
    let definitions = response["result"]["tools"].as_array().unwrap();
    for (name, required) in [
        ("pin_message", json!(["channel_id", "message_id"])),
        ("unpin_message", json!(["channel_id", "message_id"])),
        ("delete_message", json!(["channel_id", "message_id"])),
        ("create_thread", json!(["channel_id", "name"])),
    ] {
        let found: Vec<_> = definitions
            .iter()
            .filter(|definition| definition["name"] == name)
            .collect();
        assert_eq!(found.len(), 1, "{name} has exactly one definition");
        assert_eq!(found[0]["inputSchema"]["type"], "object");
        assert_eq!(found[0]["inputSchema"]["required"], required);
        assert!(
            server.module_tools.find(name).is_some(),
            "{name} is registered"
        );
        assert!(resident_request_allowed(
            &server.module_tools,
            &request(name, json!({}))
        ));
        assert!(!resident_request_allowed(
            &ToolRegistry::default(),
            &request(name, json!({}))
        ));
        let error = server
            .clone()
            .with_module_tools(vec![fake(name, false)])
            .err()
            .unwrap();
        assert_eq!(error.name.to_string(), name);
        assert!(matches!(error.kind, RegistrationErrorKind::Duplicate));
    }
    assert!(server.module_tools.find("extension").is_some());
    for name in ["send_dm", "get_presence", "approve_access"] {
        assert!(!resident_request_allowed(
            &server.module_tools,
            &request(name, json!({}))
        ));
    }
    assert!(!resident_request_allowed(
        &server.module_tools,
        &json!({"method":"tools/list"})
    ));
}
