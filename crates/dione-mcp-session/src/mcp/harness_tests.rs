//! Exercises a third adapter through common MCP and notification routing.
use super::*;
use dione_access_requests::AccessQueue;
use dione_contracts::MessageRef;
use dione_harness::{AttentionPolicy, HarnessDelivery, HarnessError, HarnessProtocol};
use dione_messages::delivery::Delivery;
use dione_tracing_channel::TraceLevelController;
use futures_util::future::BoxFuture;
use serenity::model::id::{ChannelId, MessageId};
use std::sync::Mutex as StdMutex;

#[derive(Clone)]
enum Received {
    Delivery(Delivery),
    Invalidated(MessageRef),
}
impl Received {
    async fn snapshot(&self) -> Value {
        match self {
            Self::Delivery(value) => crate::harness_test_support::snapshot(value).await,
            Self::Invalidated(source) => {
                json!({"invalidate":[source.conversation().local().as_numeric().unwrap().get(),source.local().as_numeric().unwrap().get()]})
            }
        }
    }
}
struct ThirdAdapter {
    received: Arc<StdMutex<Vec<Received>>>,
    changed: Arc<tokio::sync::Notify>,
}
impl HarnessDelivery for ThirdAdapter {
    fn attention_policy(&self) -> AttentionPolicy {
        AttentionPolicy::Direct
    }
    fn deliver<'a>(&'a self, value: &'a Delivery) -> BoxFuture<'a, Result<(), HarnessError>> {
        Box::pin(async move {
            self.received
                .lock()
                .unwrap()
                .push(Received::Delivery(value.clone()));
            self.changed.notify_one();
            Ok(())
        })
    }
    fn invalidate_attention(&self, source: MessageRef) -> BoxFuture<'_, Result<(), HarnessError>> {
        Box::pin(async move {
            self.received
                .lock()
                .unwrap()
                .push(Received::Invalidated(source));
            self.changed.notify_one();
            Ok(())
        })
    }
}
impl HarnessProtocol for ThirdAdapter {
    fn capabilities(&self) -> Value {
        json!({"tools": {}, "experimental": {"third/inbound": {}}})
    }
    fn tools(&self) -> Vec<Value> {
        vec![json!({"name": "third_echo", "inputSchema": {"type": "object"}})]
    }
    fn resident_control(&self) -> bool {
        false
    }
    fn permission_relay(&self) -> bool {
        false
    }
    fn call<'a>(
        &'a self,
        name: &'a str,
        args: &'a Value,
    ) -> BoxFuture<'a, Option<Result<Value, HarnessError>>> {
        Box::pin(
            async move { (name == "third_echo").then(|| Ok(json!({"echo": args["message"]}))) },
        )
    }
}
#[tokio::test]
async fn third_adapter_routes_notifications_invalidation_and_protocol() {
    let received = Arc::new(StdMutex::new(Vec::new()));
    let connection = HarnessConnection::from_adapter(
        ThirdAdapter {
            received: received.clone(),
            changed: Arc::new(tokio::sync::Notify::new()),
        },
        ProtocolOutput::new(tokio::io::sink()),
    );
    let dir = tempfile::TempDir::new().unwrap();
    let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let server = DioneServer::new(
        crate::TEST_APPLICATION_VERSION,
        dione_discord_state::new_state(),
        Arc::new(Mutex::new(AccessQueue::load(&path))),
        Arc::new(serenity::http::Http::new("fake")),
        path.clone(),
        mpsc::channel(1).0,
        TraceLevelController::noop(),
        connection.clone(),
        Arc::new(ConsentGate::new(&path)),
        Arc::new(dione_discord_runtime::IngressLedger::new()),
        dione_contracts::ModuleInstanceId::new(
            dione_contracts::ProviderId::new("discord").unwrap(),
            dione_contracts::ModuleSlotId::new("primary").unwrap(),
        ),
        None,
    )
    .await
    .expect("Discord reply registration");
    let initialize = dispatch(
        &server,
        "initialize",
        json!({"protocolVersion": "2025-06-18"}),
        RequestOrigin::Stdio,
    )
    .await
    .unwrap();
    assert_eq!(
        initialize["capabilities"],
        json!({"tools": {}, "experimental": {"third/inbound": {}}})
    );
    let discovery = dispatch(&server, "tools/list", json!({}), RequestOrigin::Stdio)
        .await
        .unwrap();
    assert_eq!(
        discovery["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "third_echo"),
        Some(&json!({"name": "third_echo", "inputSchema": {"type": "object"}}))
    );
    assert_eq!(
        dispatch(
            &server,
            "tools/call",
            json!({"name": "third_echo", "arguments": {"message": "hello"}}),
            RequestOrigin::Stdio,
        )
        .await
        .unwrap(),
        json!({"content": [{"type": "text", "text": "{\"echo\":\"hello\"}"}]})
    );
    assert_eq!(
        dispatch(
            &server,
            "tools/call",
            json!({"name": "unknown"}),
            RequestOrigin::Stdio
        )
        .await
        .unwrap_err(),
        "unknown tool: unknown"
    );
    assert_eq!(
        dispatch(
            &server,
            "notifications/claude/channel/permission_request",
            json!({}),
            RequestOrigin::Stdio,
        )
        .await
        .unwrap_err(),
        "method not found: notifications/claude/channel/permission_request"
    );
    let authority_module = server.attention.authority_module().clone();
    let attention = server.attention.clone();
    let no_rly = server.no_rly().clone();
    assert_eq!(
        run_resident_control(
            Arc::new(server),
            path.join("control.sock"),
            CancellationToken::new()
        )
        .await
        .unwrap_err()
        .to_string(),
        "resident control is unavailable for this harness"
    );
    for (request_id, granted) in [("first", true), ("second", false)] {
        let (events, input) = mpsc::channel(1);
        events
            .send(NotificationEvent::PermissionResponse {
                request_id: request_id.into(),
                granted,
            })
            .await
            .unwrap();
        drop(events);
        dione_recipient_attention::run_notifications(
            attention.clone(),
            connection.clone(),
            no_rly.clone(),
            None,
            input,
            CancellationToken::new(),
        )
        .await;
        if granted {
            connection
                .invalidate_attention(MessageRef::new(
                    dione_contracts::ConversationRef::new(
                        authority_module.clone(),
                        dione_contracts::ConversationId::numeric(10.try_into().unwrap()),
                    ),
                    dione_contracts::MessageId::numeric(20.try_into().unwrap()),
                ))
                .await
                .unwrap();
        }
    }
    assert_eq!(
        snapshots(&received).await,
        vec![
            json!({"jsonrpc": "2.0", "method": "notifications/claude/channel/permission", "params": {"request_id": "first", "behavior": "allow"}}),
            json!({"invalidate": [10, 20]}),
            json!({"jsonrpc": "2.0", "method": "notifications/claude/channel/permission", "params": {"request_id": "second", "behavior": "deny"}}),
        ]
    );
}

#[tokio::test]
async fn notification_loop_invalidates_edits_and_deletes_before_delivery() {
    let received = Arc::new(StdMutex::new(Vec::new()));
    let changed = Arc::new(tokio::sync::Notify::new());
    let connection = HarnessConnection::from_adapter(
        ThirdAdapter {
            received: received.clone(),
            changed: changed.clone(),
        },
        ProtocolOutput::new(tokio::io::sink()),
    );
    let dir = tempfile::TempDir::new().unwrap();
    let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let server = Arc::new(
        DioneServer::new(
            crate::TEST_APPLICATION_VERSION,
            dione_discord_state::new_state(),
            Arc::new(Mutex::new(AccessQueue::load(&path))),
            Arc::new(serenity::http::Http::new("fake")),
            path.clone(),
            mpsc::channel(1).0,
            TraceLevelController::noop(),
            connection,
            Arc::new(ConsentGate::new(&path)),
            Arc::new(dione_discord_runtime::IngressLedger::new()),
            dione_contracts::ModuleInstanceId::new(
                dione_contracts::ProviderId::new("discord").unwrap(),
                dione_contracts::ModuleSlotId::new("primary").unwrap(),
            ),
            None,
        )
        .await
        .expect("Discord reply registration"),
    );
    let (events, receiver) = mpsc::channel(4);
    let cancel = CancellationToken::new();
    let task = tokio::spawn(run_with_input(
        server,
        receiver,
        cancel.clone(),
        StdioExitPolicy::DetachUntilCancellation,
        tokio::io::empty(),
    ));
    events
        .send(NotificationEvent::MessageEdit {
            chat_id: ChannelId::new(10),
            message_id: MessageId::new(20),
            user: "author".into(),
            user_id: serenity::model::id::UserId::new(30),
            new_content: "revised".into(),
            timestamp: dione_messages::timestamp::Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
            thread_parent_id: None,
            reply_to_message_id: None,
        })
        .await
        .unwrap();
    events
        .send(NotificationEvent::MessageDelete {
            chat_id: ChannelId::new(10),
            message_id: MessageId::new(20),
            thread_parent_id: None,
        })
        .await
        .unwrap();
    drop(events);
    let completed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if received
                .lock()
                .unwrap()
                .iter()
                .any(|entry| matches!(entry, Received::Delivery(value) if matches!(value.kind(), dione_messages::delivery::DeliveryKind::Ordinary { first: dione_messages::events::OrdinaryEvent::MessageDeleted { .. }, .. })))
            {
                break;
            }
            changed.notified().await;
        }
    })
    .await;
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    completed.expect("notification loop delivered the delete");
    assert_eq!(
        snapshots(&received).await,
        vec![
            json!({"invalidate": [10, 20]}),
            json!({"jsonrpc": "2.0", "method": "notifications/claude/channel", "params": {"content": "revised", "meta": {"chat_id": "10", "message_id": "20", "user": "30=author", "user_id": "30", "type": "message_edit", "ts": "2026-01-01T00:00:00+00:00"}}}),
            json!({"invalidate": [10, 20]}),
            json!({"jsonrpc": "2.0", "method": "notifications/claude/channel", "params": {"content": "message deleted", "meta": {"chat_id": "10", "message_id": "20", "type": "message_delete"}}}),
        ]
    );
}

