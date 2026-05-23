//! MCP protocol responses: initialize handshake and tool list.

use serde_json::{Value, json};

/// Build the MCP `initialize` response.
pub(crate) fn initialize_response() -> Value {
    json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {
            "tools": {},
            "experimental": {
                "claude/channel": {},
                "claude/channel/permission": {},
            }
        },
        "serverInfo": {
            "name": "dione",
            "version": env!("CARGO_PKG_VERSION"),
        }
    })
}

/// Build the MCP `tools/list` response.
pub(crate) fn tools_list() -> Value {
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
            tool("send_file", "Upload a file as an attachment to a Discord channel", json!({
                "type": "object",
                "required": ["channel_id", "file_path"],
                "properties": {
                    "channel_id": { "type": "string", "description": "Discord channel ID" },
                    "file_path": { "type": "string", "description": "Absolute path to the file to upload" },
                    "caption": { "type": "string", "description": "Optional message text to accompany the file" }
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
            tool("render_latex", "Render a LaTeX math expression to a PNG image file", json!({
                "type": "object",
                "required": ["latex"],
                "properties": {
                    "latex": { "type": "string", "description": "LaTeX math expression (without $ delimiters)" }
                }
            })),
            tool("render_latex_to_channel", "Render a LaTeX math expression and post it as an image to a Discord channel", json!({
                "type": "object",
                "required": ["channel_id", "latex"],
                "properties": {
                    "channel_id": { "type": "string", "description": "Discord channel ID" },
                    "latex": { "type": "string", "description": "LaTeX math expression (without $ delimiters)" },
                    "caption": { "type": "string", "description": "Optional message text to accompany the image" }
                }
            })),
            tool("send_typing", "Send a typing indicator to a channel", json!({
                "type": "object",
                "required": ["channel_id"],
                "properties": {
                    "channel_id": { "type": "string" }
                }
            })),
            tool("get_version", "Get dione version information", json!({
                "type": "object",
                "properties": {}
            })),
            tool("set_trace_level", "Set the channel-forwarding trace filter (events matching this filter are sent as channel notifications with type=\"trace\")", json!({
                "type": "object",
                "required": ["filter"],
                "properties": {
                    "filter": { "type": "string", "description": "tracing EnvFilter string, e.g. \"dione=debug\", \"dione::discord::events=trace\", or \"off\" to disable" }
                }
            })),
            tool("set_stderr_level", "Set the stderr logging trace filter", json!({
                "type": "object",
                "required": ["filter"],
                "properties": {
                    "filter": { "type": "string", "description": "tracing EnvFilter string, e.g. \"dione=debug\", \"dione::discord::events=trace\"" }
                }
            })),
            tool("list_config_channels", "List all channels in dione's config with their settings", json!({
                "type": "object",
                "properties": {}
            })),
            tool("get_access_config", "Get the current access config (dm_policy, allow_from, admins)", json!({
                "type": "object",
                "properties": {}
            })),
            tool("add_channel", "Add a channel to dione's config", json!({
                "type": "object",
                "required": ["id"],
                "properties": {
                    "id": { "type": "string", "description": "Discord channel ID" },
                    "require_mention": { "type": "boolean", "description": "Whether the bot must be mentioned to respond (default: true)" },
                    "allow_from": { "type": "array", "items": { "type": "string" }, "description": "User IDs allowed in this channel (empty = everyone)" }
                }
            })),
            tool("remove_channel", "Remove a channel from dione's config", json!({
                "type": "object",
                "required": ["id"],
                "properties": {
                    "id": { "type": "string", "description": "Discord channel ID" }
                }
            })),
            tool("update_channel", "Update settings for a channel in dione's config", json!({
                "type": "object",
                "required": ["id"],
                "properties": {
                    "id": { "type": "string", "description": "Discord channel ID" },
                    "require_mention": { "type": "boolean", "description": "Whether the bot must be mentioned to respond" },
                    "allow_from": { "type": "array", "items": { "type": "string" }, "description": "User IDs allowed in this channel (empty = everyone)" }
                }
            })),
            tool("update_dm_policy", "Update the DM policy in dione's config", json!({
                "type": "object",
                "required": ["policy"],
                "properties": {
                    "policy": { "type": "string", "enum": ["queue", "drop", "disabled"], "description": "DM handling policy" }
                }
            })),
            tool("add_allow_from", "Add a user ID to the global allow_from list", json!({
                "type": "object",
                "required": ["user_id"],
                "properties": {
                    "user_id": { "type": "string", "description": "Discord user ID to allow" }
                }
            })),
            tool("remove_allow_from", "Remove a user ID from the global allow_from list", json!({
                "type": "object",
                "required": ["user_id"],
                "properties": {
                    "user_id": { "type": "string", "description": "Discord user ID to remove" }
                }
            })),
        ]
    })
}

pub(crate) fn tool(name: &str, description: &str, input_schema: Value) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": input_schema,
    })
}
