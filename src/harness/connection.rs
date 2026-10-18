//! A complete harness connection with coupled delivery and protocol facets.
use super::ProtocolOutput;
use crate::{
    core::{MessageRef, delivery::Delivery},
    mcp::harness_wire::HarnessProtocol,
};
use futures_util::future::BoxFuture;
use std::sync::Arc;
use thiserror::Error;

/// Determines whether managed delivery requires a safe harness turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttentionPolicy {
    /// Notifications can be delivered directly.
    Direct,
    /// The harness must fence delivery at a safe turn.
    SafeTurn,
}

/// One harness adapter and its shared MCP output.
#[derive(Clone)]
pub struct HarnessConnection {
    delivery: Arc<dyn HarnessDelivery>,
    protocol: Arc<dyn HarnessProtocol>,
    output: ProtocolOutput,
}
impl HarnessConnection {
    pub(crate) fn from_adapter<A: HarnessDelivery + HarnessProtocol + 'static>(
        adapter: A,
        output: ProtocolOutput,
    ) -> Self {
        let adapter = Arc::new(adapter);
        Self {
            delivery: adapter.clone(),
            protocol: adapter,
            output,
        }
    }
    /// Returns the attention policy required by this connection.
    pub fn attention_policy(&self) -> AttentionPolicy {
        self.delivery.attention_policy()
    }
    pub(crate) fn protocol(&self) -> &dyn HarnessProtocol {
        self.protocol.as_ref()
    }
    pub(crate) fn output(&self) -> &ProtocolOutput {
        &self.output
    }
    pub(crate) async fn deliver(&self, value: &Delivery) -> Result<(), HarnessError> {
        self.delivery.deliver(value).await
    }
    pub(crate) async fn invalidate_attention(
        &self,
        source: MessageRef,
    ) -> Result<(), HarnessError> {
        self.delivery.invalidate_attention(source).await
    }
}
pub(crate) trait HarnessDelivery: Send + Sync {
    fn attention_policy(&self) -> AttentionPolicy;
    fn deliver<'a>(&'a self, value: &'a Delivery) -> BoxFuture<'a, Result<(), HarnessError>>;
    fn invalidate_attention(&self, source: MessageRef) -> BoxFuture<'_, Result<(), HarnessError>>;
}
#[derive(Debug, Error)]
#[error("{source}")]
pub(crate) struct HarnessError {
    kind: HarnessErrorKind,
    #[source]
    source: Box<dyn std::error::Error + Send + Sync>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HarnessErrorKind {
    Failed,
    CompletionUnknown,
}

impl HarnessError {
    pub(crate) fn source(error: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self {
            kind: HarnessErrorKind::Failed,
            source: Box::new(error),
        }
    }

    pub(crate) fn completion_unknown(
        error: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind: HarnessErrorKind::CompletionUnknown,
            source: Box::new(error),
        }
    }

    pub(crate) fn completion_is_unknown(&self) -> bool {
        self.kind == HarnessErrorKind::CompletionUnknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{error::Error, io};
    #[test]
    fn delivery_error_preserves_upstream_source() {
        let error = HarnessError::source(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "queue denied",
        ));
        assert_eq!(error.to_string(), "queue denied");
        let source = error.source().unwrap().downcast_ref::<io::Error>().unwrap();
        assert_eq!(source.kind(), io::ErrorKind::PermissionDenied);
    }
}
