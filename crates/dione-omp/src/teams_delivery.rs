//! Explicit Teams admission composition for resident harness owners.

use crate::{OmpAdmission, OmpSessionError, OmpSessionErrorKind};
use dione_durable::{AdmissionOutcome, QueueErrorKind};
use dione_provider_delivery::{
    TeamsAdmissionOutcome, TeamsDelivery, TeamsDeliveryError, TeamsDeliveryErrorKind,
    TeamsEventSink, TeamsResidentEvent,
};
use std::{future::Future, pin::Pin};

/// Assembles Teams delivery over an existing OMP session admission capability.
pub fn teams_delivery(admission: OmpAdmission) -> TeamsDelivery {
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
        Box::pin(async move { map_admission(self.admission.enqueue(event).await) })
    }
}

fn map_admission(
    result: Result<AdmissionOutcome, OmpSessionError>,
) -> Result<TeamsAdmissionOutcome, TeamsDeliveryError> {
    match result {
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
                    TeamsDeliveryErrorKind::RouteUnavailable(Box::new(OmpSessionError { kind }))
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use dione_durable::{
        AllocationKind, CodecError, CodecErrorKind, EndpointId, QueueError, SnapshotIssue,
    };
    use std::{error::Error, io};

    fn original_io(error: &TeamsDeliveryError) -> &io::Error {
        let mut source: &(dyn Error + 'static) = error;
        loop {
            if let Some(io) = source.downcast_ref::<io::Error>() {
                return io;
            }
            source = source
                .source()
                .expect("concrete IO cause remains reachable");
        }
    }
    fn failure(kind: OmpSessionErrorKind) -> TeamsDeliveryError {
        map_admission(Err(kind.into()))
            .err()
            .expect("rejected admission remains rejected")
    }
    #[test]
    fn committed_duplicate_and_visible_uncertainty_remain_distinct() {
        assert!(matches!(
            map_admission(Ok(AdmissionOutcome::Committed)),
            Ok(TeamsAdmissionOutcome::Committed)
        ));
        assert!(matches!(
            map_admission(Ok(AdmissionOutcome::Duplicate)),
            Ok(TeamsAdmissionOutcome::Duplicate)
        ));
        let queue = QueueError::new(
            "visible sentinel",
            QueueErrorKind::DurabilityUncertain(io::Error::from_raw_os_error(71)),
        );
        let result =
            map_admission(Ok(AdmissionOutcome::CommittedDurabilityUncertain(queue))).unwrap();
        let TeamsAdmissionOutcome::CommittedDurabilityUncertain(error) = result else {
            panic!("visible uncertain is neither rejection nor certain commit")
        };
        assert!(
            matches!(&error.kind, TeamsDeliveryErrorKind::Storage(source) if source.downcast_ref::<QueueError>().unwrap().operation=="visible sentinel")
        );
        assert_eq!(original_io(&error).raw_os_error(), Some(71));
    }
    #[tokio::test]
    async fn both_completion_unknown_paths_retain_concrete_causes() {
        let join = tokio::spawn(async { panic!("owned worker sentinel") })
            .await
            .unwrap_err();
        assert!(join.is_panic());
        let result = map_admission(Ok(AdmissionOutcome::CompletionUnknown(join))).unwrap();
        let TeamsAdmissionOutcome::CompletionUnknown(error) = result else {
            panic!("owned completion cannot imply commit or rejection")
        };
        assert!(
            matches!(error.kind,TeamsDeliveryErrorKind::CompletionUnknown(source) if source.is_panic())
        );
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        drop(tx);
        let result = map_admission(Err(OmpSessionErrorKind::CompletionUnknown(
            rx.await.unwrap_err(),
        )
        .into()))
        .unwrap();
        let TeamsAdmissionOutcome::CompletionUnknown(error) = result else {
            panic!("lost response cannot imply commit or rejection")
        };
        assert!(matches!(
            error.kind,
            TeamsDeliveryErrorKind::ResponseLost(_)
        ));
    }
    #[test]
    fn route_errors_keep_original_session_kind() {
        for kind in [
            OmpSessionErrorKind::Unavailable,
            OmpSessionErrorKind::StaleBinding,
            OmpSessionErrorKind::InvalidSession,
            OmpSessionErrorKind::InvalidBinding,
            OmpSessionErrorKind::EventIdentity,
            OmpSessionErrorKind::EventShape,
            OmpSessionErrorKind::OutputUnavailable,
        ] {
            let expected = std::mem::discriminant(&kind);
            let error = failure(kind);
            let TeamsDeliveryErrorKind::RouteUnavailable(source) = error.kind else {
                panic!("route error category")
            };
            assert_eq!(
                std::mem::discriminant(&source.downcast_ref::<OmpSessionError>().unwrap().kind),
                expected
            );
        }
    }
    #[test]
    fn queue_capacity_routing_and_storage_keep_typed_sources() {
        let error = failure(OmpSessionErrorKind::Queue(QueueError::new(
            "capacity",
            QueueErrorKind::ReplayCapacity,
        )));
        assert!(
            matches!(error.kind,TeamsDeliveryErrorKind::ReplayCapacity(source) if matches!(source.downcast_ref::<QueueError>().unwrap().kind,QueueErrorKind::ReplayCapacity))
        );
        for kind in [
            QueueErrorKind::ResidentUnavailable,
            QueueErrorKind::UnknownConsumer,
            QueueErrorKind::NotPrimaryConsumer,
            QueueErrorKind::PrimaryConsumerExists,
        ] {
            let expected = std::mem::discriminant(&kind);
            let error = failure(OmpSessionErrorKind::Queue(QueueError::new("route", kind)));
            assert!(
                matches!(error.kind,TeamsDeliveryErrorKind::RouteUnavailable(source) if std::mem::discriminant(&source.downcast_ref::<QueueError>().unwrap().kind)==expected)
            );
        }
        for kind in [
            QueueErrorKind::InvalidStorageLayout,
            QueueErrorKind::Io(io::Error::from_raw_os_error(72)),
            QueueErrorKind::Locked(io::Error::from_raw_os_error(73)),
            QueueErrorKind::Codec(CodecError {
                kind: CodecErrorKind::Decode(Box::new(io::Error::from_raw_os_error(74))),
            }),
            QueueErrorKind::PriorSync(io::Error::from_raw_os_error(75)),
            QueueErrorKind::DurabilityUncertain(io::Error::from_raw_os_error(76)),
            QueueErrorKind::Identity(EndpointId::new("").unwrap_err()),
            QueueErrorKind::Integrity(SnapshotIssue::DuplicateEvent),
            QueueErrorKind::AllocationExhausted {
                allocation: AllocationKind::Event,
            },
            QueueErrorKind::UnknownDeliveryToken,
            QueueErrorKind::NotDeferrable,
        ] {
            let expected = std::mem::discriminant(&kind);
            let error = failure(OmpSessionErrorKind::Queue(QueueError::new(
                "storage sentinel",
                kind,
            )));
            assert!(
                matches!(error.kind,TeamsDeliveryErrorKind::Storage(source) if std::mem::discriminant(&source.downcast_ref::<QueueError>().unwrap().kind)==expected)
            );
        }
        let error = failure(OmpSessionErrorKind::Queue(QueueError::new(
            "io sentinel",
            QueueErrorKind::Io(io::Error::from_raw_os_error(77)),
        )));
        assert_eq!(original_io(&error).raw_os_error(), Some(77));
    }
    #[tokio::test]
    async fn protocol_output_failure_keeps_actual_io_cause() {
        let (writer, reader) = tokio::io::duplex(64);
        drop(reader);
        let output = dione_harness::ProtocolOutput::new(writer);
        let source = output
            .write(&serde_json::json!({"sentinel":"frame"}))
            .await
            .unwrap_err();
        assert!(source.completion_is_unknown());
        let error = failure(OmpSessionErrorKind::Output(source));
        assert!(
            matches!(&error.kind,TeamsDeliveryErrorKind::RouteUnavailable(source) if matches!(&source.downcast_ref::<OmpSessionError>().unwrap().kind,OmpSessionErrorKind::Output(output) if output.completion_is_unknown()))
        );
        assert_eq!(original_io(&error).kind(), io::ErrorKind::BrokenPipe);
    }
    #[tokio::test]
    async fn worker_and_queue_completion_errors_do_not_lose_panic_identity() {
        let source = tokio::spawn(async { panic!("route worker") })
            .await
            .unwrap_err();
        let error = failure(OmpSessionErrorKind::Worker(source));
        assert!(
            matches!(error.kind,TeamsDeliveryErrorKind::RouteUnavailable(source) if matches!(&source.downcast_ref::<OmpSessionError>().unwrap().kind,OmpSessionErrorKind::Worker(join) if join.is_panic()))
        );
        let source = tokio::spawn(async { panic!("storage worker") })
            .await
            .unwrap_err();
        let error = failure(OmpSessionErrorKind::Queue(QueueError::new(
            "queue completion",
            QueueErrorKind::CompletionUnknown(source),
        )));
        assert!(
            matches!(error.kind,TeamsDeliveryErrorKind::Storage(source) if matches!(&source.downcast_ref::<QueueError>().unwrap().kind,QueueErrorKind::CompletionUnknown(join) if join.is_panic()))
        );
    }
}
