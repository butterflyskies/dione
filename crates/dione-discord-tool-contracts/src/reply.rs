//! Reply discovery and borrowed request metadata with explicit evidence settings.
use dione_discord_messaging_contracts::{LinkPreviews, ReplyPing};
use dione_discord_tool_arguments::{
    ArgumentError, ArgumentErrorKind, parse_enabled_sentex_handles, parse_hook_overrides, parse_id,
    parse_optional_bool, parse_optional_id, sentex_handles_schema,
};
use dione_evidence::SentexHandles;
use dione_pre_send::HookName;
use serde_json::{Value, json};
use serenity::model::id::{ChannelId, MessageId};
/// The existing reply discovery description.
pub const DESCRIPTION: &str = "Send a reply to a Discord channel or DM. A contradictionary block-tier match does not send: the message is held under a single-use handle and the error names the matched pattern(s) plus the handle. Act on it with no_rly (send verbatim), rephrase (replacement, re-checked), or ignore it to let it expire. An auto-tier match is rewritten to its replacement and sent; the result lists each rewrite under auto_rewrites (`auto: <match> → <replace>`).";

/// Projects reply discovery using explicit current evidence settings.
pub fn definition(
    evidence_markers_enabled: bool,
    version: dione_evidence::EvidenceMarkerVersion,
) -> Value {
    let mut definition = crate::tool(
        "reply",
        DESCRIPTION,
        json!({
            "type": "object",
            "required": ["channel_id", "content"],
            "properties": {
                "suppress_embeds": { "type": "boolean", "description": "Suppress Discord link previews (default: false)" },
                "channel_id": { "type": "string", "description": "Discord channel ID" },
                "content": { "type": "string", "description": "Message content" },
                "reply_to_message_id": { "type": "string", "description": "Optional message ID to reply to" },
                "suppress_ping": { "type": "boolean", "description": "When true, the reply will not ping the user being replied to (default: false)" },
                "no_rly_hooks": { "type": "array", "items": { "type": "string" }, "description": "Names individual pre-send hooks to bypass for this send. Every bypass is audited. This does not bypass the contradictionary; use the no_rly(handle) tool for that." },
                "claim_handles": sentex_handles_schema("Claims asserted by this message", version),
                "citation_handles": sentex_handles_schema("Claims cited as support or context", version)
            }
        }),
    );
    if !evidence_markers_enabled
        && let Some(properties) = definition["inputSchema"]["properties"].as_object_mut()
    {
        properties.remove("claim_handles");
        properties.remove("citation_handles");
    }
    definition
}

