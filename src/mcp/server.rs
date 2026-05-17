//! Manual MCP JSON-RPC server over stdio.
//!
//! We implement the MCP protocol by hand rather than using rmcp's stdio
//! transport. This gives us full control of stdout so we can interleave
//! custom `notifications/channel` JSON-RPC notifications alongside normal
//! tool responses without fighting rmcp's transport ownership.
//!
//! Protocol: line-delimited JSON-RPC 2.0.
//! - Request  → `{"jsonrpc":"2.0","id":N,"method":"...","params":{...}}`
//! - Response → `{"jsonrpc":"2.0","id":N,"result":{...}}`
//! - Error    → `{"jsonrpc":"2.0","id":N,"error":{"code":-32000,"message":"..."}}`
//! - Notification (no id) → `{"jsonrpc":"2.0","method":"...","params":{...}}`

use std::sync::Arc;

use camino::Utf8PathBuf;
use serde_json::{Value, json};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

use crate::discord::events::NotificationEvent;
use crate::mcp::tools::{
    access::{AccessCtx, approve_access, deny_access, list_access_requests},
    bot_state::{BotStateCtx, DiscordCommand, send_typing, set_presence},
    introspection::{
        IntrospectionCtx, get_channel, get_member, get_user, list_channels, list_emojis,
        list_guilds, list_roles,
    },
    management::{ManagementCtx, create_thread, delete_message, pin_message, unpin_message},
    messaging::{
        MessagingCtx, download_attachment, edit_message, fetch_messages, get_message,
        react as discord_react, reply,
    },
};

// ── Server struct ─────────────────────────────────────────────────────────────

/// Runtime context for the MCP server.
pub struct DioneServer {
    pub state: crate::state::State,
    pub queue: Arc<Mutex<crate::queue::AccessQueue>>,
    pub http: Arc<serenity::http::Http>,
    pub state_dir: Utf8PathBuf,
    pub notification_tx: mpsc::Sender<Value>,
    pub discord_cmd_tx: Option<mpsc::Sender<DiscordCommand>>,
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Runs the MCP server: reads JSON-RPC from stdin, writes to stdout.
///
/// Also spawns a task that converts [`NotificationEvent`]s from Discord into
/// MCP notifications and writes them to stdout.
pub async fn run(
    server: DioneServer,
    event_rx: mpsc::Receiver<NotificationEvent>,
    cancel: CancellationToken,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let stdin = BufReader::new(tokio::io::stdin());
    let stdout = Arc::new(Mutex::new(tokio::io::stdout()));

    // Notification forwarding task.
    // The task exits naturally when the sender side is dropped (channel closed).
    let stdout_notif = stdout.clone();
    let notif_task = tokio::spawn(async move {
        let mut rx = event_rx;
        while let Some(event) = rx.recv().await {
            let notification = event_to_notification(event);
            write_line(&stdout_notif, &notification).await;
        }
    });

    // Main request loop.
    let mut lines = stdin.lines();
    loop {
        tokio::select! {
            biased;

            _ = cancel.cancelled() => {
                tracing::info!("MCP server shutting down");
                break;
            }

            line = lines.next_line() => {
                match line {
                    Ok(Some(text)) => {
                        let trimmed = text.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        match serde_json::from_str::<Value>(trimmed) {
                            Ok(req) => {
                                let resp = handle_request(&server, req).await;
                                if let Some(resp_value) = resp {
                                    write_line(&stdout, &resp_value).await;
                                }
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "failed to parse JSON-RPC line");
                                let err_resp = json!({
                                    "jsonrpc": "2.0",
                                    "id": null,
                                    "error": { "code": -32700, "message": "parse error" }
                                });
                                write_line(&stdout, &err_resp).await;
                            }
                        }
                    }
                    Ok(None) => {
                        // EOF — client disconnected.
                        tracing::info!("stdin EOF, MCP server exiting");
                        break;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "stdin read error");
                        break;
                    }
                }
            }
        }
    }

    // Drop the server to close the notification channel sender, signalling the
    // notif_task to drain remaining events and exit cleanly.
    drop(server);

    // Give the notification task a short window to drain remaining events.
    let _ = tokio::time::timeout(Duration::from_millis(500), notif_task).await;

    Ok(())
}

// ── Request dispatch ──────────────────────────────────────────────────────────

