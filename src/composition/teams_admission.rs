//! Explicit Teams admission composition for resident harness owners.

use crate::{
    codex::{CodexEventQueue, LiveAdmissionOutcome, LiveQueueErrorKind},
    durable::{AdmissionOutcome, QueueErrorKind},
    harness::omp_session::{OmpAdmission, OmpSessionError, OmpSessionErrorKind},
    teams::delivery::{
        TeamsAdmissionOutcome, TeamsDelivery, TeamsDeliveryError, TeamsDeliveryErrorKind,
        TeamsEventSink, TeamsResidentEvent,
    },
};
use serde_json::json;
use std::{future::Future, pin::Pin};

struct CodexTeamsAdmission {
    queue: CodexEventQueue,
}

/// Assembles Teams delivery over a prepared Codex admission queue.
pub fn from_codex(queue: CodexEventQueue) -> TeamsDelivery {
    TeamsDelivery::new(CodexTeamsAdmission { queue })
}

pub(crate) fn from_omp(admission: OmpAdmission) -> TeamsDelivery {
    TeamsDelivery::new(OmpTeamsAdmission { admission })
}

struct OmpTeamsAdmission {
    admission: OmpAdmission,
}

impl TeamsEventSink for OmpTeamsAdmission {
    fn deliver<'a>(
        &'a self,
        event: TeamsResidentEvent,
    ) -> Pin<Box<dyn Future<Output = Result<TeamsAdmissionOutcome, TeamsDeliveryError>> + Send + 'a>>
    {
        Box::pin(async move {
            match self.admission.enqueue(event).await {
                Ok(AdmissionOutcome::Committed) => Ok(TeamsAdmissionOutcome::Committed),
                Ok(AdmissionOutcome::Duplicate) => Ok(TeamsAdmissionOutcome::Duplicate),
                Ok(AdmissionOutcome::CommittedDurabilityUncertain(source)) => Ok(
                    TeamsAdmissionOutcome::CommittedDurabilityUncertain(TeamsDeliveryError {
                        kind: TeamsDeliveryErrorKind::Storage(Box::new(source)),
                    }),
                ),
                Ok(AdmissionOutcome::CompletionUnknown(source)) => Ok(
                    TeamsAdmissionOutcome::CompletionUnknown(TeamsDeliveryError {
                        kind: TeamsDeliveryErrorKind::CompletionUnknown(source),
                    }),
                ),
                Err(error) => {
                    let kind = match error.kind {
                        kind @ (OmpSessionErrorKind::Unavailable
                        | OmpSessionErrorKind::StaleBinding
                        | OmpSessionErrorKind::InvalidSession
                        | OmpSessionErrorKind::InvalidBinding
                        | OmpSessionErrorKind::EventIdentity
                        | OmpSessionErrorKind::EventShape
                        | OmpSessionErrorKind::Output(_)
                        | OmpSessionErrorKind::OutputUnavailable
                        | OmpSessionErrorKind::Worker(_)) => {
                            TeamsDeliveryErrorKind::RouteUnavailable(Box::new(OmpSessionError {
                                kind,
                            }))
                        }
                        OmpSessionErrorKind::Queue(source) => match &source.kind {
                            QueueErrorKind::ReplayCapacity => {
                                TeamsDeliveryErrorKind::ReplayCapacity(Box::new(source))
                            }
                            QueueErrorKind::ResidentUnavailable
                            | QueueErrorKind::UnknownConsumer
                            | QueueErrorKind::NotPrimaryConsumer
                            | QueueErrorKind::PrimaryConsumerExists => {
                                TeamsDeliveryErrorKind::RouteUnavailable(Box::new(source))
                            }
                            QueueErrorKind::InvalidStorageLayout
                            | QueueErrorKind::Io(_)
                            | QueueErrorKind::Locked(_)
                            | QueueErrorKind::Codec(_)
                            | QueueErrorKind::PriorSync(_)
                            | QueueErrorKind::DurabilityUncertain(_)
                            | QueueErrorKind::CompletionUnknown(_)
                            | QueueErrorKind::Identity(_)
                            | QueueErrorKind::Integrity(_)
                            | QueueErrorKind::AllocationExhausted { .. }
                            | QueueErrorKind::UnknownDeliveryToken
                            | QueueErrorKind::NotDeferrable => {
                                TeamsDeliveryErrorKind::Storage(Box::new(source))
                            }
                        },
                        OmpSessionErrorKind::CompletionUnknown(source) => {
                            return Ok(TeamsAdmissionOutcome::CompletionUnknown(
                                TeamsDeliveryError {
                                    kind: TeamsDeliveryErrorKind::ResponseLost(source),
                                },
                            ));
                        }
                    };
                    Err(TeamsDeliveryError { kind })
                }
            }
        })
    }
}

