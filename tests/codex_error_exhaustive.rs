use dione::codex::CodexQueueError;

// This integration crate checks the public enum from a downstream caller's
// perspective. New variants require an explicit compatibility/version decision.
#[test]
fn codex_queue_error_can_be_matched_exhaustively() {
    let error = CodexQueueError::InvalidDeliveryToken;
    let description = match error {
        CodexQueueError::InboxIo { .. }
        | CodexQueueError::InboxDurabilityUncertain { .. }
        | CodexQueueError::InboxDecode { .. }
        | CodexQueueError::InboxLocked { .. }
        | CodexQueueError::InvalidDeliveryToken
        | CodexQueueError::InvalidConsumerId
        | CodexQueueError::InvalidThreadId
        | CodexQueueError::UnknownConsumer
        | CodexQueueError::NotPrimaryConsumer
        | CodexQueueError::PrimaryConsumerExists
        | CodexQueueError::UnknownDeliveryToken
        | CodexQueueError::MalformedAttentionMetadata { .. }
        | CodexQueueError::AttentionSelectorRequired
        | CodexQueueError::NotAttentionManaged => "Codex queue error",
    };
    assert_eq!(description, "Codex queue error");
}