async fn handle_request(server: &DioneServer, req: Value) -> Option<Value> {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(json!({}));

    // Notifications (no id) from client — process but don't respond.
    let is_notification = id.is_none();

    let result = dispatch(server, method, params).await;

    if is_notification {
        // Client-sent notification: consume silently.
        return None;
    }

    let id = id.unwrap();
    Some(match result {
        Ok(v) => json!({ "jsonrpc": "2.0", "id": id, "result": v }),
        Err(msg) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32000, "message": msg }
        }),
    })
}

async fn dispatch(server: &DioneServer, method: &str, params: Value) -> Result<Value, String> {
    match method {
        // ── MCP lifecycle ─────────────────────────────────────────────────────
        "initialize" => Ok(initialize_response()),
        "notifications/initialized" => Ok(json!({})),

        // ── Tool discovery ────────────────────────────────────────────────────
        "tools/list" => Ok(tools_list()),

        // ── Tool invocation ───────────────────────────────────────────────────
        "tools/call" => {
            let tool_name = params
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing tool name".to_string())?;
            let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
            call_tool(server, tool_name, arguments).await
        }

        other => {
            tracing::debug!(method = other, "unknown MCP method");
            Err(format!("method not found: {other}"))
        }
    }
}

// ── MCP initialize response ───────────────────────────────────────────────────

fn initialize_response() -> Value {
    json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {
            "tools": {}
        },
        "serverInfo": {
            "name": "dione",
            "version": env!("CARGO_PKG_VERSION"),
        }
    })
}

// ── Tool list ─────────────────────────────────────────────────────────────────

fn tools_list() -> Value {
    json!({
        "tools": [
            tool("reply", "Send a reply to a Discord channel or DM", json!({
                "type": "object",
                "required": ["channel_id", "content"],
                "properties": {
                    "channel_id": { "type": "string", "description": "Discord channel ID" },
                    "content": { "type": "string", "description": "Message content" },
                    "reply_to_message_id": { "type": "string", "description": "Optional message ID to reply to" }
                }
            })),
            tool("react", "Add a reaction to a message", json!({
                "type": "object",
                "required": ["channel_id", "message_id", "emoji"],
                "properties": {
                    "channel_id": { "type": "string" },
                    "message_id": { "type": "string" },
                    "emoji": { "type": "string", "description": "Unicode emoji or emoji name" }
                }
            })),
            tool("edit_message", "Edit a bot message", json!({
                "type": "object",
                "required": ["channel_id", "message_id", "content"],
                "properties": {
                    "channel_id": { "type": "string" },
                    "message_id": { "type": "string" },
                    "content": { "type": "string" }
                }
            })),
            tool("fetch_messages", "Fetch recent messages from a channel", json!({
                "type": "object",
                "required": ["channel_id"],
                "properties": {
                    "channel_id": { "type": "string" },
                    "limit": { "type": "integer", "default": 20, "maximum": 100 }
                }
            })),
            tool("download_attachment", "Download all attachments from a message to the inbox", json!({
                "type": "object",
                "required": ["channel_id", "message_id"],
                "properties": {
                    "channel_id": { "type": "string" },
                    "message_id": { "type": "string" }
                }
            })),
            tool("get_message", "Retrieve a single message", json!({
                "type": "object",
                "required": ["channel_id", "message_id"],
                "properties": {
                    "channel_id": { "type": "string" },
                    "message_id": { "type": "string" }
                }
            })),
            tool("list_guilds", "List guilds the bot is in", json!({
                "type": "object",
                "properties": {}
            })),
            tool("list_channels", "List channels in a guild", json!({
                "type": "object",
                "required": ["guild_id"],
                "properties": {
                    "guild_id": { "type": "string" }
                }
            })),
            tool("get_channel", "Get channel details", json!({
                "type": "object",
                "required": ["channel_id"],
                "properties": {
                    "channel_id": { "type": "string" }
                }
            })),
            tool("get_user", "Get user information", json!({
                "type": "object",
                "required": ["user_id"],
                "properties": {
                    "user_id": { "type": "string" }
                }
            })),
            tool("get_member", "Get guild member information", json!({
                "type": "object",
                "required": ["guild_id", "user_id"],
                "properties": {
                    "guild_id": { "type": "string" },
                    "user_id": { "type": "string" }
                }
            })),
            tool("list_roles", "List roles in a guild", json!({
                "type": "object",
                "required": ["guild_id"],
                "properties": {
                    "guild_id": { "type": "string" }
                }
            })),
            tool("list_emojis", "List custom emoji available in a guild (name, id, animated flag, and the string to use in reactions/messages)", json!({
                "type": "object",
                "required": ["guild_id"],
                "properties": {
                    "guild_id": { "type": "string" }
                }
            })),
            tool("pin_message", "Pin a message in a channel", json!({
                "type": "object",
                "required": ["channel_id", "message_id"],
                "properties": {
                    "channel_id": { "type": "string" },
                    "message_id": { "type": "string" }
                }
            })),
            tool("unpin_message", "Unpin a message", json!({
                "type": "object",
                "required": ["channel_id", "message_id"],
                "properties": {
                    "channel_id": { "type": "string" },
                    "message_id": { "type": "string" }
                }
            })),
            tool("create_thread", "Create a thread", json!({
                "type": "object",
                "required": ["channel_id", "name"],
                "properties": {
                    "channel_id": { "type": "string" },
                    "message_id": { "type": "string", "description": "If set, creates a thread from this message" },
                    "name": { "type": "string" }
                }
            })),
            tool("delete_message", "Delete a message", json!({
                "type": "object",
                "required": ["channel_id", "message_id"],
                "properties": {
                    "channel_id": { "type": "string" },
                    "message_id": { "type": "string" }
                }
            })),
            tool("list_access_requests", "List pending access requests from unknown users", json!({
                "type": "object",
                "properties": {}
            })),
            tool("approve_access", "Approve a user's access request (adds to allow_from)", json!({
                "type": "object",
                "required": ["user_id"],
                "properties": {
                    "user_id": { "type": "string" }
                }
            })),
            tool("deny_access", "Deny a user's access request", json!({
                "type": "object",
                "required": ["user_id"],
                "properties": {
                    "user_id": { "type": "string" }
                }
            })),
            // set_presence is intentionally excluded from the tools list:
            // presence updates require the Discord gateway shard manager, which is
            // not yet wired to the MCP command channel. The implementation stub
            // remains in bot_state.rs for future use.
            tool("send_typing", "Send a typing indicator to a channel", json!({
                "type": "object",
                "required": ["channel_id"],
                "properties": {
                    "channel_id": { "type": "string" }
                }
            })),
        ]
    })
}

