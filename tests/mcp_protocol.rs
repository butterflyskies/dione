//! Integration tests for the MCP JSON-RPC protocol layer.
//!
//! These tests exercise `handle_request` (via `test_helpers::dispatch_request`)
//! and `event_to_notification` (via `test_helpers::make_notification`) without
//! a real Discord connection.  Tool calls that require Discord HTTP are tested
//! only up to the gate-rejection path.

use dione::{
    codex::TransportMode,
    discord::events::{AttachmentMeta, MessageEvent, NotificationEvent},
    mcp::server::{DioneServer, test_helpers},
    no_rly::consent::ConsentGate,
    queue::AccessQueue,
    state::new_state,
    timestamp::Timestamp,
    tracing_channel::TraceLevelController,
};
use serde_json::json;
use serenity::model::id::{ChannelId, MessageId, UserId};
use std::sync::Arc;
use tempfile::TempDir;
use tokio::sync::{Mutex, mpsc};

// ── Test fixture ──────────────────────────────────────────────────────────────

fn temp_state_dir() -> (TempDir, camino::Utf8PathBuf) {
    let dir = TempDir::new().unwrap();
    let path = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    (dir, path)
}

/// Serializes tests that write the process-global config cache.
///
/// `store_loaded_config` swaps a process-wide `ArcSwap` (`LAST_VALID_CONFIG`).
/// Under `cargo test` all tests share one process, so two tests storing
/// different configs in parallel clobber each other mid-flight (~1/8 flake
/// rate on the suppress_ping gate tests). nextest masks this by running each
/// test in its own process.
///
/// Any test that needs a non-default global config must go through
/// [`set_global_config`], which takes this lock for the test's duration and
/// restores the default config on drop so later tests see a clean slate.
static CONFIG_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// RAII guard from [`set_global_config`]: holds [`CONFIG_LOCK`] and restores
/// the default global config when dropped.
struct GlobalConfigGuard(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

impl Drop for GlobalConfigGuard {
    fn drop(&mut self) {
        // Runs while the lock is still held (fields drop after the drop body).
        dione::config::store_loaded_config(&dione::config::LoadedConfig::from_raw(
            dione::config::Config::default(),
        ));
    }
}

/// Installs `config` as the process-global config for the lifetime of the
/// returned guard. See [`CONFIG_LOCK`] for why this must be serialized.
fn set_global_config(config: dione::config::Config) -> GlobalConfigGuard {
    let guard = CONFIG_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    dione::config::store_loaded_config(&dione::config::LoadedConfig::from_raw(config));
    GlobalConfigGuard(guard)
}

fn make_server(state_dir: &camino::Utf8PathBuf) -> DioneServer {
    let http = Arc::new(serenity::http::Http::new("fake-token-for-tests"));
    let state = new_state();
    let queue = Arc::new(Mutex::new(AccessQueue::load(state_dir)));
    let (tx, _rx) = mpsc::channel(4);
    DioneServer {
        state,
        queue,
        http,
        state_dir: state_dir.clone(),
        notification_tx: tx,
        discord_cmd_tx: None,
        trace_controller: TraceLevelController::noop(),
        mode: TransportMode::ClaudeCode,
        codex_queue: None,
        codex_thread_binding: None,
        no_rly: Arc::new(ConsentGate::new(state_dir)),
    }
}

// ── Initialize handshake ──────────────────────────────────────────────────────

#[tokio::test]
async fn test_initialize_returns_capabilities() {
    let resp = test_helpers::get_initialize_response();
    // Must advertise the tools capability.
    assert!(
        resp.get("capabilities")
            .and_then(|c| c.get("tools"))
            .is_some(),
        "initialize response must include capabilities.tools"
    );
    // Must advertise experimental channel capabilities.
    let experimental = resp.get("capabilities").and_then(|c| c.get("experimental"));
    assert!(
        experimental.is_some(),
        "must include capabilities.experimental"
    );
    let experimental = experimental.unwrap();
    assert!(
        experimental.get("claude/channel").is_some(),
        "must declare claude/channel experimental capability"
    );
    assert!(
        experimental.get("claude/channel/permission").is_some(),
        "must declare claude/channel/permission experimental capability"
    );
    // Must include server info.
    assert!(
        resp.get("serverInfo").and_then(|s| s.get("name")).is_some(),
        "initialize response must include serverInfo.name"
    );
    // Protocol version must be present.
    assert!(
        resp.get("protocolVersion").is_some(),
        "initialize response must include protocolVersion"
    );
}

#[test]
fn test_codex_initialize_omits_claude_experimental_capabilities() {
    let response = test_helpers::get_codex_initialize_response();
    assert!(response["capabilities"].get("tools").is_some());
    assert!(response["capabilities"].get("experimental").is_none());
}

#[tokio::test]
async fn test_initialize_request_dispatch() {
    let (_dir, state_dir) = temp_state_dir();
    let server = make_server(&state_dir);

    let req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": { "protocolVersion": "2024-11-05" }
    });
    let resp = test_helpers::dispatch_request(&server, req).await.unwrap();

    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["id"], 1);
    // Result should have capabilities.
    assert!(
        resp["result"]["capabilities"]["tools"].is_object(),
        "dispatch initialize must return capabilities.tools"
    );
}