#[tokio::test]
async fn oh_my_pi_ordinary_mcp_tools_remain_available() {
    let connection = dione_omp::connect(
        ProtocolOutput::new(tokio::io::sink()),
        None,
        dione_codex::TOOL_NAMES,
    );
    let dir = tempfile::TempDir::new().unwrap();
    let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let server = DioneServer::new(
        crate::TEST_APPLICATION_VERSION,
        dione_discord_state::new_state(),
        Arc::new(Mutex::new(AccessQueue::load(&path))),
        Arc::new(serenity::http::Http::new("fake")),
        path.clone(),
        mpsc::channel(1).0,
        TraceLevelController::noop(),
        connection.clone(),
        Arc::new(ConsentGate::new(&path)),
        Arc::new(dione_discord_runtime::IngressLedger::new()),
        dione_contracts::ModuleInstanceId::new(
            dione_contracts::ProviderId::new("discord").unwrap(),
            dione_contracts::ModuleSlotId::new("primary").unwrap(),
        ),
        None,
    )
    .await
    .expect("Discord reply registration");
    let initialized = dispatch(
        &server,
        "initialize",
        json!({"protocolVersion":"2025-06-18"}),
        RequestOrigin::Stdio,
    )
    .await
    .unwrap();
    assert_eq!(initialized["capabilities"], json!({"tools":{}}));
    assert!(!connection.protocol().resident_control());
    assert!(!connection.protocol().permission_relay());
    let discovery = dispatch(&server, "tools/list", json!({}), RequestOrigin::Stdio)
        .await
        .unwrap();
    let names: Vec<_> = discovery["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"list_access_requests"));
    assert!(!names.contains(&"event_queue_status"));
    assert!(!names.contains(&"bind_codex_thread"));
    assert_eq!(
        dispatch(
            &server,
            "tools/call",
            json!({"name":"list_access_requests","arguments":{}}),
            RequestOrigin::Stdio,
        )
        .await
        .unwrap(),
        json!({"content":[{"type":"text","text":"{\"requests\":[]}"}]})
    );
    assert_eq!(
        dispatch(
            &server,
            "tools/call",
            json!({"name":"event_queue_status","arguments":{}}),
            RequestOrigin::Stdio,
        )
        .await
        .unwrap_err(),
        "event_queue_status is only available in Codex mode"
    );
    assert_eq!(
        dispatch(
            &server,
            "notifications/claude/channel/permission_request",
            json!({}),
            RequestOrigin::Stdio,
        )
        .await
        .unwrap_err(),
        "method not found: notifications/claude/channel/permission_request"
    );
}

#[tokio::test]
async fn third_adapter_retains_opaque_scoped_records_and_unprojected_observations() {
    let received = Arc::new(StdMutex::new(Vec::new()));
    let connection = HarnessConnection::from_adapter(
        ThirdAdapter {
            received: received.clone(),
            changed: Arc::new(tokio::sync::Notify::new()),
        },
        ProtocolOutput::new(tokio::io::sink()),
    );
    let make = |slot: &str, text: &str| {
        let module = dione_contracts::ModuleInstanceId::new(
            dione_contracts::ProviderId::new("fake").unwrap(),
            dione_contracts::ModuleSlotId::new(slot).unwrap(),
        );
        dione_messages::events::OrdinaryEvent::Message(Box::new(
            dione_messages::events::MessageCreated::new(
                MessageRef::new(
                    dione_contracts::ConversationRef::new(
                        module.clone(),
                        dione_contracts::ConversationId::opaque("10").unwrap(),
                    ),
                    dione_contracts::MessageId::opaque("20").unwrap(),
                ),
                dione_messages::events::ObservedAuthor::new(
                    dione_contracts::ActorRef::new(
                        module,
                        dione_contracts::ActorId::opaque("30").unwrap(),
                    ),
                    "same display".into(),
                    dione_messages::events::AuthorOrigin::VerifiedWebhook,
                ),
                text.into(),
                dione_messages::events::Targeting::ReplyToConstruct,
                dione_messages::timestamp::Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
            )
            .with_attachments(
                vec![dione_messages::events::ObservedAttachment::new(
                    "empty.bin".into(),
                    None,
                    dione_messages::events::ByteSize::new(0),
                )],
                true,
            ),
        ))
    };
    connection
        .deliver(&Delivery::batch(
            make("second", "first\n\n"),
            vec![make("first", "")],
            crate::harness_test_support::context(),
        ))
        .await
        .unwrap();
    let received = received.lock().unwrap();
    let [Received::Delivery(delivery)] = received.as_slice() else {
        panic!("one complete typed delivery");
    };
    let dione_messages::delivery::DeliveryKind::Ordinary { first, rest } = delivery.kind() else {
        panic!("ordinary records");
    };
    assert_eq!(rest.len(), 1);
    assert_ne!(
        first.source().conversation(),
        rest[0].source().conversation()
    );
    for (event, expected_text) in [(first, "first\n\n"), (&rest[0], "")] {
        let dione_messages::events::OrdinaryEvent::Message(message) = event else {
            panic!("created observation");
        };
        assert_eq!(message.source().local().as_opaque(), Some("20"));
        assert!(message.source().local().as_numeric().is_none());
        assert_eq!(message.author().actor().local().as_opaque(), Some("30"));
        assert_eq!(
            message.author().origin(),
            dione_messages::events::AuthorOrigin::VerifiedWebhook
        );
        assert_eq!(
            message.targeting(),
            dione_messages::events::Targeting::ReplyToConstruct
        );
        assert_eq!(message.content().text(), expected_text);
        assert!(message.is_voice());
        assert_eq!(message.attachments()[0].size().get(), 0);
        assert_eq!(message.attachments()[0].media_type(), None);
    }
}

async fn snapshots(received: &Arc<StdMutex<Vec<Received>>>) -> Vec<Value> {
    let captured = received.lock().unwrap().clone();
    let mut result = Vec::new();
    for entry in captured {
        result.push(entry.snapshot().await);
    }
    result
}