fn tool(name: &str, description: &str, input_schema: Value) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": input_schema,
    })
}

// ── Tool dispatch ─────────────────────────────────────────────────────────────

async fn call_tool(server: &DioneServer, name: &str, args: Value) -> Result<Value, String> {
    let result = match name {
        // Messaging
        "reply" => {
            let ctx = MessagingCtx {
                http: server.http.clone(),
                state: server.state.clone(),
                state_dir: server.state_dir.clone(),
            };
            let channel_id = parse_id(&args, "channel_id")?;
            let content = args
                .get("content")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing content".to_string())?;
            let reply_to = parse_optional_id(&args, "reply_to_message_id");
            reply(&ctx, channel_id, content, reply_to).await
        }
        "react" => {
            let ctx = MessagingCtx {
                http: server.http.clone(),
                state: server.state.clone(),
                state_dir: server.state_dir.clone(),
            };
            let channel_id = parse_id(&args, "channel_id")?;
            let message_id = parse_id(&args, "message_id")?;
            let emoji = args
                .get("emoji")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing emoji".to_string())?;
            discord_react(&ctx, channel_id, message_id, emoji).await
        }
        "edit_message" => {
            let ctx = MessagingCtx {
                http: server.http.clone(),
                state: server.state.clone(),
                state_dir: server.state_dir.clone(),
            };
            let channel_id = parse_id(&args, "channel_id")?;
            let message_id = parse_id(&args, "message_id")?;
            let content = args
                .get("content")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing content".to_string())?;
            edit_message(&ctx, channel_id, message_id, content).await
        }
        "fetch_messages" => {
            let ctx = MessagingCtx {
                http: server.http.clone(),
                state: server.state.clone(),
                state_dir: server.state_dir.clone(),
            };
            let channel_id = parse_id(&args, "channel_id")?;
            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .map(|v| v.min(100) as u8)
                .unwrap_or(20);
            fetch_messages(&ctx, channel_id, limit).await
        }
        "download_attachment" => {
            let ctx = MessagingCtx {
                http: server.http.clone(),
                state: server.state.clone(),
                state_dir: server.state_dir.clone(),
            };
            let channel_id = parse_id(&args, "channel_id")?;
            let message_id = parse_id(&args, "message_id")?;
            download_attachment(&ctx, channel_id, message_id).await
        }
        "get_message" => {
            let ctx = MessagingCtx {
                http: server.http.clone(),
                state: server.state.clone(),
                state_dir: server.state_dir.clone(),
            };
            let channel_id = parse_id(&args, "channel_id")?;
            let message_id = parse_id(&args, "message_id")?;
            get_message(&ctx, channel_id, message_id).await
        }

        // Introspection
        "list_guilds" => {
            let ctx = IntrospectionCtx {
                http: server.http.clone(),
            };
            list_guilds(&ctx).await
        }
        "list_channels" => {
            let ctx = IntrospectionCtx {
                http: server.http.clone(),
            };
            let guild_id = parse_id(&args, "guild_id")?;
            list_channels(&ctx, guild_id).await
        }
        "get_channel" => {
            let ctx = IntrospectionCtx {
                http: server.http.clone(),
            };
            let channel_id = parse_id(&args, "channel_id")?;
            get_channel(&ctx, channel_id).await
        }
        "get_user" => {
            let ctx = IntrospectionCtx {
                http: server.http.clone(),
            };
            let user_id = parse_id(&args, "user_id")?;
            get_user(&ctx, user_id).await
        }
        "get_member" => {
            let ctx = IntrospectionCtx {
                http: server.http.clone(),
            };
            let guild_id = parse_id(&args, "guild_id")?;
            let user_id = parse_id(&args, "user_id")?;
            get_member(&ctx, guild_id, user_id).await
        }
        "list_roles" => {
            let ctx = IntrospectionCtx {
                http: server.http.clone(),
            };
            let guild_id = parse_id(&args, "guild_id")?;
            list_roles(&ctx, guild_id).await
        }
        "list_emojis" => {
            let ctx = IntrospectionCtx {
                http: server.http.clone(),
            };
            let guild_id = parse_id(&args, "guild_id")?;
            list_emojis(&ctx, guild_id).await
        }

        // Management
        "pin_message" => {
            let ctx = ManagementCtx {
                http: server.http.clone(),
                state: server.state.clone(),
                state_dir: server.state_dir.clone(),
            };
            let channel_id = parse_id(&args, "channel_id")?;
            let message_id = parse_id(&args, "message_id")?;
            pin_message(&ctx, channel_id, message_id).await
        }
        "unpin_message" => {
            let ctx = ManagementCtx {
                http: server.http.clone(),
                state: server.state.clone(),
                state_dir: server.state_dir.clone(),
            };
            let channel_id = parse_id(&args, "channel_id")?;
            let message_id = parse_id(&args, "message_id")?;
            unpin_message(&ctx, channel_id, message_id).await
        }
        "create_thread" => {
            let ctx = ManagementCtx {
                http: server.http.clone(),
                state: server.state.clone(),
                state_dir: server.state_dir.clone(),
            };
            let channel_id = parse_id(&args, "channel_id")?;
            let message_id = parse_optional_id(&args, "message_id");
            let name = args
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing name".to_string())?;
            create_thread(&ctx, channel_id, message_id, name).await
        }
        "delete_message" => {
            let ctx = ManagementCtx {
                http: server.http.clone(),
                state: server.state.clone(),
                state_dir: server.state_dir.clone(),
            };
            let channel_id = parse_id(&args, "channel_id")?;
            let message_id = parse_id(&args, "message_id")?;
            delete_message(&ctx, channel_id, message_id).await
        }

        // Access
        "list_access_requests" => {
            let ctx = AccessCtx {
                queue: server.queue.clone(),
                state_dir: server.state_dir.clone(),
            };
            list_access_requests(&ctx).await
        }
        "approve_access" => {
            let ctx = AccessCtx {
                queue: server.queue.clone(),
                state_dir: server.state_dir.clone(),
            };
            let user_id = parse_id(&args, "user_id")?;
            approve_access(&ctx, user_id).await
        }
        "deny_access" => {
            let ctx = AccessCtx {
                queue: server.queue.clone(),
                state_dir: server.state_dir.clone(),
            };
            let user_id = parse_id(&args, "user_id")?;
            deny_access(&ctx, user_id).await
        }

        // Bot state
        "set_presence" => {
            let ctx = BotStateCtx {
                http: server.http.clone(),
                discord_cmd_tx: server.discord_cmd_tx.clone(),
                state: server.state.clone(),
                state_dir: server.state_dir.clone(),
            };
            let status = args
                .get("status")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing status".to_string())?;
            let activity_name = args.get("activity_name").and_then(Value::as_str);
            set_presence(&ctx, status, activity_name).await
        }
        "send_typing" => {
            let ctx = BotStateCtx {
                http: server.http.clone(),
                discord_cmd_tx: server.discord_cmd_tx.clone(),
                state: server.state.clone(),
                state_dir: server.state_dir.clone(),
            };
            let channel_id = parse_id(&args, "channel_id")?;
            send_typing(&ctx, channel_id).await
        }

        unknown => return Err(format!("unknown tool: {unknown}")),
    };

    // Wrap result in MCP tool result format.
    // If the result contains an "error" key, set isError: true per MCP spec.
    let is_error = result.get("error").is_some();
    let mut response = json!({
        "content": [
            { "type": "text", "text": result.to_string() }
        ]
    });
    if is_error {
        response["isError"] = json!(true);
    }
    Ok(response)
}