impl TeamsEventSink for CodexTeamsAdmission {
    fn deliver<'a>(
        &'a self,
        event: TeamsResidentEvent,
    ) -> Pin<Box<dyn Future<Output = Result<TeamsAdmissionOutcome, TeamsDeliveryError>> + Send + 'a>>
    {
        Box::pin(async move {
            let notification = json!({
                "jsonrpc": "2.0",
                "method": "notifications/claude/channel",
                "params": {
                    "content": event.content,
                    "meta": {
                        "provider": "teams",
                        "message_id": event.message_id,
                        "conversation_id": event.conversation_id,
                        "channel_id": event.channel_id,
                        "user_id": event.user_id,
                        "bot_id": event.bot_id,
                        "tenant_id": event.tenant_id,
                        "reply_handle": event.reply_handle,
                        "ts": event.timestamp,
                    }
                }
            });
            match self
                .queue
                .enqueue_live(notification, event.provider_key)
                .await
            {
                Ok(LiveAdmissionOutcome::Committed) => Ok(TeamsAdmissionOutcome::Committed),
                Ok(LiveAdmissionOutcome::CompletionUnknown(source)) => Ok(
                    TeamsAdmissionOutcome::CompletionUnknown(TeamsDeliveryError {
                        kind: TeamsDeliveryErrorKind::CompletionUnknown(source),
                    }),
                ),
                Ok(LiveAdmissionOutcome::Duplicate) => Ok(TeamsAdmissionOutcome::Duplicate),
                Ok(LiveAdmissionOutcome::CommittedDurabilityUncertain(error)) => Ok(
                    TeamsAdmissionOutcome::CommittedDurabilityUncertain(TeamsDeliveryError {
                        kind: TeamsDeliveryErrorKind::Storage(Box::new(error)),
                    }),
                ),
                Err(error) => {
                    let kind = match &error.kind {
                        LiveQueueErrorKind::ResidentUnavailable(_) => {
                            TeamsDeliveryErrorKind::RouteUnavailable(Box::new(error))
                        }
                        LiveQueueErrorKind::ReplayCapacity(_) => {
                            TeamsDeliveryErrorKind::ReplayCapacity(Box::new(error))
                        }
                        LiveQueueErrorKind::Queue(_) => {
                            TeamsDeliveryErrorKind::Storage(Box::new(error))
                        }
                    };
                    Err(TeamsDeliveryError { kind })
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        codex::{
            CodexQueueError, CodexQueueErrorKind, CodexThreadId, LegacyReplayIdentity,
            LiveQueueError,
        },
        delivery::ProviderEventKey,
        durable::{PersistFailure, QueueError, QueueErrorKind, state::ReplayReceipt},
        teams::ReplyHandle,
        timestamp::Timestamp,
    };
    use camino::Utf8PathBuf;
    use chrono::Utc;
    use std::{error::Error, io};
    use tempfile::TempDir;

    fn event(activity: &str) -> TeamsResidentEvent {
        TeamsResidentEvent {
            provider_key: ProviderEventKey::teams(
                "tenant".parse().unwrap(),
                "bot".parse().unwrap(),
                "conversation".parse().unwrap(),
                activity.parse().unwrap(),
            ),
            content: "private-message-sentinel".into(),
            message_id: activity.parse().unwrap(),
            conversation_id: "conversation".parse().unwrap(),
            channel_id: "msteams".into(),
            user_id: "actor".parse().unwrap(),
            bot_id: "bot".parse().unwrap(),
            tenant_id: "tenant".parse().unwrap(),
            reply_handle: ReplyHandle::parse("teams-00000000-0000-4000-8000-000000000001").unwrap(),
            timestamp: Timestamp::now(),
        }
    }

    fn original_io(error: &TeamsDeliveryError) -> &io::Error {
        let mut source: &(dyn Error + 'static) = error;
        loop {
            if let Some(io) = source.downcast_ref::<io::Error>() {
                return io;
            }
            source = source
                .source()
                .expect("original IO cause must remain reachable");
        }
    }

    #[tokio::test]
    async fn adapter_preserves_causes_and_current_event_commit_classification() {
        let temp = TempDir::new().unwrap();
        let path = Utf8PathBuf::from_path_buf(temp.path().to_owned()).unwrap();
        let queue = CodexEventQueue::load(
            &path,
            crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
                crate::core::ProviderId::new("discord").unwrap(),
                crate::core::ModuleSlotId::new("primary").unwrap(),
            )),
        )
        .unwrap();
        let unavailable = from_codex(queue.clone())
            .deliver(event("first"))
            .await
            .err()
            .unwrap();
        assert!(
            matches!(&unavailable.kind, TeamsDeliveryErrorKind::RouteUnavailable(source) if matches!(source.downcast_ref::<LiveQueueError>(), Some(LiveQueueError { kind: LiveQueueErrorKind::ResidentUnavailable(_) })))
        );
        assert_eq!(unavailable.to_string(), "resident route unavailable");
        let resident = queue.register_live_consumer().await.unwrap();
        resident
            .bind_live_thread(Some(CodexThreadId::parse("resident").unwrap()))
            .await
            .unwrap();
        for stage in [PersistFailure::BeforeWrite, PersistFailure::BeforeRename] {
            queue.test_set_persist_failure(Some(stage)).await;
            let rejected = from_codex(queue.clone())
                .deliver(event("first"))
                .await
                .err()
                .unwrap();
            assert!(
                matches!(&rejected.kind, TeamsDeliveryErrorKind::Storage(source) if matches!(source.downcast_ref::<LiveQueueError>(), Some(LiveQueueError { kind: LiveQueueErrorKind::Queue(CodexQueueError { kind: CodexQueueErrorKind::Durable(QueueError { kind: QueueErrorKind::Io(_), .. }) }) })))
            );
            assert_eq!(original_io(&rejected).kind(), io::ErrorKind::Other);
            assert_eq!(rejected.to_string(), "resident storage failed");
            assert_eq!(queue.status().await.queued, 0);
        }
        queue
            .test_set_persist_failure(Some(PersistFailure::AfterRename))
            .await;
        let receipt = from_codex(queue.clone())
            .deliver(event("first"))
            .await
            .unwrap();
        let TeamsAdmissionOutcome::CommittedDurabilityUncertain(committed) = receipt else {
            panic!("visible commit must remain a receipt")
        };
        assert!(
            matches!(&committed.kind, TeamsDeliveryErrorKind::Storage(source) if matches!(source.downcast_ref::<CodexQueueError>(), Some(CodexQueueError { kind: CodexQueueErrorKind::Durable(QueueError { kind: QueueErrorKind::DurabilityUncertain(_), .. }) })))
        );
        assert_eq!(original_io(&committed).kind(), io::ErrorKind::Other);
        assert_eq!(queue.status().await.queued, 1);
        let rejected = from_codex(queue.clone())
            .deliver(event("second"))
            .await
            .err()
            .unwrap();
        assert!(
            matches!(&rejected.kind, TeamsDeliveryErrorKind::Storage(source) if matches!(source.downcast_ref::<LiveQueueError>(), Some(LiveQueueError { kind: LiveQueueErrorKind::Queue(CodexQueueError { kind: CodexQueueErrorKind::Durable(QueueError { kind: QueueErrorKind::PriorSync(_), .. }) }) })))
        );
        assert_eq!(queue.status().await.queued, 1);
        queue.test_set_persist_failure(None).await;
        assert!(matches!(
            from_codex(queue.clone())
                .deliver(event("first"))
                .await
                .unwrap(),
            TeamsAdmissionOutcome::Duplicate
        ));
        assert!(matches!(
            from_codex(queue.clone())
                .deliver(event("second"))
                .await
                .unwrap(),
            TeamsAdmissionOutcome::Committed
        ));
        assert_eq!(queue.status().await.queued, 2);
    }

    #[tokio::test]
    async fn adapter_capacity_classification_preserves_original_rejection_and_duplicates() {
        let temp = TempDir::new().unwrap();
        let path = Utf8PathBuf::from_path_buf(temp.path().to_owned()).unwrap();
        let queue = CodexEventQueue::load(
            &path,
            crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
                crate::core::ProviderId::new("discord").unwrap(),
                crate::core::ModuleSlotId::new("primary").unwrap(),
            )),
        )
        .unwrap();
        let resident = queue.register_live_consumer().await.unwrap();
        resident
            .bind_live_thread(Some(CodexThreadId::parse("resident").unwrap()))
            .await
            .unwrap();
        queue
            .test_mutate_snapshot(|snapshot| {
                let now = Utc::now();
                for index in 0..10_000 {
                    snapshot.acknowledged.push_back(ReplayReceipt {
                        key: LegacyReplayIdentity::Provider(
                            event(&format!("activity-{index}")).provider_key,
                        ),
                        acknowledged_at: now,
                    });
                }
            })
            .await;
        let rejected = from_codex(queue.clone())
            .deliver(event("new"))
            .await
            .err()
            .unwrap();
        assert!(
            matches!(&rejected.kind, TeamsDeliveryErrorKind::ReplayCapacity(source) if matches!(source.downcast_ref::<LiveQueueError>(), Some(LiveQueueError { kind: LiveQueueErrorKind::ReplayCapacity(_) })))
        );
        assert!(matches!(
            from_codex(queue.clone())
                .deliver(event("activity-0"))
                .await
                .unwrap(),
            TeamsAdmissionOutcome::Duplicate
        ));
        assert_eq!(queue.status().await.queued, 0);
    }
}