// ── tools/list ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_tools_list_contains_expected_tools() {
    let list = test_helpers::get_tools_list();
    let tools = list["tools"].as_array().expect("tools must be an array");

    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();

    // Core messaging tools.
    assert!(names.contains(&"reply"), "tools/list must include reply");
    assert!(names.contains(&"react"), "tools/list must include react");
    assert!(
        names.contains(&"edit_message"),
        "tools/list must include edit_message"
    );
    assert!(
        names.contains(&"fetch_messages"),
        "tools/list must include fetch_messages"
    );
    assert!(
        names.contains(&"fetch_new_since"),
        "tools/list must include fetch_new_since"
    );

    // Introspection tools.
    assert!(
        names.contains(&"list_guilds"),
        "tools/list must include list_guilds"
    );
    assert!(
        names.contains(&"list_channels"),
        "tools/list must include list_channels"
    );

    // Access management tools.
    assert!(
        names.contains(&"list_access_requests"),
        "tools/list must include list_access_requests"
    );
    assert!(
        names.contains(&"approve_access"),
        "tools/list must include approve_access"
    );
    assert!(
        names.contains(&"deny_access"),
        "tools/list must include deny_access"
    );

    // Bot state tools.
    assert!(
        names.contains(&"send_typing"),
        "tools/list must include send_typing"
    );

    // Rendering tools.
    assert!(
        names.contains(&"render_latex"),
        "tools/list must include render_latex"
    );
    assert!(
        names.contains(&"render_latex_to_channel"),
        "tools/list must include render_latex_to_channel"
    );

    // File tools.
    assert!(
        names.contains(&"send_file"),
        "tools/list must include send_file"
    );

    // DM tools.
    assert!(
        names.contains(&"send_dm"),
        "tools/list must include send_dm"
    );

    // no_rly consent-gate tools.
    assert!(names.contains(&"no_rly"), "tools/list must include no_rly");
    assert!(
        names.contains(&"rephrase"),
        "tools/list must include rephrase"
    );
    assert!(
        names.contains(&"no_rly_stats"),
        "tools/list must include no_rly_stats"
    );
    assert!(
        names.contains(&"no_rly_condense"),
        "tools/list must include no_rly_condense"
    );
    assert!(
        names.contains(&"no_rly_vacuum"),
        "tools/list must include no_rly_vacuum"
    );
}

/// Wire contract: every tool schema must declare `type: "object"` — a strict
/// MCP client (Claude Code) rejects the entire tool list otherwise.
#[test]
fn test_every_tool_schema_is_object_typed() {
    let list = test_helpers::get_tools_list();
    let tools = list["tools"].as_array().expect("tools must be an array");
    for tool in tools {
        let name = tool["name"].as_str().unwrap_or("<unnamed>");
        assert_eq!(
            tool["inputSchema"]["type"], "object",
            "tool {name} must have an object input schema"
        );
    }
}

/// The `no_rly` arg is gone from reply — the handle queue replaced the
/// resend-with-a-flag override, and a leftover schema property would invite
/// pre-emptive overrides the design exists to kill.
#[test]
fn test_reply_schema_has_no_no_rly_arg() {
    let list = test_helpers::get_tools_list();
    let tools = list["tools"].as_array().expect("tools must be an array");
    let reply_tool = tools
        .iter()
        .find(|t| t["name"] == "reply")
        .expect("reply tool must exist");
    assert!(
        reply_tool["inputSchema"]["properties"]
            .get("no_rly")
            .is_none(),
        "reply must not accept a no_rly argument"
    );
}

#[tokio::test]
async fn test_tools_list_dispatch() {
    let (_dir, state_dir) = temp_state_dir();
    let server = make_server(&state_dir);

    let req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    });
    let resp = test_helpers::dispatch_request(&server, req).await.unwrap();

    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["id"], 2);
    assert!(
        resp["result"]["tools"].is_array(),
        "tools/list result must have tools array"
    );
}

// ── Invalid method ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_unknown_method_returns_error() {
    let (_dir, state_dir) = temp_state_dir();
    let server = make_server(&state_dir);

    let req = json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "no_such_method/v99",
        "params": {}
    });
    let resp = test_helpers::dispatch_request(&server, req).await.unwrap();

    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["id"], 3);
    // Must have an error field, not a result field.
    assert!(
        resp.get("error").is_some(),
        "unknown method must return an error response"
    );
    assert!(
        resp.get("result").is_none(),
        "unknown method must not return a result"
    );
}

// ── Client notification (no id) ───────────────────────────────────────────────

#[tokio::test]
async fn test_client_notification_no_response() {
    let (_dir, state_dir) = temp_state_dir();
    let server = make_server(&state_dir);

    // A JSON-RPC notification has no "id" field.
    let notif = json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized",
        "params": {}
    });
    let resp = test_helpers::dispatch_request(&server, notif).await;
    // Server must return None (no response to a notification).
    assert!(
        resp.is_none(),
        "server must not respond to client notifications"
    );
}

// ── tools/call — gate rejection path ─────────────────────────────────────────

#[tokio::test]
async fn test_tools_call_send_typing_rejected_unknown_channel() {
    let (_dir, state_dir) = temp_state_dir();
    // Empty config → no opted-in channels, no DM map → gate will reject.
    let server = make_server(&state_dir);

    let req = json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/call",
        "params": {
            "name": "send_typing",
            "arguments": { "channel_id": "999999" }
        }
    });
    let resp = test_helpers::dispatch_request(&server, req).await.unwrap();

    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["id"], 4);
    // The tool result must be present and flagged as an error (channel not in allowlist).
    let result = &resp["result"];
    assert_eq!(
        result["isError"],
        json!(true),
        "send_typing to unknown channel should return isError: true"
    );
}

#[tokio::test]
async fn test_tools_call_fetch_new_since_rejected_unknown_channel() {
    let (_dir, state_dir) = temp_state_dir();
    // Empty config → no opted-in channels, no DM map → gate will reject.
    let server = make_server(&state_dir);

    let req = json!({
        "jsonrpc": "2.0",
        "id": 33,
        "method": "tools/call",
        "params": {
            "name": "fetch_new_since",
            "arguments": { "channel_id": "999999", "after_message_id": "123456" }
        }
    });
    let resp = test_helpers::dispatch_request(&server, req).await.unwrap();

    assert_eq!(resp["id"], 33);
    assert_eq!(
        resp["result"]["isError"],
        json!(true),
        "fetch_new_since on an unknown channel should return isError: true"
    );
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("not a permitted outbound target"),
        "expected gate rejection, got: {text}"
    );
}

