//! Durable Codex delivery and MCP extension.
use super::{AttentionPolicy, HarnessConnection, HarnessDelivery, HarnessError, ProtocolOutput};
use crate::{
    codex::{
        CodexBindingController, CodexEventQueue, CodexThreadId, ConsumerId, DeliveryToken,
        consumer_ttl, lease_duration, timeout_response, wait_duration,
    },
    core::{MessageRef, delivery::Delivery},
    durable::EndpointId,
    mcp::{harness_wire::HarnessProtocol, protocol::tool},
};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use thiserror::Error;
use tokio::sync::watch;

pub(super) const ATTENTION_POLICY: AttentionPolicy = AttentionPolicy::SafeTurn;

struct CodexAdapter {
    queue: CodexEventQueue,
    controller: CodexBindingController,
    binding: watch::Sender<Option<EndpointId>>,
}
impl HarnessConnection {
    /// Connects the queue and owner-issued binding control without claiming a consumer.
    pub fn codex(queue: CodexEventQueue, binding: watch::Sender<Option<EndpointId>>) -> Self {
        let controller = queue.binding_controller();
        Self::from_adapter(
            CodexAdapter {
                queue,
                controller,
                binding,
            },
            ProtocolOutput::new(tokio::io::stdout()),
        )
    }
}
impl HarnessDelivery for CodexAdapter {
    fn attention_policy(&self) -> AttentionPolicy {
        ATTENTION_POLICY
    }
    fn deliver<'a>(&'a self, value: &'a Delivery) -> BoxFuture<'a, Result<(), HarnessError>> {
        Box::pin(async move {
            self.queue
                .enqueue(
                    crate::codex::legacy_delivery::encode(value, self.queue.legacy_scope())
                        .map_err(HarnessError::source)?,
                )
                .await
                .map(|_| ())
                .map_err(|error| {
                    let completion_unknown = match &error.kind {
                        crate::codex::CodexQueueErrorKind::Durable(error) => match &error.kind {
                            crate::durable::QueueErrorKind::CompletionUnknown(_) => true,
                            crate::durable::QueueErrorKind::Io(_)
                            | crate::durable::QueueErrorKind::Locked(_)
                            | crate::durable::QueueErrorKind::Codec(_)
                            | crate::durable::QueueErrorKind::PriorSync(_)
                            | crate::durable::QueueErrorKind::DurabilityUncertain(_)
                            | crate::durable::QueueErrorKind::Identity(_)
                            | crate::durable::QueueErrorKind::Integrity(_)
                            | crate::durable::QueueErrorKind::AllocationExhausted { .. }
                            | crate::durable::QueueErrorKind::InvalidStorageLayout
                            | crate::durable::QueueErrorKind::UnknownConsumer
                            | crate::durable::QueueErrorKind::NotPrimaryConsumer
                            | crate::durable::QueueErrorKind::PrimaryConsumerExists
                            | crate::durable::QueueErrorKind::UnknownDeliveryToken
                            | crate::durable::QueueErrorKind::NotDeferrable
                            | crate::durable::QueueErrorKind::ResidentUnavailable
                            | crate::durable::QueueErrorKind::ReplayCapacity => false,
                        },
                        crate::codex::CodexQueueErrorKind::LegacyAttention(_)
                        | crate::codex::CodexQueueErrorKind::InvalidThreadId => false,
                    };
                    if completion_unknown {
                        HarnessError::completion_unknown(error)
                    } else {
                        HarnessError::source(error)
                    }
                })
        })
    }
    fn invalidate_attention(&self, source: MessageRef) -> BoxFuture<'_, Result<(), HarnessError>> {
        Box::pin(async move {
            self.queue
                .invalidate_attention(crate::codex::AttentionInvalidation::Source(&source))
                .await
                .map(|_| ())
                .map_err(HarnessError::source)
        })
    }
}
impl HarnessProtocol for CodexAdapter {
    fn capabilities(&self) -> Value {
        json!({"tools": {}})
    }
    fn resident_control(&self) -> bool {
        true
    }
    fn permission_relay(&self) -> bool {
        false
    }
    fn tools(&self) -> Vec<Value> {
        [
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
        ].into()
    }
    fn call<'a>(
        &'a self,
        name: &'a str,
        args: &'a Value,
    ) -> BoxFuture<'a, Option<Result<Value, HarnessError>>> {
        Box::pin(async move {
            if recognizes(name) {
                Some(self.call_tool(name, args).await)
            } else {
                None
            }
        })
    }
}
impl CodexAdapter {
    async fn call_tool(&self, name: &str, args: &Value) -> Result<Value, HarnessError> {
        Ok(match name {
            "bind_codex_thread" => {
                let binding = &self.binding;
                let thread_id = CodexThreadId::parse(parse_str(args, "thread_id")?)
                    .map_err(HarnessError::source)?;
                self.controller
                    .bind_live_thread_and_publish(Some(thread_id.clone()), binding)
                    .await
                    .map_err(HarnessError::source)?;
                if binding.receiver_count() == 0 {
                    return Err(HarnessError::source(ToolArgumentError(
                        "Codex live delivery worker is unavailable".to_string(),
                    )));
                }
                json!({ "bound": true, "thread_id": thread_id })
            }
            "next_event" => {
                let queue = &self.queue;
                let consumer_id = ConsumerId::parse(parse_str(args, "consumer_id")?)
                    .map_err(HarnessError::source)?;
                let wait = wait_duration(args.get("wait_seconds").and_then(Value::as_u64));
                let lease = lease_duration(args.get("lease_seconds").and_then(Value::as_u64));
                match queue
                    .next_event(&consumer_id, wait, lease)
                    .await
                    .map_err(HarnessError::source)?
                {
                    Some(event) => serde_json::to_value(event).map_err(|source| {
                        HarnessError::source(SerializationError {
                            context: "failed to serialize leased event: ",
                            source,
                        })
                    })?,
                    None => timeout_response(),
                }
            }
            "ack_event" => {
                let queue = &self.queue;
                let consumer_id = ConsumerId::parse(parse_str(args, "consumer_id")?)
                    .map_err(HarnessError::source)?;
                let raw_token = parse_str(args, "delivery_token")?;
                let token = DeliveryToken::parse(raw_token).map_err(HarnessError::source)?;
                queue
                    .acknowledge(&consumer_id, &token)
                    .await
                    .map_err(HarnessError::source)?;
                json!({ "acknowledged": true })
            }
            "event_queue_status" => {
                let queue = &self.queue;
                serde_json::to_value(queue.status().await).map_err(|source| {
                    HarnessError::source(SerializationError {
                        context: "failed to serialize queue status: ",
                        source,
                    })
                })?
            }
            "register_event_consumer" => {
                let queue = &self.queue;
                let label = parse_str(args, "label")?.trim();
                if label.is_empty() || label.len() > 120 {
                    return Err(HarnessError::source(ToolArgumentError(
                        "label must contain 1 to 120 characters".to_string(),
                    )));
                }
                let ttl = consumer_ttl(args.get("ttl_seconds").and_then(Value::as_u64));
                let make_primary = args
                    .get("make_primary")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let claim_unassigned = args
                    .get("claim_unassigned")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let registration = queue
                    .register_consumer(label.to_owned(), ttl, make_primary, claim_unassigned)
                    .await
                    .map_err(HarnessError::source)?;
                serde_json::to_value(registration).map_err(|source| {
                    HarnessError::source(SerializationError {
                        context: "failed to serialize consumer registration: ",
                        source,
                    })
                })?
            }
            "handoff_event_consumer" => {
                let queue = &self.queue;
                let from = ConsumerId::parse(parse_str(args, "from_consumer_id")?)
                    .map_err(HarnessError::source)?;
                let to = ConsumerId::parse(parse_str(args, "to_consumer_id")?)
                    .map_err(HarnessError::source)?;
                let move_pending = args
                    .get("move_pending")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let result = queue
                    .handoff(&from, &to, move_pending)
                    .await
                    .map_err(HarnessError::source)?;
                serde_json::to_value(result).map_err(|source| {
                    HarnessError::source(SerializationError {
                        context: "failed to serialize consumer handoff: ",
                        source,
                    })
                })?
            }
            "claim_event_consumer" => {
                let queue = &self.queue;
                let consumer_id = ConsumerId::parse(parse_str(args, "consumer_id")?)
                    .map_err(HarnessError::source)?;
                let claim_orphaned = args
                    .get("claim_orphaned")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let claimed_events = queue
                    .claim_primary(&consumer_id, claim_orphaned)
                    .await
                    .map_err(HarnessError::source)?;
                json!({
                    "primary_consumer_id": consumer_id,
                    "claimed_events": claimed_events
                })
            }

            _ => {
                return Err(HarnessError::source(ToolArgumentError(format!(
                    "unknown tool: {name}"
                ))));
            }
        })
    }
}
pub(crate) const TOOL_NAMES: &[&str] = &[
    "bind_codex_thread",
    "next_event",
    "ack_event",
    "event_queue_status",
    "register_event_consumer",
    "handoff_event_consumer",
    "claim_event_consumer",
];
pub(super) fn recognizes(name: &str) -> bool {
    TOOL_NAMES.contains(&name)
}
#[derive(Debug, Error)]
#[error("{0}")]
pub(super) struct ToolArgumentError(pub(super) String);
#[derive(Debug, Error)]
#[error("{context}{source}")]
struct SerializationError {
    context: &'static str,
    #[source]
    source: serde_json::Error,
}
fn parse_str<'a>(args: &'a Value, field: &str) -> Result<&'a str, HarnessError> {
    args.get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| HarnessError::source(ToolArgumentError(format!("missing {field}"))))
}
