//! Integration tests for the MCP JSON-RPC protocol layer.
//!
//! These tests exercise `handle_request` (via `test_helpers::dispatch_request`)
//! and `event_to_notification` (via `test_helpers::make_notification`) without
//! a real Discord connection.  Tool calls that require Discord HTTP are tested
//! only up to the gate-rejection path.

use std::sync::Arc;

use dione::discord::events::{AttachmentMeta, NotificationEvent};
use dione::mcp::server::{DioneServer, test_helpers};
use dione::queue::AccessQueue;
use dione::state::new_state;
use dione::tracing_channel::TraceLevelController;
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::{Mutex, mpsc};

// ── Test fixture ──────────────────────────────────────────────────────────────

fn temp_state_dir() -> (TempDir, camino::Utf8PathBuf) {
    let dir = TempDir::new().unwrap();
    let path = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    (dir, path)
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

// ── Notification format tests ─────────────────────────────────────────────────

// Semantic property tests — wire format is pinned by snapshots below.

#[test]
fn test_notification_has_no_id_field() {
    let event = NotificationEvent::Message {
        chat_id: "1".to_string(),
        message_id: "2".to_string(),
        user: "x".to_string(),
        user_id: "3".to_string(),
        content: "hi".to_string(),
        timestamp: "2026-01-01T00:00:00Z".to_string(),
        attachments: vec![],
        is_voice_message: false,
    };
    let notif = test_helpers::make_notification(event);
    assert!(
        notif.get("id").is_none(),
        "notifications must not have an id field"
    );
}

#[test]
fn test_notification_attachment_metadata_present() {
    let event = NotificationEvent::Message {
        chat_id: "1".to_string(),
        message_id: "2".to_string(),
        user: "x".to_string(),
        user_id: "3".to_string(),
        content: "see file".to_string(),
        timestamp: "2026-01-01T00:00:00Z".to_string(),
        attachments: vec![AttachmentMeta {
            name: "photo.png".to_string(),
            content_type: Some("image/png".to_string()),
            size: 2048,
        }],
        is_voice_message: false,
    };
    let notif = test_helpers::make_notification(event);
    let meta = &notif["params"]["meta"];
    assert_eq!(meta["attachment_count"], "1");
    assert!(meta["attachments"].as_str().unwrap().contains("photo.png"));
}

#[test]
fn test_notification_voice_flag_in_meta() {
    let event = NotificationEvent::Message {
        chat_id: "1".to_string(),
        message_id: "2".to_string(),
        user: "x".to_string(),
        user_id: "3".to_string(),
        content: String::new(),
        timestamp: "2026-01-01T00:00:00Z".to_string(),
        attachments: vec![],
        is_voice_message: true,
    };
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
    let event = NotificationEvent::Message {
        chat_id: "1000".to_string(),
        message_id: "2000".to_string(),
        user: "snapuser".to_string(),
        user_id: "3000".to_string(),
        content: "snapshot content".to_string(),
        timestamp: "2026-01-01T00:00:00+00:00".to_string(),
        attachments: vec![],
        is_voice_message: false,
    };
    let notif = test_helpers::make_notification(event);
    insta::assert_json_snapshot!(notif);
}

#[test]
fn test_notification_reaction_snapshot() {
    let event = NotificationEvent::Reaction {
        chat_id: "1001".to_string(),
        message_id: "2001".to_string(),
        user: "reactor".to_string(),
        user_id: "3001".to_string(),
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
        chat_id: "1002".to_string(),
        message_id: "2002".to_string(),
        user: "editor".to_string(),
        user_id: "3002".to_string(),
        new_content: "fixed a typo".to_string(),
        timestamp: "2026-01-01T00:01:00+00:00".to_string(),
    };
    let notif = test_helpers::make_notification(event);
    insta::assert_json_snapshot!(notif);
}

#[test]
fn test_notification_message_delete_snapshot() {
    let event = NotificationEvent::MessageDelete {
        chat_id: "1003".to_string(),
        message_id: "2003".to_string(),
    };
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
