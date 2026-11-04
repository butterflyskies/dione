//! Principal-bound tool dispatcher for context reads; authentication is not a JSON argument.

use super::{
    context::{
        ContextDirection, ContextError, ContextErrorKind, ContextQuery, ContextRequest,
        ContextService, PageLimit,
    },
    model::AuthenticatedRecipient,
    observation::{ConversationKey, EventKey, MessageKey},
};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExactArguments {
    event: EventKey,
    target: MessageKey,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryArguments {
    event: EventKey,
    room: ConversationKey,
    direction: ContextDirection,
    limit: PageLimit,
}
/// A context tool surface permanently bound to one authenticated connection identity.
///
/// Trusted HTTP/MCP embedding code constructs this value after authenticating the
/// caller. Neither event content nor tool arguments can replace its principal.
/// This surface exposes no resident reply, configuration, replay or filesystem tools.
#[derive(Clone)]
pub struct ContextTools {
    recipient: AuthenticatedRecipient,
    service: ContextService,
}
impl ContextTools {
    /// Bind an already authenticated connection to its narrow context service.
    pub fn new(recipient: AuthenticatedRecipient, service: ContextService) -> Self {
        Self { recipient, service }
    }
    /// JSON tool definitions with explicit coordinate, direction and size schemas.
    pub fn definitions(&self) -> Vec<Value> {
        let capabilities = self.service.capabilities();
        let mut definitions = Vec::new();
        let defs = identity_definitions();
        if capabilities.exact {
            definitions.push(json!({"name":"rook_get_message","description":"Read the current original by exact ID within an admitted event conversation. Returns authoritative common and native metadata, not the event preview.","inputSchema":{
                "type":"object","additionalProperties":false,"required":["event","target"],"properties":{"event":{"$ref":"#/$defs/event"},"target":{"$ref":"#/$defs/message"}},"$defs":defs.clone()
            }}));
        }
        if capabilities.history && capabilities.max_page_size > 0 {
            definitions.push(json!({"name":"rook_get_context","description":"Read an oldest-first page in the admitted event conversation. Before/after anchors are exclusive; has_more_hint is not a completeness guarantee.","inputSchema":{
                "type":"object","additionalProperties":false,"required":["event","room","direction","limit"],"properties":{"event":{"$ref":"#/$defs/event"},"room":{"$ref":"#/$defs/room"},"direction":{"$ref":"#/$defs/direction"},"limit":{"type":"integer","minimum":1,"maximum":capabilities.max_page_size.min(100)}},"$defs":defs
            }}));
        }
        definitions
    }
    /// Parse a typed request and execute it under this connection's fixed recipient.
    ///
    /// A future authenticated MCP transport can wrap this result in its protocol
    /// envelope; it cannot bypass the service's owner and current-access checks.
    pub async fn call(&self, name: &str, arguments: Value) -> Result<Value, ContextError> {
        let request = match name {
            "rook_get_message" => {
                let args: ExactArguments =
                    serde_path_to_error::deserialize(arguments).map_err(|error| {
                        ContextError::with_cause(ContextErrorKind::InvalidArguments, error)
                    })?;
                ContextRequest {
                    event: args.event,
                    query: ContextQuery::Exact(args.target),
                }
            }
            "rook_get_context" => {
                let args: HistoryArguments =
                    serde_path_to_error::deserialize(arguments).map_err(|error| {
                        ContextError::with_cause(ContextErrorKind::InvalidArguments, error)
                    })?;
                ContextRequest {
                    event: args.event,
                    query: ContextQuery::History {
                        room: args.room,
                        direction: args.direction,
                        limit: args.limit,
                    },
                }
            }
            _ => return Err(ContextError::new(ContextErrorKind::UnknownTool)),
        };
        let response = self.service.read(self.recipient.clone(), request).await?;
        serde_json::to_value(response)
            .map_err(|error| ContextError::with_cause(ContextErrorKind::Provider, error))
    }
}

pub(super) fn identity_definitions() -> Value {
    json!({
        "source": {"type":"object","additionalProperties":false,"required":["provider","slot"],"properties":{"provider":{"type":"string","minLength":1},"slot":{"type":"string","minLength":1}}},
        "local": {"oneOf":[
            {"type":"object","additionalProperties":false,"required":["kind","value"],"properties":{"kind":{"const":"numeric"},"value":{"type":"integer","minimum":1}}},
            {"type":"object","additionalProperties":false,"required":["kind","value"],"properties":{"kind":{"const":"opaque"},"value":{"type":"string","minLength":1}}}
        ]},
        "room": {"type":"object","additionalProperties":false,"required":["source","local"],"properties":{"source":{"$ref":"#/$defs/source"},"local":{"$ref":"#/$defs/local"}}},
        "message": {"type":"object","additionalProperties":false,"required":["conversation","local"],"properties":{"conversation":{"$ref":"#/$defs/room"},"local":{"$ref":"#/$defs/local"}}},
        "event": {"type":"object","additionalProperties":false,"required":["message","occurrence"],"properties":{"message":{"$ref":"#/$defs/message"},"occurrence":{"oneOf":[
            {"type":"object","additionalProperties":false,"required":["kind"],"properties":{"kind":{"const":"created"}}},
            {"type":"object","additionalProperties":false,"required":["kind","revision"],"properties":{"kind":{"const":"edited"},"revision":{"type":"string","format":"date-time"}}}
        ]}}},
        "direction": {"oneOf":[
            {"type":"object","additionalProperties":false,"required":["kind"],"properties":{"kind":{"const":"recent"}}},
            {"type":"object","additionalProperties":false,"required":["kind","anchor"],"properties":{"kind":{"enum":["before","after"]},"anchor":{"$ref":"#/$defs/message"}}}
        ]}
    })
}
