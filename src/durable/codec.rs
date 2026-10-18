//! Serialization boundary selected by the queue owner.
use super::{CodecError, QueueSnapshot};
/// Complete snapshot serialization; implementations preserve their own disk schema.
pub trait QueueCodec: Send + Sync {
    /// Adapter-owned immutable entry payload.
    type Payload;
    /// Exact replay identity, interpreted only by the configured classifier.
    type Key;
    /// Decode and validate adapter-specific fields without modifying the source file.
    fn decode(&self, bytes: &[u8]) -> Result<QueueSnapshot<Self::Payload, Self::Key>, CodecError>;
    /// Encode the complete retained snapshot before any visibility change.
    fn encode(
        &self,
        snapshot: &QueueSnapshot<Self::Payload, Self::Key>,
    ) -> Result<Vec<u8>, CodecError>;
}
