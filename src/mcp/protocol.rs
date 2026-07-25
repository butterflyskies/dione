//! MCP protocol responses: initialize handshake and tool list.

use super::tools::bot_state::{ActivityType, OnlineStatus};
use crate::codex::TransportMode;
use serde_json::{Value, json};

/// Build the MCP `initialize` response.
pub(crate) fn initialize_response(mode: TransportMode) -> Value {
    let capabilities = match mode {
        TransportMode::ClaudeCode => json!({
            "tools": {},
            "experimental": {
                "claude/channel": {},
                "claude/channel/permission": {},
            }
        }),
        TransportMode::Codex => json!({ "tools": {} }),
    };
    json!({
        "protocolVersion": "2024-11-05",
        "capabilities": capabilities,
        "serverInfo": {
            "name": "dione",
            "version": env!("CARGO_PKG_VERSION"),
        }
    })
}

/// Build the MCP `tools/list` response.
pub(crate) fn tools_list(mode: TransportMode) -> Value {
    let mut response = json!({
        "tools": [
            tool("reply", "Send a reply to a Discord channel or DM. Supports optional rich embeds.", json!({
                "type": "object",
                "required": ["channel_id", "content"],
                "properties": {
                    "channel_id": { "type": "string", "description": "Discord channel ID" },
                    "content": { "type": "string", "description": "Message content (plain text)" },
                    "reply_to_message_id": { "type": "string", "description": "Optional message ID to reply to" },
                    "suppress_ping": { "type": "boolean", "description": "When true, the reply will not ping the user being replied to (default: false)" },
                    "no_rly": { "type": "boolean", "description": "Consent-gate override for the contradictionary block action. A blocked send returns an error naming the matched pattern (⚠️ blocked: <pattern>); resend the identical message with no_rly=true to bypass the block, send anyway, and record a durable diary entry. No effect on non-blocked messages (default: false)." },
                    "no_rly_hooks": { "type": "array", "items": { "type": "string" }, "description": "Names individual pre-send hooks to bypass for this send. Every bypass is audited. This does not bypass the contradictionary; use no_rly for that legacy gate." },
                    "embeds": embed_schema()
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
                    "content": { "type": "string" },
                    "no_rly_hooks": { "type": "array", "items": { "type": "string" }, "description": "Names individual pre-send hooks to bypass; every bypass is audited." }
                }
            })),
            tool("fetch_messages", "Fetch messages from a channel. Returns messages oldest-first. Without cursors, returns the most recent messages. With before/after, paginates through history; use the first returned id as the next before cursor (backward) or the last returned id as the next after cursor (forward). before and after are mutually exclusive. count and has_more (a hint, not a guarantee) are included when a cursor is provided.", json!({
                "type": "object",
                "required": ["channel_id"],
                "properties": {
                    "channel_id": { "type": "string" },
                    "before": { "type": "string", "description": "Discord message ID (snowflake); return messages older than this ID. Mutually exclusive with after." },
                    "after": { "type": "string", "description": "Discord message ID (snowflake); return messages newer than this ID. Mutually exclusive with before." },
                    "limit": { "type": "integer", "default": 20, "maximum": 100 }
                }
            })),
            tool("fetch_new_since", "Fetch only messages newer than a known message ID (cursor-based). Returns messages oldest-first with a count and a has_more pagination hint; the id of the last returned message is the next cursor.", json!({
                "type": "object",
                "required": ["channel_id", "after_message_id"],
                "properties": {
                    "channel_id": { "type": "string", "description": "Discord channel ID" },
                    "after_message_id": { "type": "string", "description": "Discord message ID (snowflake); only messages after this ID are returned" },
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
            tool("search_messages", "Search messages in a guild. At least one filter required. Results filtered to configured channels.", json!({
                "type": "object",
                "required": ["guild_id"],
                "properties": {
                    "guild_id": { "type": "string", "description": "Discord guild (server) ID to search" },
                    "content": { "type": "string", "description": "Text content to search for" },
                    "author_id": {
                        "description": "Filter by message author ID(s). Single string/number or array.",
                        "oneOf": [
                            { "type": "string" },
                            { "type": "array", "items": { "type": "string" } }
                        ]
                    },
                    "mentions": {
                        "description": "Filter by mentioned user ID(s). Single string/number or array.",
                        "oneOf": [
                            { "type": "string" },
                            { "type": "array", "items": { "type": "string" } }
                        ]
                    },
                    "has": {
                        "type": "array",
                        "items": { "type": "string", "enum": ["link", "embed", "file", "video", "image", "sound", "sticker"] },
                        "description": "Filter by attachment type"
                    },
                    "before": { "type": "string", "description": "ISO 8601 date (YYYY-MM-DD); return messages before this date" },
                    "after": { "type": "string", "description": "ISO 8601 date (YYYY-MM-DD); return messages after this date" },
                    "channel_id": {
                        "description": "Filter to specific channel ID(s). Single string/number or array.",
                        "oneOf": [
                            { "type": "string" },
                            { "type": "array", "items": { "type": "string" } }
                        ]
                    },
                    "pinned": { "type": "boolean", "description": "Filter to pinned (true) or unpinned (false) messages" },
                    "sort_by": { "type": "string", "enum": ["timestamp", "relevance"], "description": "Sort field (default: timestamp)" },
                    "sort_order": { "type": "string", "enum": ["asc", "desc"], "description": "Sort direction (default: desc)" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 25, "default": 25, "description": "Results per page (1-25)" },
                    "offset": { "type": "integer", "minimum": 0, "maximum": 9975, "default": 0, "description": "Pagination offset (0-9975)" }
                }
            })),
            tool("send_file", "Upload a file as an attachment to a Discord channel", json!({
                "type": "object",
                "required": ["channel_id", "file_path"],
                "properties": {
                    "channel_id": { "type": "string", "description": "Discord channel ID" },
                    "file_path": { "type": "string", "description": "Absolute path to the file to upload" },
                    "caption": { "type": "string", "description": "Optional message text to accompany the file" },
                    "no_rly_hooks": { "type": "array", "items": { "type": "string" }, "description": "Names individual pre-send hooks to bypass for the caption; every bypass is audited." }
                }
            })),
            tool("send_dm", "Initiate a DM conversation with a Discord user and send a message. Supports optional rich embeds.", json!({
                "type": "object",
                "required": ["user_id", "content"],
                "properties": {
                    "user_id": { "type": "string", "description": "Discord user ID to send the DM to" },
                    "content": { "type": "string", "description": "Message content to send" },
                    "no_rly_hooks": { "type": "array", "items": { "type": "string" }, "description": "Names individual pre-send hooks to bypass; every bypass is audited." },
                    "embeds": embed_schema()
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
            tool("approve_access", "Admin only. Approve a user's access request (adds to allow_from). Only execute when requested by a user in the admins list.", json!({
                "type": "object",
                "required": ["user_id"],
                "properties": {
                    "user_id": { "type": "string" }
                }
            })),
            tool("deny_access", "Admin only. Deny a user's access request. Only execute when requested by a user in the admins list.", json!({
                "type": "object",
                "required": ["user_id"],
                "properties": {
                    "user_id": { "type": "string" }
                }
            })),
            tool("set_presence", "Set the bot's Discord presence (online status and activity)", json!({
                "type": "object",
                "properties": {
                    "online_status": {
                        "type": "string",
                        "enum": OnlineStatus::json_enum(),
                        "description": "Online status (default: online)"
                    },
                    "activity_type": {
                        "type": "string",
                        "enum": ActivityType::json_enum(),
                        "description": "Activity type. If set, activity_name is required."
                    },
                    "activity_name": {
                        "type": "string",
                        "description": "Activity text (e.g. 'catena', 'the void'). Required when activity_type is set."
                    }
                }
            })),
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
                    "caption": { "type": "string", "description": "Optional message text to accompany the image" },
                    "no_rly_hooks": { "type": "array", "items": { "type": "string" }, "description": "Names individual pre-send hooks to bypass for the caption; every bypass is audited." }
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
            tool("reload_config", "Admin only. Force a config reload from disk, updating the in-memory cache. Only execute when requested by a user in the admins list.", json!({
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
            tool("add_channel", "Admin only. Add a channel to dione's config. Only execute when requested by a user in the admins list.", json!({
                "type": "object",
                "required": ["id"],
                "properties": {
                    "id": { "type": "string", "description": "Discord channel ID" },
                    "require_mention": { "type": "boolean", "description": "Whether the bot must be mentioned to respond (default: true)" },
                    "allow_from": { "type": "array", "items": { "type": "string" }, "description": "User IDs allowed in this channel (empty = everyone)" }
                }
            })),
            tool("remove_channel", "Admin only. Remove a channel from dione's config. Only execute when requested by a user in the admins list.", json!({
                "type": "object",
                "required": ["id"],
                "properties": {
                    "id": { "type": "string", "description": "Discord channel ID" }
                }
            })),
            tool("update_channel", "Admin only. Update settings for a channel in dione's config. Only execute when requested by a user in the admins list.", json!({
                "type": "object",
                "required": ["id"],
                "properties": {
                    "id": { "type": "string", "description": "Discord channel ID" },
                    "require_mention": { "type": "boolean", "description": "Whether the bot must be mentioned to respond" },
                    "allow_from": { "type": "array", "items": { "type": "string" }, "description": "User IDs allowed in this channel (empty = everyone)" }
                }
            })),
            tool("update_dm_policy", "Admin only. Update the DM policy in dione's config. Only execute when requested by a user in the admins list.", json!({
                "type": "object",
                "required": ["policy"],
                "properties": {
                    "policy": { "type": "string", "enum": ["queue", "drop", "disabled"], "description": "DM handling policy" }
                }
            })),
            tool("add_allow_from", "Admin only. Add a user ID to the global allow_from list. Only execute when requested by a user in the admins list.", json!({
                "type": "object",
                "required": ["user_id"],
                "properties": {
                    "user_id": { "type": "string", "description": "Discord user ID to allow" }
                }
            })),
            tool("remove_allow_from", "Admin only. Remove a user ID from the global allow_from list. Only execute when requested by a user in the admins list.", json!({
                "type": "object",
                "required": ["user_id"],
                "properties": {
                    "user_id": { "type": "string", "description": "Discord user ID to remove" }
                }
            })),
        ]
    });
    if mode == TransportMode::Codex
        && let Some(tools) = response["tools"].as_array_mut()
    {
        tools.extend([
            tool("bind_codex_thread", "Bind live inbound Discord delivery to this exact Codex thread. Call once at startup and after resuming, forking, or switching conversations. Future events move to the new binding; old backlog is not replayed.", json!({
                "type": "object",
                "required": ["thread_id"],
                "properties": {
                    "thread_id": { "type": "string", "minLength": 1, "maxLength": 128, "description": "Exact current CODEX_THREAD_ID; Dione never guesses among loaded threads" }
                }
            })),
            tool("register_event_consumer", "Register this Codex conversation as an event consumer. A consumer may become primary only when no live primary exists; use handoff_event_consumer to switch deliberately.", json!({
                "type": "object",
                "required": ["label"],
                "properties": {
                    "label": { "type": "string", "minLength": 1, "maxLength": 120, "description": "Human-readable conversation label; this is not an authorization token" },
                    "ttl_seconds": { "type": "integer", "minimum": 60, "maximum": 86400, "default": 900 },
                    "make_primary": { "type": "boolean", "default": false, "description": "Become the primary consumer only if no live primary exists" },
                    "claim_unassigned": { "type": "boolean", "default": false, "description": "When becoming primary, route currently unassigned events here" }
                }
            })),
            tool("handoff_event_consumer", "Explicitly transfer future inbound delivery from the active primary Codex conversation to another registered consumer.", json!({
                "type": "object",
                "required": ["from_consumer_id", "to_consumer_id"],
                "properties": {
                    "from_consumer_id": { "type": "string", "description": "Current primary consumer id" },
                    "to_consumer_id": { "type": "string", "description": "Registered destination consumer id" },
                    "move_pending": { "type": "boolean", "default": false, "description": "Also move pending events and invalidate their active leases; false routes only future events" }
                }
            })),
            tool("claim_event_consumer", "Make a registered consumer primary after the previous primary has expired or released ownership. Fails while a live primary exists.", json!({
                "type": "object",
                "required": ["consumer_id"],
                "properties": {
                    "consumer_id": { "type": "string", "description": "Registered consumer becoming primary" },
                    "claim_orphaned": { "type": "boolean", "default": false, "description": "Route unassigned events and events owned by expired consumers here, invalidating their leases" }
                }
            })),
            tool("next_event", "Wait for and lease the next structured Discord event routed to this registered consumer. Call ack_event after handling it, then call next_event again.", json!({
                "type": "object",
                "required": ["consumer_id"],
                "properties": {
                    "consumer_id": { "type": "string", "description": "Opaque id returned by register_event_consumer" },
                    "wait_seconds": { "type": "integer", "minimum": 0, "maximum": 55, "default": 45 },
                    "lease_seconds": { "type": "integer", "minimum": 1, "maximum": 3600, "default": 120 }
                }
            })),
            tool("ack_event", "Acknowledge a leased Discord event after it has been handled successfully.", json!({
                "type": "object",
                "required": ["consumer_id", "delivery_token"],
                "properties": {
                    "consumer_id": { "type": "string", "description": "Consumer that owns the lease" },
                    "delivery_token": { "type": "string", "description": "Opaque token returned by next_event" }
                }
            })),
            tool("event_queue_status", "Inspect the durable Codex event queue without leasing an event.", json!({
                "type": "object",
                "properties": {}
            })),
        ]);
    }
    response
}

pub(crate) fn tool(name: &str, description: &str, input_schema: Value) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": input_schema,
    })
}

fn embed_schema() -> Value {
    let field_schema = json!({
        "type": "object",
        "required": ["name", "value"],
        "properties": {
            "name": { "type": "string" },
            "value": { "type": "string" },
            "inline": { "type": "boolean", "default": false }
        }
    });
    let item_schema = json!({
        "type": "object",
        "properties": {
            "title": { "type": "string" },
            "description": { "type": "string" },
            "url": { "type": "string" },
            "color": { "description": "Embed color as integer or hex string (e.g. 3447003, \"#3498DB\", \"FF0000\")" },
            "timestamp": { "type": "string", "description": "ISO 8601 timestamp" },
            "footer": { "description": "Footer text (string) or object with text and optional icon_url" },
            "author": { "description": "Author name (string) or object with name, optional url and icon_url" },
            "thumbnail": { "description": "Thumbnail URL (string) or object with url" },
            "image": { "description": "Image URL (string) or object with url" },
            "fields": { "type": "array", "items": field_schema }
        }
    });
    json!({
        "type": "array",
        "description": "Optional array of Discord embed objects (max 10)",
        "items": item_schema
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fetch_messages_schema() -> Value {
        let list = tools_list(TransportMode::ClaudeCode);
        list["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "fetch_messages")
            .expect("fetch_messages must be in tools list")
            .clone()
    }

    #[test]
    fn fetch_messages_schema_has_cursor_fields() {
        let tool = fetch_messages_schema();
        let props = &tool["inputSchema"]["properties"];
        assert!(
            props.get("before").is_some(),
            "schema must include 'before'"
        );
        assert!(props.get("after").is_some(), "schema must include 'after'");
    }

    #[test]
    fn fetch_messages_schema_documents_mutual_exclusion() {
        let tool = fetch_messages_schema();
        let desc = tool["description"].as_str().unwrap();
        assert!(
            desc.contains("mutually exclusive") || desc.contains("Mutually exclusive"),
            "tool description must document mutual exclusion of before/after"
        );
    }

    #[test]
    fn fetch_messages_schema_documents_ordering() {
        let tool = fetch_messages_schema();
        let desc = tool["description"].as_str().unwrap();
        assert!(
            desc.contains("oldest-first"),
            "tool description must document oldest-first ordering"
        );
    }

    #[test]
    fn fetch_messages_schema_cursor_descriptions_mention_exclusivity() {
        let tool = fetch_messages_schema();
        let props = &tool["inputSchema"]["properties"];
        let before_desc = props["before"]["description"].as_str().unwrap();
        let after_desc = props["after"]["description"].as_str().unwrap();
        assert!(
            before_desc.contains("Mutually exclusive"),
            "before description must mention mutual exclusivity"
        );
        assert!(
            after_desc.contains("Mutually exclusive"),
            "after description must mention mutual exclusivity"
        );
    }
}
