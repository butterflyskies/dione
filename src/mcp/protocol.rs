//! MCP protocol responses: initialize handshake and tool list.

use crate::{
    discord::{
        presence::{ActivityType, OnlineStatus},
        tool_arguments::sentex_handles_schema,
    },
    harness::HarnessConnection,
};
use serde_json::{Value, json};

const LATEST_PROTOCOL_VERSION: &str = "2025-11-25";
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &[
    LATEST_PROTOCOL_VERSION,
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
];

/// Build the MCP `initialize` response.
pub(crate) fn initialize_response(
    harness: &HarnessConnection,
    requested_protocol_version: Option<&str>,
) -> Value {
    let protocol_version = requested_protocol_version
        .filter(|requested| SUPPORTED_PROTOCOL_VERSIONS.contains(requested))
        .unwrap_or(LATEST_PROTOCOL_VERSION);
    let capabilities = harness.protocol().capabilities();
    json!({
        "protocolVersion": protocol_version,
        "capabilities": capabilities,
        "serverInfo": {
            "name": "dione",
            "version": env!("CARGO_PKG_VERSION"),
        }
    })
}

/// Build the MCP `tools/list` response.
pub(crate) fn tools_list(harness: &HarnessConnection, evidence_markers_enabled: bool) -> Value {
    let mut response = json!({
        "tools": [
            tool("attention", "Recipient-local attention controls: status/configure, authorized source review/retrieval and feedback, offline replay/fitting, held-out evaluation and explicit digest promotion/rollback. Configure replaces settings from status. No command is accepted from Discord message text; this authenticated seat supplies recipient identity.", crate::attention::control::schema()),
            tool("no_rly", "Release a message held by the contradictionary: sends the byte-identical held text with its original addressing. Handles are single-use — they die on release, rephrase, or expiry (default 3 minutes) and cannot be replayed; only a failed send leaves the handle live for a retry. Ordering: the message lands when released, not at its original position in the conversation.", json!({
                "type": "object",
                "required": ["handle"],
                "properties": {
                    "handle": { "type": "string", "description": "The hold handle returned by the bounced send (e.g. nr-3f92-7)" }
                }
            })),
            tool("rephrase", "Replace a held message with new text. The replacement is re-checked by the contradictionary: a clean verdict sends it (original addressing preserved) and journals the (original, reason, replacement) triple; a re-bounce kills the old handle and returns a NEW handle chained to it. An auto-tier match in the replacement is rewritten before the check, and the result lists each rewrite under auto_rewrites (`auto: <match> → <replace>`); if the send fails, the handle holds the replacement as written. Like no_rly, the sent text lands when released, not at its original conversation position.", json!({
                "type": "object",
                "required": ["handle", "content"],
                "properties": {
                    "handle": { "type": "string", "description": "The hold handle returned by the bounced send" },
                    "content": { "type": "string", "description": "Replacement message text" }
                }
            })),
            tool("no_rly_stats", "Query the no_rly audit journal: bounce counts by outcome (released/rephrased/expired) and by pattern, bounce-to-action latency (min/p50/mean/max), rephrase chains, plus validation counters (dangling chain parents, malformed lines) and the live held-message count.", json!({
                "type": "object",
                "properties": {
                    "since_days": { "type": "integer", "minimum": 0, "description": "Only bounces resolved within the last N days" },
                    "outcome": { "type": "string", "enum": ["released", "rephrased", "expired"], "description": "Only bounces with this outcome" },
                    "pattern": { "type": "string", "description": "Only bounces whose reason includes this pattern" }
                }
            })),
            tool("no_rly_condense", "Fold raw no_rly journal records older than the cutoff into per-day/per-reason/per-outcome summaries (counts + latency aggregates). Message text and chain links are dropped for condensed records — that is the trade. Atomic rewrite; malformed lines are preserved (vacuum drops those).", json!({
                "type": "object",
                "properties": {
                    "older_than_days": { "type": "integer", "minimum": 0, "description": "Condense records resolved more than N days ago (default: configured journal_raw_retention_days, 365)" }
                }
            })),
            tool("no_rly_vacuum", "Compact the no_rly journal: drop summary records older than the cutoff and any malformed lines, then rewrite the file atomically. Raw bounce records are never dropped by vacuum — condense them first. Reports bytes before/after.", json!({
                "type": "object",
                "properties": {
                    "older_than_days": { "type": "integer", "minimum": 0, "description": "Drop summaries older than N days (default: configured journal_summary_retention_days, 730)" }
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
                    "suppress_embeds": { "type": "boolean", "description": "Suppress Discord link previews; false restores previews, omission preserves the current setting" },
                    "channel_id": { "type": "string" },
                    "message_id": { "type": "string" },
                    "content": { "type": "string" },
                    "no_rly_hooks": { "type": "array", "items": { "type": "string" }, "description": "Names individual pre-send hooks to bypass; every bypass is audited." }
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
            tool("send_dm", "Initiate a DM conversation with a Discord user and send a message. Contradictionary matches gate it as on reply: a block-tier match holds it under a handle, and an auto-tier match is rewritten and sent, with each rewrite listed under auto_rewrites (`auto: <match> → <replace>`).", json!({
                "type": "object",
                "required": ["user_id", "content"],
                "properties": {
                    "suppress_embeds": { "type": "boolean", "description": "Suppress Discord link previews (default: false)" },
                    "user_id": { "type": "string", "description": "Discord user ID to send the DM to" },
                    "content": { "type": "string", "description": "Message content to send" },
                    "no_rly_hooks": { "type": "array", "items": { "type": "string" }, "description": "Names individual pre-send hooks to bypass; every bypass is audited." },
                    "claim_handles": sentex_handles_schema("Claims asserted by this message"),
                    "citation_handles": sentex_handles_schema("Claims cited as support or context")
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
            tool("get_presence", "Read back the last presence this process requested (what set_presence stored and replays on reconnect) — not a live Discord read-back", json!({
                "type": "object",
                "properties": {}
            })),
            tool("set_presence", "Set the bot's Discord presence (online status and activity)", json!({
                "type": "object",
                "properties": {
                    "online_status": {
                        "type": "string",
                        "enum": OnlineStatus::ALL.iter().map(ToString::to_string).collect::<Vec<_>>(),
                        "description": "Online status (default: online)"
                    },
                    "activity_type": {
                        "type": "string",
                        "enum": ActivityType::ALL.iter().map(ToString::to_string).collect::<Vec<_>>(),
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
            tool("list_config_channels", "List all channels in dione's config with their settings. Active guild mutes are listed in a separate guild_mutes array because channel-to-guild mapping requires Discord event context and is not available at config-list time.", json!({
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
            tool("mute_server", "Admin only. Suspend push delivery for all channels in a guild. Outbound (reply, send_file) and manual pull (fetch_messages, search_messages) remain available. Re-issuing a mute with a shorter TTL is rejected to prevent accidental shortening; unmute first to replace.", json!({
                "type": "object",
                "required": ["guild_id", "ttl_minutes"],
                "properties": {
                    "guild_id": { "type": "string", "description": "Discord guild (server) ID to mute" },
                    "ttl_minutes": { "type": "integer", "minimum": 1, "maximum": 43200, "description": "Duration of mute in minutes (max 30 days)" },
                    "reason": { "type": "string", "description": "Optional reason for the mute" }
                }
            })),
            tool("unmute_server", "Admin only. Manually unmute a guild, resuming push delivery.", json!({
                "type": "object",
                "required": ["guild_id"],
                "properties": {
                    "guild_id": { "type": "string", "description": "Discord guild (server) ID to unmute" }
                }
            })),
            tool("list_muted_servers", "Admin only. List all currently muted guilds with remaining TTL and metadata.", json!({
                "type": "object",
                "properties": {}
            })),
        ]
    });
    if let Some(tools) = response["tools"].as_array_mut() {
        tools.extend(harness.protocol().tools());
    }
    if !evidence_markers_enabled && let Some(tools) = response["tools"].as_array_mut() {
        for tool in tools {
            if matches!(tool["name"].as_str(), Some("send_dm"))
                && let Some(properties) = tool["inputSchema"]["properties"].as_object_mut()
            {
                properties.remove("claim_handles");
                properties.remove("citation_handles");
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn fetch_messages_schema() -> Value {
        crate::discord::query_tools::definitions()
            .find(|tool| tool["name"] == "fetch_messages")
            .expect("fetch_messages must be in owner definitions")
    }

    /// Boundary pin for the presence tools: deleting either from the
    /// advertised list must fail here, not in a reviewer's client.
    #[test]
    fn presence_tools_are_listed_with_their_schemas() {
        let list = tools_list(&HarnessConnection::claude(), false);
        let tools = list["tools"].as_array().unwrap();
        let set = tools
            .iter()
            .find(|t| t["name"] == "set_presence")
            .expect("set_presence must be in tools list");
        for field in ["online_status", "activity_type", "activity_name"] {
            assert!(
                set["inputSchema"]["properties"].get(field).is_some(),
                "set_presence schema must carry {field}"
            );
        }
        assert!(
            tools.iter().any(|t| t["name"] == "get_presence"),
            "get_presence must be in tools list"
        );
    }

    #[test]
    fn claim_and_citation_handle_schemas_are_exposed_only_when_enabled() {
        for enabled in [false, true] {
            let (_queue_dir, harness) = crate::mcp::server::test_codex_connection();
            let list = tools_list(&harness, enabled);
            for name in ["send_dm"] {
                let tool = list["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|tool| tool["name"] == name)
                    .unwrap();
                let properties = &tool["inputSchema"]["properties"];
                for field in ["claim_handles", "citation_handles"] {
                    assert_eq!(
                        properties.get(field).is_some(),
                        enabled,
                        "{name} {field} schema must follow the config gate"
                    );
                }
                assert!(
                    properties.get("evidence_keys").is_none(),
                    "{name} must not advertise the removed evidence_keys alias"
                );
            }
        }
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