#[tokio::test]
async fn test_tools_call_fetch_new_since_zero_cursor_returns_error() {
    let (_dir, state_dir) = temp_state_dir();
    let server = make_server(&state_dir);

    // after_message_id = 0 must be rejected at the MCP boundary: serenity's
    // `MessageId::new` wraps a `NonZeroU64` and panics on zero.
    let req = json!({
        "jsonrpc": "2.0",
        "id": 34,
        "method": "tools/call",
        "params": {
            "name": "fetch_new_since",
            "arguments": { "channel_id": "999999", "after_message_id": "0" }
        }
    });
    let resp = test_helpers::dispatch_request(&server, req).await.unwrap();

    assert_eq!(resp["id"], 34);
    let err = resp
        .get("error")
        .expect("zero after_message_id should produce a JSON-RPC error, not a panic");
    let msg = err["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("after_message_id"),
        "error should name the offending parameter, got: {msg}"
    );
}

#[tokio::test]
async fn test_tools_call_zero_snowflake_returns_error_across_tools() {
    let (_dir, state_dir) = temp_state_dir();
    let server = make_server(&state_dir);

    // Every tool that feeds a parsed ID into serenity's NonZeroU64-backed Id
    // wrappers must reject zero at the MCP boundary instead of panicking.
    let cases: Vec<(&str, serde_json::Value)> = vec![
        (
            "get_message",
            json!({ "channel_id": "999999", "message_id": "0" }),
        ),
        (
            "react",
            json!({ "channel_id": "999999", "message_id": "0", "emoji": "x" }),
        ),
        (
            "pin_message",
            json!({ "channel_id": "999999", "message_id": "0" }),
        ),
        (
            "unpin_message",
            json!({ "channel_id": "999999", "message_id": "0" }),
        ),
        (
            "delete_message",
            json!({ "channel_id": "999999", "message_id": "0" }),
        ),
        (
            "edit_message",
            json!({ "channel_id": "999999", "message_id": "0", "content": "x" }),
        ),
        (
            "download_attachment",
            json!({ "channel_id": "999999", "message_id": "0" }),
        ),
        ("fetch_messages", json!({ "channel_id": "0" })),
        (
            "fetch_new_since",
            json!({ "channel_id": "0", "after_message_id": "123456" }),
        ),
        ("get_channel", json!({ "channel_id": "0" })),
        ("get_user", json!({ "user_id": "0" })),
        ("send_typing", json!({ "channel_id": "0" })),
        ("send_dm", json!({ "user_id": "0", "content": "x" })),
        ("reply", json!({ "channel_id": "0", "content": "x" })),
    ];

    for (i, (tool, arguments)) in cases.into_iter().enumerate() {
        let req = json!({
            "jsonrpc": "2.0",
            "id": 200 + i,
            "method": "tools/call",
            "params": { "name": tool, "arguments": arguments }
        });
        let resp = test_helpers::dispatch_request(&server, req).await.unwrap();
        let err = resp.get("error").unwrap_or_else(|| {
            panic!("{tool} with a zero snowflake should produce a JSON-RPC error, got: {resp}")
        });
        let msg = err["message"].as_str().unwrap_or_default();
        assert!(
            msg.contains("nonzero Discord snowflake"),
            "{tool}: error should explain the nonzero requirement, got: {msg}"
        );
    }
}

#[tokio::test]
async fn test_tools_call_send_file_rejected_unknown_channel() {
    let (_dir, state_dir) = temp_state_dir();
    let server = make_server(&state_dir);

    let req = json!({
        "jsonrpc": "2.0",
        "id": 30,
        "method": "tools/call",
        "params": {
            "name": "send_file",
            "arguments": { "channel_id": "999999", "file_path": "/tmp/test.png" }
        }
    });
    let resp = test_helpers::dispatch_request(&server, req).await.unwrap();
    assert_eq!(resp["result"]["isError"], json!(true));
}

#[tokio::test]
async fn test_tools_call_send_file_rejects_relative_path() {
    let (_dir, state_dir) = temp_state_dir();
    let server = make_server(&state_dir);

    let req = json!({
        "jsonrpc": "2.0",
        "id": 31,
        "method": "tools/call",
        "params": {
            "name": "send_file",
            "arguments": { "channel_id": "999999", "file_path": "relative/path.png" }
        }
    });
    let resp = test_helpers::dispatch_request(&server, req).await.unwrap();
    assert_eq!(resp["result"]["isError"], json!(true));
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("file_path must be absolute"),
        "expected path rejection, got: {text}"
    );
}

#[tokio::test]
async fn test_tools_call_send_file_rejects_captionless_hook_override() {
    let (_dir, state_dir) = temp_state_dir();
    let server = make_server(&state_dir);
    let req = json!({
        "jsonrpc": "2.0",
        "id": 311,
        "method": "tools/call",
        "params": {
            "name": "send_file",
            "arguments": {
                "channel_id": "999999",
                "file_path": "/does/not/exist",
                "no_rly_hooks": ["tier-1"]
            }
        }
    });

    let resp = test_helpers::dispatch_request(&server, req).await.unwrap();
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("no_rly_hooks cannot be used when no caption is sent"));
}

