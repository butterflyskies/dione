use dione::codex::{
    CodexDeliveryError, CodexDeliveryErrorKind, CodexQueueError, CodexQueueErrorKind, QueueError,
    QueueErrorKind,
};

#[test]
fn codex_queue_error_can_be_matched_exhaustively() {
    let error = CodexQueueError::from(QueueError::new(
        "fixture",
        QueueErrorKind::UnknownDeliveryToken,
    ));
    let description = match error.kind {
        CodexQueueErrorKind::Durable(error) => match error.kind {
            QueueErrorKind::CompletionUnknown(_) => "unknown queue completion",
            QueueErrorKind::Io(_)
            | QueueErrorKind::Locked(_)
            | QueueErrorKind::Codec(_)
            | QueueErrorKind::PriorSync(_)
            | QueueErrorKind::DurabilityUncertain(_)
            | QueueErrorKind::Identity(_)
            | QueueErrorKind::Integrity(_)
            | QueueErrorKind::AllocationExhausted { .. }
            | QueueErrorKind::InvalidStorageLayout
            | QueueErrorKind::UnknownConsumer
            | QueueErrorKind::NotPrimaryConsumer
            | QueueErrorKind::PrimaryConsumerExists
            | QueueErrorKind::UnknownDeliveryToken
            | QueueErrorKind::NotDeferrable
            | QueueErrorKind::ResidentUnavailable
            | QueueErrorKind::ReplayCapacity => "Codex queue error",
        },
        CodexQueueErrorKind::LegacyAttention(_) | CodexQueueErrorKind::InvalidThreadId => {
            "Codex queue error"
        }
    };
    assert_eq!(description, "Codex queue error");
}

#[test]
fn codex_delivery_error_can_be_matched_exhaustively() {
    let error = CodexDeliveryError::from(CodexDeliveryErrorKind::ActiveTurnUnknown);
    let description = match error.kind {
        CodexDeliveryErrorKind::ActiveTurnUnknown => "active turn unknown",
        CodexDeliveryErrorKind::MissingHome
        | CodexDeliveryErrorKind::Connect { .. }
        | CodexDeliveryErrorKind::Handshake { .. }
        | CodexDeliveryErrorKind::HandshakeTimeout { .. }
        | CodexDeliveryErrorKind::Timeout { .. }
        | CodexDeliveryErrorKind::Disconnected { .. }
        | CodexDeliveryErrorKind::Rejected { .. }
        | CodexDeliveryErrorKind::Decode { .. }
        | CodexDeliveryErrorKind::Protocol { .. }
        | CodexDeliveryErrorKind::UnsupportedThreadStatus { .. }
        | CodexDeliveryErrorKind::Queue(_) => "delivery failed",
    };
    assert_eq!(description, "active turn unknown");
}
