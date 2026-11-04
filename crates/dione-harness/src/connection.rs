//! A complete harness connection with coupled delivery and protocol facets.
use crate::ProtocolOutput;
use dione_contracts::MessageRef;
use futures_util::future::BoxFuture;
use std::sync::Arc;
use thiserror::Error;
use {crate::HarnessProtocol, dione_messages::delivery::Delivery};

/// Determines whether managed delivery requires a safe harness turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttentionPolicy {
    /// Notifications can be delivered directly.
    Direct,
    /// The harness must fence delivery at a safe turn.
    SafeTurn,
}

/// One harness adapter and its application-supplied MCP output.
///
/// Both facets come from one adapter instance. Application factories pass the
/// same output used by their adapter. The fields prevent independent facet
/// replacement; an arbitrary adapter's internal behavior remains its contract.
///
/// ```compile_fail
/// use dione_harness::HarnessConnection;
/// fn delivery(connection: &HarnessConnection) { let _ = &connection.delivery; }
/// ```
///
/// ```compile_fail
/// use dione_harness::HarnessConnection;
/// fn protocol(connection: &HarnessConnection) { let _ = &connection.protocol; }
/// ```
#[derive(Clone)]
pub struct HarnessConnection {
    delivery: Arc<dyn HarnessDelivery>,
    protocol: Arc<dyn HarnessProtocol>,
    output: ProtocolOutput,
}
impl HarnessConnection {
    /// Derive both facets from one adapter allocation and retain its supplied output.
    /// The caller passes the same output its adapter uses for notifications.
    ///
    /// ```
    /// use dione_harness::{HarnessConnection, HarnessDelivery, HarnessProtocol, ProtocolOutput};
    /// fn connect<A: HarnessDelivery + HarnessProtocol + 'static>(adapter: A, output: ProtocolOutput) -> HarnessConnection {
    ///     HarnessConnection::from_adapter(adapter, output)
    /// }
    /// ```
    pub fn from_adapter<A: HarnessDelivery + HarnessProtocol + 'static>(
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
    /// Borrow the adapter's protocol facet; application dispatch authorizes requests.
    pub fn protocol(&self) -> &dyn HarnessProtocol {
        self.protocol.as_ref()
    }
    /// Borrow the output supplied by application composition.
    pub fn output(&self) -> &ProtocolOutput {
        &self.output
    }
    /// Attempt delivery through the adapter, retaining its failure classification.
    pub async fn deliver(&self, value: &Delivery) -> Result<(), HarnessError> {
        self.delivery.deliver(value).await
    }
    /// Forward source invalidation to the same adapter instance.
    pub async fn invalidate_attention(&self, source: MessageRef) -> Result<(), HarnessError> {
        self.delivery.invalidate_attention(source).await
    }
}
/// Delivery and invalidation operations supplied by one application adapter.
pub trait HarnessDelivery: Send + Sync {
    /// Report the turn-fencing policy required by this adapter.
    fn attention_policy(&self) -> AttentionPolicy;
    /// Attempt one delivery and report its classified failure.
    fn deliver<'a>(&'a self, value: &'a Delivery) -> BoxFuture<'a, Result<(), HarnessError>>;
    /// Invalidate retained attention work for the supplied source.
    fn invalidate_attention(&self, source: MessageRef) -> BoxFuture<'_, Result<(), HarnessError>>;
}
#[derive(Debug, Error)]
#[error("{source}")]
/// An adapter-classified delivery or protocol failure with its cause retained.
pub struct HarnessError {
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
    /// Wrap a cause using the adapter's ordinary failure classification.
    pub fn source(error: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self {
            kind: HarnessErrorKind::Failed,
            source: Box::new(error),
        }
    }

    /// Wrap a cause when the adapter cannot establish whether delivery completed.
    pub fn completion_unknown(error: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self {
            kind: HarnessErrorKind::CompletionUnknown,
            source: Box::new(error),
        }
    }

    /// Return whether the adapter classified completion as unknown.
    pub fn completion_is_unknown(&self) -> bool {
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

    struct CountingAdapter(std::sync::atomic::AtomicUsize);

    impl HarnessDelivery for CountingAdapter {
        fn attention_policy(&self) -> AttentionPolicy {
            AttentionPolicy::Direct
        }

        fn deliver<'a>(&'a self, _value: &'a Delivery) -> BoxFuture<'a, Result<(), HarnessError>> {
            Box::pin(async move {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            })
        }

        fn invalidate_attention(
            &self,
            _source: MessageRef,
        ) -> BoxFuture<'_, Result<(), HarnessError>> {
            Box::pin(async { Ok(()) })
        }
    }

    impl crate::HarnessProtocol for CountingAdapter {
        fn capabilities(&self) -> serde_json::Value {
            serde_json::json!({"deliveries": self.0.load(std::sync::atomic::Ordering::SeqCst)})
        }

        fn tools(&self) -> Vec<serde_json::Value> {
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
            _args: &'a serde_json::Value,
        ) -> BoxFuture<'a, Option<Result<serde_json::Value, HarnessError>>> {
            Box::pin(async { None })
        }
    }

    #[tokio::test]
    async fn facets_share_one_adapter_instance_across_clones() {
        let connection = HarnessConnection::from_adapter(
            CountingAdapter(std::sync::atomic::AtomicUsize::new(0)),
            ProtocolOutput::new(tokio::io::sink()),
        );
        let clone = connection.clone();
        let delivery = Delivery::control(
            dione_messages::events::ControlEvent::Trace {
                level: dione_messages::events::TraceLevel::INFO,
                target: "fixture".to_owned(),
                message: "coupled facets".to_owned(),
                fields: Vec::new(),
            },
            dione_messages::delivery::PresentationContext::new(
                None,
                dione_messages::delivery::EvidenceVisibility::Disabled,
            ),
        );
        clone.deliver(&delivery).await.unwrap();
        assert_eq!(
            connection.protocol().capabilities(),
            serde_json::json!({"deliveries": 1})
        );
        connection.deliver(&delivery).await.unwrap();
        assert_eq!(
            clone.protocol().capabilities(),
            serde_json::json!({"deliveries": 2})
        );
    }
}