#[tokio::test]
async fn test_tools_call_render_latex_to_channel_rejected_unknown_channel() {
    let (_dir, state_dir) = temp_state_dir();
    let server = make_server(&state_dir);

    let req = json!({
        "jsonrpc": "2.0",
        "id": 32,
        "method": "tools/call",
        "params": {
            "name": "render_latex_to_channel",
            "arguments": { "channel_id": "999999", "latex": "x^2" }
        }
    });
    let resp = test_helpers::dispatch_request(&server, req).await.unwrap();
    assert_eq!(resp["result"]["isError"], json!(true));
}

#[tokio::test]
async fn test_tools_call_render_rejects_captionless_hook_override() {
    let (_dir, state_dir) = temp_state_dir();
    let server = make_server(&state_dir);
    let req = json!({
        "jsonrpc": "2.0",
        "id": 321,
        "method": "tools/call",
        "params": {
            "name": "render_latex_to_channel",
            "arguments": {
                "channel_id": "999999",
                "latex": "deliberately invalid",
                "no_rly_hooks": ["tier-1"]
            }
        }
    });

    let resp = test_helpers::dispatch_request(&server, req).await.unwrap();
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("no_rly_hooks cannot be used when no caption is sent"));
}

#[tokio::test]
async fn test_tools_call_send_dm_disabled_returns_error() {
    let (_dir, state_dir) = temp_state_dir();
    let mut config = dione::config::Config::default();
    config.access.dm_policy = dione::config::DmPolicy::Disabled;
    let _config = set_global_config(config);
    let server = make_server(&state_dir);

    let req = json!({
        "jsonrpc": "2.0",
        "id": 40,
        "method": "tools/call",
        "params": {
            "name": "send_dm",
            "arguments": { "user_id": "123456789", "content": "hello" }
        }
    });
    let resp = test_helpers::dispatch_request(&server, req).await.unwrap();
    assert_eq!(resp["id"], 40);
    assert_eq!(
        resp["result"]["isError"],
        json!(true),
        "send_dm with dm_policy=disabled should return isError: true"
    );
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("disabled"),
        "error message should mention 'disabled', got: {text}"
    );
}

#[tokio::test]
async fn test_tools_call_send_dm_missing_user_id_returns_error() {
    let (_dir, state_dir) = temp_state_dir();
    let server = make_server(&state_dir);

    let req = json!({
        "jsonrpc": "2.0",
        "id": 41,
        "method": "tools/call",
        "params": {
            "name": "send_dm",
            "arguments": { "content": "hello" }
        }
    });
    let resp = test_helpers::dispatch_request(&server, req).await.unwrap();
    assert_eq!(resp["id"], 41);
    assert!(
        resp.get("error").is_some(),
        "send_dm without user_id should produce a JSON-RPC error"
    );
}

#[tokio::test]
async fn test_tools_call_unknown_tool_returns_error() {
    let (_dir, state_dir) = temp_state_dir();
    let server = make_server(&state_dir);

    let req = json!({
        "jsonrpc": "2.0",
        "id": 5,
        "method": "tools/call",
        "params": {
            "name": "nonexistent_tool",
            "arguments": {}
        }
    });
    let resp = test_helpers::dispatch_request(&server, req).await.unwrap();

    // The RPC layer should return a JSON-RPC error (not a tool result with isError).
    assert!(
        resp.get("error").is_some(),
        "unknown tool should produce a JSON-RPC error"
    );
}

#[tokio::test]
async fn test_tools_call_missing_tool_name_returns_error() {
    let (_dir, state_dir) = temp_state_dir();
    let server = make_server(&state_dir);

    let req = json!({
        "jsonrpc": "2.0",
        "id": 6,
        "method": "tools/call",
        "params": {
            "arguments": { "channel_id": "123" }
        }
    });
    let resp = test_helpers::dispatch_request(&server, req).await.unwrap();
    assert!(
        resp.get("error").is_some(),
        "tools/call without name must return error"
    );
}

// ── list_access_requests tool — empty queue ───────────────────────────────────

#[tokio::test]
async fn test_tools_call_list_access_requests_empty() {
    let (_dir, state_dir) = temp_state_dir();
    let server = make_server(&state_dir);

    let req = json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "tools/call",
        "params": {
            "name": "list_access_requests",
            "arguments": {}
        }
    });
    let resp = test_helpers::dispatch_request(&server, req).await.unwrap();
    assert_eq!(resp["id"], 7);
    // Should be a valid result, not an error.
    assert!(
        resp.get("result").is_some(),
        "list_access_requests should succeed"
    );
    assert!(
        resp.get("error").is_none(),
        "list_access_requests should not error"
    );
}

#[tokio::test]
async fn test_bind_codex_thread_updates_live_binding() {
    let (_dir, state_dir) = temp_state_dir();
    let mut server = make_server(&state_dir);
    let (binding_tx, mut binding_rx) = tokio::sync::watch::channel(None);
    server.mode = TransportMode::Codex;
    server.codex_queue = Some(dione::codex::CodexEventQueue::load(&state_dir).unwrap());
    server.codex_thread_binding = Some(binding_tx);
    let thread_id = "019f4b14-ccc7-7db2-80c8-fe2b888c8844";
    let request = json!({
        "jsonrpc": "2.0",
        "id": 71,
        "method": "tools/call",
        "params": {
            "name": "bind_codex_thread",
            "arguments": { "thread_id": thread_id }
        }
    });

    let response = test_helpers::dispatch_request(&server, request.clone())
        .await
        .unwrap();

    assert!(response.get("error").is_none());
    assert_eq!(
        binding_rx
            .borrow_and_update()
            .as_ref()
            .map(dione::codex::CodexThreadId::as_str),
        Some(thread_id)
    );

    let response = test_helpers::dispatch_request(&server, request)
        .await
        .unwrap();

    assert!(response.get("error").is_none());
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), binding_rx.changed())
            .await
            .is_err(),
        "binding the current thread again must not notify the delivery worker"
    );
}