// ── Notification conversion ───────────────────────────────────────────────────

fn event_to_notification(event: NotificationEvent) -> Value {
    match event {
        NotificationEvent::Message {
            chat_id,
            message_id,
            user,
            user_id,
            content,
            timestamp,
            attachments,
            is_voice_message,
        } => {
            json!({
                "jsonrpc": "2.0",
                "method": "notifications/channel",
                "params": {
                    "type": "message",
                    "chat_id": chat_id,
                    "message_id": message_id,
                    "user": user,
                    "user_id": user_id,
                    "content": content,
                    "timestamp": timestamp,
                    "is_voice_message": is_voice_message,
                    "attachments": attachments.iter().map(|a| json!({
                        "name": a.name,
                        "content_type": a.content_type,
                        "size": a.size,
                    })).collect::<Vec<_>>(),
                }
            })
        }
        NotificationEvent::Reaction {
            chat_id,
            message_id,
            user,
            user_id,
            emoji,
        } => {
            json!({
                "jsonrpc": "2.0",
                "method": "notifications/channel",
                "params": {
                    "type": "reaction",
                    "chat_id": chat_id,
                    "message_id": message_id,
                    "user": user,
                    "user_id": user_id,
                    "emoji": emoji,
                }
            })
        }
        NotificationEvent::PermissionResponse {
            request_id,
            granted,
        } => {
            json!({
                "jsonrpc": "2.0",
                "method": "notifications/channel",
                "params": {
                    "type": "permission_response",
                    "request_id": request_id,
                    "granted": granted,
                }
            })
        }
    }
}

