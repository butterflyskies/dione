//! Fault-injected forwarding tests through the production scheduling path.

use super::*;
use crate::{
    core::{
        MessageRef, ModuleInstanceId, ModuleSlotId, ProviderId,
        delivery::{Delivery, DeliveryKind},
    },
    harness::{HarnessDelivery, HarnessError},
    mcp::harness_wire::HarnessProtocol,
};
use futures_util::future::BoxFuture;
use serenity::model::id::MessageId;
use std::{io, sync::Mutex as StdMutex};

#[derive(Default)]
struct SinkState {
    attempts: usize,
    accepted: Vec<MessageId>,
}

struct FailSecondSend(Arc<StdMutex<SinkState>>);

impl HarnessDelivery for FailSecondSend {
    fn attention_policy(&self) -> AttentionPolicy {
        AttentionPolicy::Direct
    }

    fn deliver<'a>(&'a self, value: &'a Delivery) -> BoxFuture<'a, Result<(), HarnessError>> {
        Box::pin(async move {
            let mut state = self.0.lock().unwrap();
            state.attempts += 1;
            if state.attempts == 2 {
                return Err(HarnessError::source(io::Error::other(
                    "injected second-send failure",
                )));
            }
            let DeliveryKind::Ordinary { first, rest } = value.kind() else {
                panic!("ordinary fixture");
            };
            assert!(rest.is_empty());
            state.accepted.push(MessageId::new(
                first.source().local().as_numeric().unwrap().get(),
            ));
            Ok(())
        })
    }

    fn invalidate_attention(&self, _source: MessageRef) -> BoxFuture<'_, Result<(), HarnessError>> {
        Box::pin(async { Ok(()) })
    }
}

impl HarnessProtocol for FailSecondSend {
    fn capabilities(&self) -> Value {
        json!({})
    }
    fn tools(&self) -> Vec<Value> {
        Vec::new()
    }
    fn resident_control(&self) -> bool {
        false
    }
    fn permission_relay(&self) -> bool {
        false
    }
    fn call<'a>(
        &'a self,
        _name: &'a str,
        _args: &'a Value,
    ) -> BoxFuture<'a, Option<Result<Value, HarnessError>>> {
        Box::pin(async { None })
    }
}

fn sink() -> (HarnessConnection, Arc<StdMutex<SinkState>>) {
    let state = Arc::new(StdMutex::new(SinkState::default()));
    let connection = HarnessConnection::from_adapter(
        FailSecondSend(state.clone()),
        ProtocolOutput::new(tokio::io::sink()),
    );
    (connection, state)
}

fn evidence(id: MessageId) -> NotificationEvent {
    let NotificationEvent::Message(mut message) = super::tests::evidence_message_event(0) else {
        panic!("message fixture");
    };
    message.message_id = id;
    NotificationEvent::Message(message)
}

fn ids(events: &[NotificationEvent]) -> Vec<MessageId> {
    events
        .iter()
        .map(|event| match event {
            NotificationEvent::Message(message) => message.message_id,
            NotificationEvent::Reaction { .. }
            | NotificationEvent::MessageEdit { .. }
            | NotificationEvent::MessageDelete { .. }
            | NotificationEvent::PermissionResponse { .. }
            | NotificationEvent::Trace { .. }
            | NotificationEvent::ConfigError { .. }
            | NotificationEvent::Coordination(_) => panic!("message fixture"),
        })
        .collect()
}

#[tokio::test]
async fn failed_fifo_send_retains_failed_event_and_unattempted_tail_for_retry() {
    let (sink, state) = sink();
    let mut buffer = DeliveryBuffer::new(ModuleInstanceId::new(
        ProviderId::new("discord").unwrap(),
        ModuleSlotId::new("primary").unwrap(),
    ));
    let outcome = deliver_flushed(
        &sink,
        buffer.module(),
        vec![
            evidence(MessageId::new(1)),
            evidence(MessageId::new(2)),
            evidence(MessageId::new(3)),
        ],
        None,
        true,
    )
    .await;
    assert!(outcome.error.is_some());
    assert_eq!(
        ids(&outcome.undelivered),
        [MessageId::new(2), MessageId::new(3)]
    );
    assert_eq!(state.lock().unwrap().accepted, [MessageId::new(1)]);
    buffer.requeue(outcome.undelivered);
    let events = buffer.flush_all();
    deliver_flushed(&sink, buffer.module(), events, None, true)
        .await
        .into_result()
        .unwrap();
    assert_eq!(
        state.lock().unwrap().accepted,
        [MessageId::new(1), MessageId::new(2), MessageId::new(3)]
    );
    assert!(buffer.flush_all().is_empty());
}

#[tokio::test]
async fn mixed_control_and_evidence_flush_is_rejected_before_the_first_send() {
    let (sink, state) = sink();
    let module = ModuleInstanceId::new(
        ProviderId::new("discord").unwrap(),
        ModuleSlotId::new("primary").unwrap(),
    );
    let events = vec![
        evidence(MessageId::new(1)),
        NotificationEvent::PermissionResponse {
            request_id: "request".into(),
            granted: true,
        },
    ];
    let outcome = deliver_flushed(&sink, &module, events, None, true).await;
    assert!(outcome.error.is_some());
    assert_eq!(outcome.undelivered.len(), 2);
    assert_eq!(state.lock().unwrap().attempts, 0);
    assert!(
        matches!(&outcome.undelivered[1],NotificationEvent::PermissionResponse {request_id,granted:true} if request_id=="request")
    );
    deliver_flushed(
        &sink,
        &module,
        vec![evidence(MessageId::new(1))],
        None,
        true,
    )
    .await
    .into_result()
    .unwrap();
    assert_eq!(state.lock().unwrap().accepted, [MessageId::new(1)]);
}