// ── Notification format tests ─────────────────────────────────────────────────

// Semantic property tests — wire format is pinned by snapshots below.

#[test]
fn test_notification_has_no_id_field() {
    let event = NotificationEvent::Message(MessageEvent {
        chat_id: ChannelId::new(1),
        message_id: MessageId::new(2),
        user: "x".to_string(),
        user_id: UserId::new(3),
        content: "hi".to_string(),
        targeting: dione::discord::events::MessageTargeting::Ambient,
        timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
        attachments: vec![],
        is_voice_message: false,
        thread_parent_id: None,
        reply_to_message_id: None,
        reply_to_user_id: None,
        reply_to_user: None,
        reply_to_content_preview: None,
        bells: None,
        bells_status: None,
    });
    let notif = test_helpers::make_notification(event);
    assert!(
        notif.get("id").is_none(),
        "notifications must not have an id field"
    );
}

#[test]
fn test_notification_attachment_metadata_present() {
    let event = NotificationEvent::Message(MessageEvent {
        chat_id: ChannelId::new(1),
        message_id: MessageId::new(2),
        user: "x".to_string(),
        user_id: UserId::new(3),
        content: "see file".to_string(),
        targeting: dione::discord::events::MessageTargeting::Ambient,
        timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
        attachments: vec![AttachmentMeta {
            name: "photo.png".to_string(),
            content_type: Some("image/png".to_string()),
            size: 2048,
        }],
        is_voice_message: false,
        thread_parent_id: None,
        reply_to_message_id: None,
        reply_to_user_id: None,
        reply_to_user: None,
        reply_to_content_preview: None,
        bells: None,
        bells_status: None,
    });
    let notif = test_helpers::make_notification(event);
    let meta = &notif["params"]["meta"];
    assert_eq!(meta["attachment_count"], "1");
    assert!(meta["attachments"].as_str().unwrap().contains("photo.png"));
}

#[test]
fn test_notification_voice_flag_in_meta() {
    let event = NotificationEvent::Message(MessageEvent {
        chat_id: ChannelId::new(1),
        message_id: MessageId::new(2),
        user: "x".to_string(),
        user_id: UserId::new(3),
        content: String::new(),
        targeting: dione::discord::events::MessageTargeting::Ambient,
        timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
        attachments: vec![],
        is_voice_message: true,
        thread_parent_id: None,
        reply_to_message_id: None,
        reply_to_user_id: None,
        reply_to_user: None,
        reply_to_content_preview: None,
        bells: None,
        bells_status: None,
    });
    let notif = test_helpers::make_notification(event);
    assert_eq!(notif["params"]["meta"]["is_voice_message"], true);
}

#[test]
fn test_permission_deny_uses_deny_behavior() {
    let event = NotificationEvent::PermissionResponse {
        request_id: "req-xyz".to_string(),
        granted: false,
    };
    let notif = test_helpers::make_notification(event);
    assert_eq!(notif["params"]["behavior"], "deny");
    assert_eq!(notif["method"], "notifications/claude/channel/permission");
}

// ── Snapshot tests ────────────────────────────────────────────────────────────

#[test]
fn test_notification_message_snapshot() {
    let event = NotificationEvent::Message(MessageEvent {
        chat_id: ChannelId::new(1000),
        message_id: MessageId::new(2000),
        user: "snapuser".to_string(),
        user_id: UserId::new(3000),
        content: "snapshot content".to_string(),
        targeting: dione::discord::events::MessageTargeting::Ambient,
        timestamp: Timestamp::parse("2026-01-01T00:00:00+00:00").unwrap(),
        attachments: vec![],
        is_voice_message: false,
        thread_parent_id: None,
        reply_to_message_id: None,
        reply_to_user_id: None,
        reply_to_user: None,
        reply_to_content_preview: None,
        bells: None,
        bells_status: None,
    });
    let notif = test_helpers::make_notification(event);
    insta::assert_json_snapshot!(notif);
}

#[test]
fn test_notification_reaction_snapshot() {
    let event = NotificationEvent::Reaction {
        chat_id: ChannelId::new(1001),
        message_id: MessageId::new(2001),
        user: "reactor".to_string(),
        user_id: UserId::new(3001),
        emoji: "🎉".to_string(),
    };
    let notif = test_helpers::make_notification(event);
    insta::assert_json_snapshot!(notif);
}

#[test]
fn test_notification_permission_response_snapshot() {
    let event = NotificationEvent::PermissionResponse {
        request_id: "snap-req-42".to_string(),
        granted: true,
    };
    let notif = test_helpers::make_notification(event);
    insta::assert_json_snapshot!(notif);
}

#[test]
fn test_notification_message_edit_snapshot() {
    let event = NotificationEvent::MessageEdit {
        chat_id: ChannelId::new(1002),
        message_id: MessageId::new(2002),
        user: "editor".to_string(),
        user_id: UserId::new(3002),
        new_content: "fixed a typo".to_string(),
        timestamp: Timestamp::parse("2026-01-01T00:01:00+00:00").unwrap(),
        thread_parent_id: None,
        reply_to_message_id: None,
    };
    let notif = test_helpers::make_notification(event);
    insta::assert_json_snapshot!(notif);
}

#[test]
fn test_notification_message_edit_reply_snapshot() {
    let event = NotificationEvent::MessageEdit {
        chat_id: ChannelId::new(1002),
        message_id: MessageId::new(2002),
        user: "editor".to_string(),
        user_id: UserId::new(3002),
        new_content: "fixed a typo".to_string(),
        timestamp: Timestamp::parse("2026-01-01T00:01:00+00:00").unwrap(),
        thread_parent_id: None,
        reply_to_message_id: Some(MessageId::new(8888)),
    };
    let notif = test_helpers::make_notification(event);
    insta::assert_json_snapshot!(notif);
}

