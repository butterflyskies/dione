//! Explicit Teams admission composition for resident harness owners.

use dione_durable::{AdmissionOutcome, QueueErrorKind};
use dione_omp::{OmpAdmission, OmpSessionError, OmpSessionErrorKind};
use dione_provider_delivery::{
    TeamsAdmissionOutcome, TeamsDelivery, TeamsDeliveryError, TeamsDeliveryErrorKind,
    TeamsEventSink, TeamsResidentEvent,
};
use std::{future::Future, pin::Pin};

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