/// Descriptive ReplyRequest metadata; construction grants no execution authority.
pub struct ReplyRequest<'a> {
    /// The decoded channel.
    pub channel: ChannelId,
    /// The decoded content.
    pub content: &'a str,
    /// The decoded reply to.
    pub reply_to: Option<MessageId>,
    /// The decoded ping.
    pub ping: ReplyPing,
    /// The decoded previews.
    pub previews: LinkPreviews,
    /// The decoded hooks.
    pub hooks: Vec<HookName>,
    /// The decoded handles.
    pub handles: SentexHandles,
}
impl<'a> ReplyRequest<'a> {
    /// Decodes metadata without checking access or executing an operation.
    pub fn parse(
        arguments: &'a Value,
        evidence_enabled: bool,
        version: dione_evidence::EvidenceMarkerVersion,
    ) -> Result<Self, ArgumentError> {
        let channel = parse_id(arguments, "channel_id")?.channel();
        let content = arguments
            .get("content")
            .and_then(Value::as_str)
            .ok_or(ArgumentErrorKind::Missing("content"))?;
        let reply_to = parse_optional_id(arguments, "reply_to_message_id")?
            .map(dione_discord_identities::Snowflake::message);
        let ping = ReplyPing::from_suppressed(
            arguments
                .get("suppress_ping")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        );
        let hooks = parse_hook_overrides(arguments)?;
        let handles = parse_enabled_sentex_handles(arguments, evidence_enabled, version)?;
        let previews = LinkPreviews::from_suppressed(
            parse_optional_bool(arguments, "suppress_embeds")?.unwrap_or(false),
        );
        Ok(Self {
            channel,
            content,
            reply_to,
            ping,
            previews,
            hooks,
            handles,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;
    use {dione_evidence::SentexError, dione_pre_send::PipelineError};

    #[test]
    fn selected_evidence_versions_do_not_bypass_the_disabled_handle_gate() {
        for version in [
            dione_evidence::EvidenceMarkerVersion::V2,
            dione_evidence::EvidenceMarkerVersion::V3,
        ] {
            for field in ["claim_handles", "citation_handles"] {
                let args = json!({"channel_id":"42", "content":"text", field:["12"]});
                assert!(ReplyRequest::parse(&args, true, version).is_ok());
                let error = ReplyRequest::parse(&args, false, version).err().unwrap();
                assert_eq!(
                    error.to_string(),
                    "claim_handles and citation_handles require evidence_markers_enabled"
                );
            }
            let args = json!({"channel_id":"42", "content":"text"});
            assert!(
                ReplyRequest::parse(&args, false, version)
                    .unwrap()
                    .handles
                    .is_empty()
            );
        }
    }

    #[test]
    fn reply_evidence_errors_preserve_typed_parser_cause() {
        let invalid =
            json!({"channel_id":"42", "content":"text", "claim_handles":["18446744073709551616"]});
        let error = ReplyRequest::parse(&invalid, true, dione_evidence::EvidenceMarkerVersion::V3)
            .err()
            .unwrap();
        assert_eq!(
            error.to_string(),
            "claim_handles must contain canonical positive decimal u64 strings"
        );
        let parser_error = error
            .source()
            .unwrap()
            .source()
            .unwrap()
            .downcast_ref::<SentexError>()
            .expect("original evidence parser cause");
        let mut cause = parser_error.source();
        let mut found_number = false;
        while let Some(source) = cause {
            found_number |= source.is::<std::num::ParseIntError>();
            cause = source.source();
        }
        assert!(
            found_number,
            "out-of-range numeric cause must survive the reply boundary"
        );
        let valid = json!({"channel_id":"42", "content":"text", "claim_handles":["18446744073709551615"], "citation_handles":["1"]});
        let request =
            ReplyRequest::parse(&valid, true, dione_evidence::EvidenceMarkerVersion::V3).unwrap();
        assert_eq!(
            dione_evidence::append_markers(
                request.content,
                &request.handles,
                dione_evidence::EvidenceMarkerVersion::V3
            ),
            "text [🔍=v3:claim:__________8] [🔍=v3:citation:AQ]"
        );
    }

    #[test]
    fn reply_argument_errors_retain_causes_and_historical_optional_behavior() {
        let bad = json!({"channel_id":"bad", "content":"text"});
        let error = ReplyRequest::parse(&bad, true, dione_evidence::EvidenceMarkerVersion::V3)
            .err()
            .unwrap();
        assert_eq!(error.to_string(), "invalid channel_id: not a valid u64");
        assert!(
            error
                .source()
                .unwrap()
                .source()
                .unwrap()
                .is::<std::num::ParseIntError>()
        );
        let bad_hook = json!({"channel_id":"42", "content":"text", "no_rly_hooks":["bad hook"]});
        let hook_error =
            ReplyRequest::parse(&bad_hook, true, dione_evidence::EvidenceMarkerVersion::V3)
                .err()
                .unwrap();
        assert_eq!(
            hook_error.to_string(),
            "invalid pre-send hook name `bad hook`"
        );
        let pipeline_error = hook_error
            .source()
            .unwrap()
            .source()
            .expect("invalid hook must retain its PipelineError source")
            .downcast_ref::<PipelineError>()
            .expect("invalid hook source must be PipelineError");
        assert_eq!(
            pipeline_error.to_string(),
            "invalid pre-send hook name `bad hook`"
        );
        let good = json!({"channel_id":"42", "content":"text", "reply_to_message_id":{}, "suppress_ping":"legacy ignored", "suppress_embeds":true, "no_rly_hooks":["valid-hook"]});
        let parsed =
            ReplyRequest::parse(&good, true, dione_evidence::EvidenceMarkerVersion::V3).unwrap();
        assert_eq!(parsed.channel, ChannelId::new(42));
        assert_eq!(parsed.content, "text");
        assert_eq!(parsed.reply_to, None);
        assert_eq!(parsed.hooks.len(), 1);
        assert_eq!(parsed.hooks[0].as_str(), "valid-hook");
        let bad_bool = json!({"channel_id":42,"content":"text","suppress_embeds":"true"});
        assert_eq!(
            ReplyRequest::parse(&bad_bool, true, dione_evidence::EvidenceMarkerVersion::V3)
                .err()
                .unwrap()
                .to_string(),
            "suppress_embeds must be a boolean"
        );
    }
}