#[test]
fn test_notification_message_delete_snapshot() {
    let event = NotificationEvent::MessageDelete {
        chat_id: ChannelId::new(1003),
        message_id: MessageId::new(2003),
        thread_parent_id: None,
    };
    let notif = test_helpers::make_notification(event);
    insta::assert_json_snapshot!(notif);
}

// ── Snapshot: message with thread_parent_id present ─────────────────────────

#[test]
fn test_notification_message_in_thread_snapshot() {
    let event = NotificationEvent::Message(MessageEvent {
        chat_id: ChannelId::new(1000),
        message_id: MessageId::new(2000),
        user: "threaduser".to_string(),
        user_id: UserId::new(3000),
        content: "reply in thread".to_string(),
        targeting: dione::discord::events::MessageTargeting::Ambient,
        timestamp: Timestamp::parse("2026-01-01T00:00:00+00:00").unwrap(),
        attachments: vec![],
        is_voice_message: false,
        thread_parent_id: Some(ChannelId::new(700)),
        reply_to_message_id: None,
        reply_to_user_id: None,
        reply_to_user: None,
        reply_to_content_preview: None,
        bells: None,
        bells_status: None,
    });
    let notif = test_helpers::make_notification(event);
    insta::assert_json_snapshot!(notif);
}

// ── Snapshot: message with reply_to_message_id present ────────────────────────

#[test]
fn test_notification_message_reply_snapshot() {
    let event = NotificationEvent::Message(MessageEvent {
        chat_id: ChannelId::new(1000),
        message_id: MessageId::new(2000),
        user: "replyuser".to_string(),
        user_id: UserId::new(3000),
        content: "replying to someone".to_string(),
        targeting: dione::discord::events::MessageTargeting::Ambient,
        timestamp: Timestamp::parse("2026-01-01T00:00:00+00:00").unwrap(),
        attachments: vec![],
        is_voice_message: false,
        thread_parent_id: None,
        reply_to_message_id: Some(MessageId::new(9999)),
        reply_to_user_id: Some(UserId::new(4444)),
        reply_to_user: Some("parentuser".to_string()),
        reply_to_content_preview: Some("the original message".to_string()),
        bells: None,
        bells_status: None,
    });
    let notif = test_helpers::make_notification(event);
    insta::assert_json_snapshot!(notif);
}

#[test]
fn test_notification_message_reply_in_thread_snapshot() {
    let event = NotificationEvent::Message(MessageEvent {
        chat_id: ChannelId::new(1000),
        message_id: MessageId::new(2000),
        user: "threaduser".to_string(),
        user_id: UserId::new(3000),
        content: "reply in thread".to_string(),
        targeting: dione::discord::events::MessageTargeting::Ambient,
        timestamp: Timestamp::parse("2026-01-01T00:00:00+00:00").unwrap(),
        attachments: vec![],
        is_voice_message: false,
        thread_parent_id: Some(ChannelId::new(700)),
        reply_to_message_id: Some(MessageId::new(5555)),
        reply_to_user_id: None,
        reply_to_user: None,
        reply_to_content_preview: None,
        bells: None,
        bells_status: None,
    });
    let notif = test_helpers::make_notification(event);
    insta::assert_json_snapshot!(notif);
}

// ── Trace notification tests ─────────────────────────────────────────────────

#[test]
fn test_trace_notification_format() {
    let event = NotificationEvent::Trace {
        level: "DEBUG".to_string(),
        target: "dione::discord::events".to_string(),
        message: "reaction_add fired".to_string(),
        fields: vec![
            ("message_id".to_string(), "12345".to_string()),
            ("cached".to_string(), "None".to_string()),
        ],
    };
    let notif = test_helpers::make_notification(event);
    assert_eq!(notif["method"], "notifications/claude/channel");
    assert_eq!(notif["params"]["meta"]["type"], "trace");
    assert_eq!(notif["params"]["meta"]["level"], "DEBUG");
    assert_eq!(notif["params"]["meta"]["target"], "dione::discord::events");
    let content = notif["params"]["content"].as_str().unwrap();
    assert!(content.contains("reaction_add fired"));
    assert!(content.contains("message_id=12345"));
    assert!(content.contains("cached=None"));
}

#[test]
fn test_trace_notification_no_fields() {
    let event = NotificationEvent::Trace {
        level: "INFO".to_string(),
        target: "dione".to_string(),
        message: "dione starting".to_string(),
        fields: vec![],
    };
    let notif = test_helpers::make_notification(event);
    assert_eq!(notif["params"]["content"], "dione starting");
    assert_eq!(notif["params"]["meta"]["type"], "trace");
}

#[test]
fn test_trace_notification_snapshot() {
    let event = NotificationEvent::Trace {
        level: "WARN".to_string(),
        target: "dione::mcp".to_string(),
        message: "something happened".to_string(),
        fields: vec![("key".to_string(), "value".to_string())],
    };
    let notif = test_helpers::make_notification(event);
    insta::assert_json_snapshot!(notif);
}

// ── Diagnostics tool tests ───────────────────────────────────────────────────

#[tokio::test]
async fn test_get_version_returns_current_version() {
    let (_dir, state_dir) = temp_state_dir();
    let server = make_server(&state_dir);
    let req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": "get_version", "arguments": {} }
    });
    let resp = test_helpers::dispatch_request(&server, req).await.unwrap();
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(parsed["name"], "dione");
    assert_eq!(parsed["version"], env!("CARGO_PKG_VERSION"));
}

#[tokio::test]
async fn test_set_trace_level_accepts_valid_filter() {
    let (_dir, state_dir) = temp_state_dir();
    let server = make_server(&state_dir);
    let req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": "set_trace_level", "arguments": { "filter": "dione=debug" } }
    });
    let resp = test_helpers::dispatch_request(&server, req).await.unwrap();
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["channel_filter"], "dione=debug");
}