// ── I/O helpers ───────────────────────────────────────────────────────────────

async fn write_line(stdout: &Arc<Mutex<tokio::io::Stdout>>, value: &Value) {
    let mut line = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    line.push('\n');
    let mut out = stdout.lock().await;
    if let Err(e) = out.write_all(line.as_bytes()).await {
        tracing::warn!(error = %e, "failed to write MCP response to stdout");
    }
    if let Err(e) = out.flush().await {
        tracing::warn!(error = %e, "failed to flush stdout");
    }
}

// ── Parameter parsing helpers ─────────────────────────────────────────────────

fn parse_id(args: &Value, key: &str) -> Result<u64, String> {
    // Accept both numeric and string IDs.
    if let Some(n) = args.get(key).and_then(Value::as_u64) {
        return Ok(n);
    }
    if let Some(s) = args.get(key).and_then(Value::as_str) {
        return s
            .parse::<u64>()
            .map_err(|_| format!("invalid {key}: not a valid u64"));
    }
    Err(format!("missing required parameter: {key}"))
}

fn parse_optional_id(args: &Value, key: &str) -> Option<u64> {
    if let Some(n) = args.get(key).and_then(Value::as_u64) {
        return Some(n);
    }
    if let Some(s) = args.get(key).and_then(Value::as_str) {
        return s.parse::<u64>().ok();
    }
    None
}

// ── Test helpers ─────────────────────────────────────────────────────────────

/// Test helpers that expose internal functions through the crate's public API.
///
/// Test helpers for integration tests — always compiled (zero-cost: only
/// re-exports existing functions for testability).
pub mod test_helpers {
    use super::*;

    /// Exposes `event_to_notification` for unit testing notification format.
    pub fn make_notification(event: NotificationEvent) -> Value {
        event_to_notification(event)
    }

    /// Exposes `tools_list` for unit testing tool discovery.
    pub fn get_tools_list() -> Value {
        tools_list()
    }

    /// Exposes `initialize_response` for unit testing the handshake.
    pub fn get_initialize_response() -> Value {
        initialize_response()
    }

    /// Exposes `handle_request` for unit testing request dispatch.
    pub async fn dispatch_request(server: &DioneServer, req: Value) -> Option<Value> {
        handle_request(server, req).await
    }
}
