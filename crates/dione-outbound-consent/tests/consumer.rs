use camino::Utf8PathBuf;
use dione_config::ChunkMode;
use dione_outbound_consent::{
    ConsentGate, DeliverReply, JournalRecord, Outcome, ReasonEntry, RejectReason, RejectedHandle,
    ReplyRequest, chunk_preserving_fences_with_context,
    delivery::{DeliverError, DeliveryCause, DeliveryProgress, PostedMessages},
    diary::DiaryRecord,
};
use serenity::model::id::{ChannelId, MessageId};
use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

const ORIGINAL: &str = "```rs\nfirst\nsecond\n```\n";

#[derive(Default)]
struct ExternalDelivery {
    requests: Mutex<Vec<ReplyRequest>>,
}

impl DeliverReply for ExternalDelivery {
    async fn deliver(&self, request: &ReplyRequest) -> Result<Vec<MessageId>, DeliverError> {
        let attempt = {
            let mut requests = self.requests.lock().unwrap();
            requests.push(request.clone());
            requests.len()
        };
        if attempt == 1 {
            return Err(DeliverError::total_with_diary(
                DeliveryCause::OutboundDenied(ChannelId::new(42)),
                request.pending_diary_records().to_vec(),
            ));
        }
        let chunks = chunk_preserving_fences_with_context(
            &request.content,
            18,
            ChunkMode::Paragraph,
            request.fence_context(),
        )
        .unwrap();
        if attempt == 2 {
            assert_eq!(chunks.len(), 2);
            assert_eq!(chunks[0].rendered, "```rs\nfirst\n```");
            assert_eq!(&request.content[chunks[1].source.start..], "second\n```\n");
            request.set_fence_context(chunks[1].incoming_context().cloned());
            return Err(DeliverError::partial(
                DeliveryCause::ChunkSend {
                    index: 1,
                    source: serenity::Error::Other("external second-chunk failure").into(),
                },
                PostedMessages::new(MessageId::new(100), Vec::new()),
                request.content[chunks[1].source.start..].to_owned(),
                request.pending_diary_records().to_vec(),
            ));
        }
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].rendered, "```rs\nsecond\n```\n");
        Ok(vec![MessageId::new(101)])
    }
}

#[tokio::test]
async fn external_delivery_preserves_held_bytes_progress_fence_diary_and_single_settlement() {
    let directory = tempfile::tempdir().unwrap();
    let state = Utf8PathBuf::from_path_buf(directory.path().to_owned()).unwrap();
    let gate = ConsentGate::new(&state);
    let delivery = ExternalDelivery::default();
    let records = vec![DiaryRecord::log_now("literal-log", ORIGINAL)];
    let request = ReplyRequest::new(
        ChannelId::new(42),
        ORIGINAL.to_owned(),
        Some(MessageId::new(7)),
        true,
        true,
    )
    .with_pending_diary_records(records.clone());
    assert_eq!(request.pending_diary_records(), records);
    assert!(request.fence_context().is_none());
    let reason = RejectReason {
        matches: vec![ReasonEntry {
            pattern: "literal-block".to_owned(),
            reason: None,
        }],
    };
    let now = Instant::now();
    let ticket = gate
        .bounce(request, reason, Duration::from_secs(30), 10, now)
        .await;
    assert!(matches!(
        gate.release(&delivery, &ticket.handle, now).await,
        Err(RejectedHandle::SendFailed {
            progress: DeliveryProgress::None,
            ..
        })
    ));
    assert_eq!(gate.pending().await, 1);
    let error = gate
        .release(&delivery, &ticket.handle, now)
        .await
        .unwrap_err();
    match error {
        RejectedHandle::SendFailed {
            progress: DeliveryProgress::Partial(posted),
            ..
        } => {
            assert_eq!(posted.iter().collect::<Vec<_>>(), vec![MessageId::new(100)]);
        }
        other => panic!("expected retained partial progress, got {other:?}"),
    }
    assert_eq!(gate.pending().await, 1);
    let released = gate.release(&delivery, &ticket.handle, now).await.unwrap();
    assert_eq!(
        released.message_ids,
        vec![MessageId::new(100), MessageId::new(101)]
    );
    assert_eq!(gate.pending().await, 0);
    assert!(matches!(
        gate.release(&delivery, &ticket.handle, now).await,
        Err(RejectedHandle::Unknown(_))
    ));
    {
        let requests = delivery.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].content, ORIGINAL);
        assert_eq!(requests[1].content, ORIGINAL);
        assert_eq!(requests[2].content, "second\n```\n");
        assert!(requests[2].fence_context().is_some());
        for request in requests.iter() {
            assert_eq!(request.channel_id, ChannelId::new(42));
            assert_eq!(request.reply_to_message_id, Some(MessageId::new(7)));
            assert!(request.suppress_ping && request.suppress_embeds);
            assert_eq!(request.pending_diary_records(), records);
        }
    }
    gate.journal().flush().await;
    let journal = gate.journal().load().unwrap();
    assert_eq!(journal.records.len(), 1);
    match &journal.records[0] {
        JournalRecord::Bounce(record) => {
            assert_eq!(record.message, ORIGINAL);
            assert_eq!(record.outcome, Outcome::Released);
        }
        JournalRecord::Summary(_) => panic!("a released handle produces one bounce resolution"),
    }
}

#[test]
fn public_constructors_retain_nonempty_progress() {
    let messages = PostedMessages::new(MessageId::new(100), vec![MessageId::new(101)]);
    assert_eq!(messages.first(), MessageId::new(100));
    assert_eq!(
        messages.iter().collect::<Vec<_>>(),
        vec![MessageId::new(100), MessageId::new(101)]
    );
    let partial = DeliverError::partial(
        DeliveryCause::OutboundDenied(ChannelId::new(42)),
        messages,
        "remaining".to_owned(),
        Vec::new(),
    );
    match partial.progress() {
        DeliveryProgress::Partial(messages) => assert_eq!(messages.iter().count(), 2),
        DeliveryProgress::None => panic!("partial construction requires posted messages"),
    }
}

#[test]
fn terminal_projection_preserves_typed_progress_cause_and_diary() {
    let records = vec![DiaryRecord::log_now("literal", "original")];
    let total = DeliverError::total_with_diary(
        DeliveryCause::OutboundDenied(ChannelId::new(42)),
        records.clone(),
    );
    let (cause, progress, diary) = total.into_parts();
    assert!(
        matches!(cause, DeliveryCause::OutboundDenied(channel) if channel == ChannelId::new(42))
    );
    assert_eq!(progress, DeliveryProgress::None);
    assert_eq!(diary, records);
    let partial = DeliverError::partial(
        DeliveryCause::OutboundDenied(ChannelId::new(43)),
        PostedMessages::new(MessageId::new(100), vec![MessageId::new(101)]),
        "private remaining source".to_owned(),
        records.clone(),
    );
    let (cause, progress, diary) = partial.into_parts();
    assert!(
        matches!(cause, DeliveryCause::OutboundDenied(channel) if channel == ChannelId::new(43))
    );
    match progress {
        DeliveryProgress::None => {
            panic!("terminal partial projection must retain nonempty posted progress")
        }
        DeliveryProgress::Partial(posted) => assert_eq!(
            posted.iter().collect::<Vec<_>>(),
            vec![MessageId::new(100), MessageId::new(101)]
        ),
    }
    assert_eq!(diary, records);
}