#[tokio::test]
async fn test_set_trace_level_rejects_invalid_filter() {
    let (_dir, state_dir) = temp_state_dir();
    let server = make_server(&state_dir);
    let req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": "set_trace_level", "arguments": { "filter": "not a valid:::filter[[[" } }
    });
    let resp = test_helpers::dispatch_request(&server, req).await.unwrap();
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(text).unwrap();
    assert!(parsed.get("error").is_some());
}

#[tokio::test]
async fn test_set_stderr_level_accepts_valid_filter() {
    let (_dir, state_dir) = temp_state_dir();
    let server = make_server(&state_dir);
    let req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": "set_stderr_level", "arguments": { "filter": "dione=warn" } }
    });
    let resp = test_helpers::dispatch_request(&server, req).await.unwrap();
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["stderr_filter"], "dione=warn");
}

#[tokio::test]
async fn test_set_trace_level_missing_filter_param() {
    let (_dir, state_dir) = temp_state_dir();
    let server = make_server(&state_dir);
    let req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": "set_trace_level", "arguments": {} }
    });
    let resp = test_helpers::dispatch_request(&server, req).await.unwrap();
    assert!(resp["error"].is_object());
}

// ── Permission request handler tests ─────────────────────────────────────────

#[tokio::test]
async fn test_permission_request_empty_id_is_ignored() {
    let (_dir, state_dir) = temp_state_dir();
    let server = make_server(&state_dir);
    let req = json!({
        "jsonrpc": "2.0",
        "method": "notifications/claude/channel/permission_request",
        "params": {
            "request_id": "",
            "tool_name": "Bash",
            "description": "run ls",
            "input_preview": "{\"command\":\"ls\"}"
        }
    });
    // Notifications return None (no response). The key test is it doesn't panic.
    let resp = test_helpers::dispatch_request(&server, req).await;
    assert!(resp.is_none());
}

#[tokio::test]
async fn test_permission_request_missing_id_is_ignored() {
    let (_dir, state_dir) = temp_state_dir();
    let server = make_server(&state_dir);
    let req = json!({
        "jsonrpc": "2.0",
        "method": "notifications/claude/channel/permission_request",
        "params": {
            "tool_name": "Write",
            "description": "write file",
            "input_preview": "{}"
        }
    });
    let resp = test_helpers::dispatch_request(&server, req).await;
    assert!(resp.is_none());
}

// ── no_rly tool dispatch tests ───────────────────────────────────────────────

#[tokio::test]
async fn test_no_rly_unknown_handle_errors() {
    let (_dir, state_dir) = temp_state_dir();
    let server = make_server(&state_dir);
    let req = json!({
        "jsonrpc": "2.0",
        "id": 60,
        "method": "tools/call",
        "params": { "name": "no_rly", "arguments": { "handle": "nr-dead-1" } }
    });
    let resp = test_helpers::dispatch_request(&server, req).await.unwrap();
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(text).unwrap();
    let error = parsed["error"].as_str().expect("must be an error");
    assert!(
        error.contains("unknown or already-used handle"),
        "unknown handle must be named: {error}"
    );
    assert_eq!(resp["result"]["isError"], true);
}

#[tokio::test]
async fn test_rephrase_unknown_handle_errors() {
    let (_dir, state_dir) = temp_state_dir();
    let server = make_server(&state_dir);
    let req = json!({
        "jsonrpc": "2.0",
        "id": 61,
        "method": "tools/call",
        "params": { "name": "rephrase", "arguments": { "handle": "nr-dead-2", "content": "new text" } }
    });
    let resp = test_helpers::dispatch_request(&server, req).await.unwrap();
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(text).unwrap();
    assert!(
        parsed["error"]
            .as_str()
            .unwrap()
            .contains("unknown or already-used handle")
    );
}

#[tokio::test]
async fn test_rephrase_missing_content_is_protocol_error() {
    let (_dir, state_dir) = temp_state_dir();
    let server = make_server(&state_dir);
    let req = json!({
        "jsonrpc": "2.0",
        "id": 62,
        "method": "tools/call",
        "params": { "name": "rephrase", "arguments": { "handle": "nr-dead-3" } }
    });
    let resp = test_helpers::dispatch_request(&server, req).await.unwrap();
    assert!(resp["error"].is_object(), "missing content must error");
}

#[tokio::test]
async fn test_no_rly_stats_empty_journal_reports_zeros() {
    let (_dir, state_dir) = temp_state_dir();
    let server = make_server(&state_dir);
    let req = json!({
        "jsonrpc": "2.0",
        "id": 63,
        "method": "tools/call",
        "params": { "name": "no_rly_stats", "arguments": {} }
    });
    let resp = test_helpers::dispatch_request(&server, req).await.unwrap();
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(parsed["bounces"], 0);
    assert_eq!(parsed["pending"], 0);
    assert_eq!(parsed["malformed_lines"], 0);
    assert!(parsed.get("error").is_none());
}

#[tokio::test]
async fn test_no_rly_stats_rejects_invalid_outcome() {
    let (_dir, state_dir) = temp_state_dir();
    let server = make_server(&state_dir);
    let req = json!({
        "jsonrpc": "2.0",
        "id": 64,
        "method": "tools/call",
        "params": { "name": "no_rly_stats", "arguments": { "outcome": "vibed" } }
    });
    let resp = test_helpers::dispatch_request(&server, req).await.unwrap();
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(text).unwrap();
    assert!(parsed["error"].as_str().unwrap().contains("invalid outcome"));
}

#[tokio::test]
async fn test_no_rly_condense_and_vacuum_empty_journal() {
    let (_dir, state_dir) = temp_state_dir();
    let server = make_server(&state_dir);

    let req = json!({
        "jsonrpc": "2.0",
        "id": 65,
        "method": "tools/call",
        "params": { "name": "no_rly_condense", "arguments": {} }
    });
    let resp = test_helpers::dispatch_request(&server, req).await.unwrap();
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(parsed["condensed_bounces"], 0);

    let req = json!({
        "jsonrpc": "2.0",
        "id": 66,
        "method": "tools/call",
        "params": { "name": "no_rly_vacuum", "arguments": {} }
    });
    let resp = test_helpers::dispatch_request(&server, req).await.unwrap();
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(parsed["dropped_summaries"], 0);
    assert_eq!(parsed["kept"], 0);
}

// ── suppress_ping tests ─────────────────────────────────────────────────────

#[test]
fn test_reply_tool_schema_includes_suppress_ping() {
    let list = test_helpers::get_tools_list();
    let tools = list["tools"].as_array().expect("tools must be an array");

    let reply_tool = tools
        .iter()
        .find(|t| t["name"] == "reply")
        .expect("reply tool must exist in tools/list");

    let props = &reply_tool["inputSchema"]["properties"];
    assert!(
        props.get("suppress_ping").is_some(),
        "reply tool schema must include suppress_ping property"
    );
    insta::assert_json_snapshot!(props["suppress_ping"]);
}

#[tokio::test]
async fn test_reply_suppress_ping_defaults_to_false_gate_rejection() {
    let (_dir, state_dir) = temp_state_dir();
    let server = make_server(&state_dir);

    // Call reply without suppress_ping — should still reach the gate (and be
    // rejected because no channel is configured).
    let req = json!({
        "jsonrpc": "2.0",
        "id": 50,
        "method": "tools/call",
        "params": {
            "name": "reply",
            "arguments": {
                "channel_id": "999999",
                "content": "hello"
            }
        }
    });
    let resp = test_helpers::dispatch_request(&server, req).await.unwrap();
    insta::assert_json_snapshot!(resp["result"]);
}

#[tokio::test]
async fn test_reply_suppress_ping_true_gate_rejection() {
    let (_dir, state_dir) = temp_state_dir();
    let server = make_server(&state_dir);

    // Call reply with suppress_ping=true — should still reach the gate (and be
    // rejected because no channel is configured). This exercises the arg parsing
    // path for suppress_ping.
    let req = json!({
        "jsonrpc": "2.0",
        "id": 51,
        "method": "tools/call",
        "params": {
            "name": "reply",
            "arguments": {
                "channel_id": "999999",
                "content": "hello",
                "suppress_ping": true
            }
        }
    });
    let resp = test_helpers::dispatch_request(&server, req).await.unwrap();
    insta::assert_json_snapshot!(resp["result"]);
}

#[tokio::test]
async fn test_reply_suppress_ping_false_explicit_gate_rejection() {
    let (_dir, state_dir) = temp_state_dir();
    let server = make_server(&state_dir);

    let req = json!({
        "jsonrpc": "2.0",
        "id": 52,
        "method": "tools/call",
        "params": {
            "name": "reply",
            "arguments": {
                "channel_id": "999999",
                "content": "hello",
                "suppress_ping": false
            }
        }
    });
    let resp = test_helpers::dispatch_request(&server, req).await.unwrap();
    insta::assert_json_snapshot!(resp["result"]);
}

#[tokio::test]
async fn test_reply_suppress_ping_true_passes_gate_with_configured_channel() {
    let (_dir, state_dir) = temp_state_dir();

    // Configure a channel so the outbound gate allows it.
    let mut config = dione::config::Config::default();
    config.channels.push(dione::config::ChannelConfig {
        id: "100100".to_string(),
        require_mention: false,
        allow_from: vec![],
        ..Default::default()
    });
    let _config = set_global_config(config);
    let server = make_server(&state_dir);

    // With suppress_ping=true, the reply should pass the gate but fail at the
    // Discord HTTP layer (fake token). The error should NOT be a gate rejection.
    let req = json!({
        "jsonrpc": "2.0",
        "id": 53,
        "method": "tools/call",
        "params": {
            "name": "reply",
            "arguments": {
                "channel_id": "100100",
                "content": "test message",
                "suppress_ping": true
            }
        }
    });
    let resp = test_helpers::dispatch_request(&server, req).await.unwrap();
    // The error should be a Discord HTTP error, not a gate rejection.
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        !text.contains("not a permitted outbound target"),
        "reply should have passed the outbound gate, got: {text}"
    );
    insta::assert_json_snapshot!(resp["result"]);
}

#[tokio::test]
async fn test_reply_suppress_ping_false_passes_gate_with_configured_channel() {
    let (_dir, state_dir) = temp_state_dir();

    // Configure a channel so the outbound gate allows it.
    let mut config = dione::config::Config::default();
    config.channels.push(dione::config::ChannelConfig {
        id: "100101".to_string(),
        require_mention: false,
        allow_from: vec![],
        ..Default::default()
    });
    let _config = set_global_config(config);
    let server = make_server(&state_dir);

    // With suppress_ping=false (default behavior), same path but without
    // allowed_mentions being set.
    let req = json!({
        "jsonrpc": "2.0",
        "id": 54,
        "method": "tools/call",
        "params": {
            "name": "reply",
            "arguments": {
                "channel_id": "100101",
                "content": "test message",
                "suppress_ping": false
            }
        }
    });
    let resp = test_helpers::dispatch_request(&server, req).await.unwrap();
    // The error should be a Discord HTTP error, not a gate rejection.
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        !text.contains("not a permitted outbound target"),
        "reply should have passed the outbound gate, got: {text}"
    );
    insta::assert_json_snapshot!(resp["result"]);
}
