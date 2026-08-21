//! A tracing [`Layer`] that forwards events as [`NotificationEvent::Trace`] values.
//!
//! Events captured by this layer are sent over an mpsc channel to the MCP
//! notification pipeline, where they appear as channel notifications with
//! `type="trace"` metadata — differentiated from Discord events.

use crate::discord::events::NotificationEvent;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::Subscriber;
use tracing_subscriber::{EnvFilter, Layer, registry::LookupSpan, reload};

// ── TraceLevelController ─────────────────────────────────────────────────────

type ReloadFn = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

/// Type-erased controller for reloading tracing filters at runtime.
///
/// Wraps reload handles behind closures so the concrete subscriber type
/// doesn't leak into the server struct.
pub struct TraceLevelController {
    reload_stderr: ReloadFn,
    reload_channel: ReloadFn,
}

impl TraceLevelController {
    /// Create a controller from two reload handles (type-erased via closures).
    pub fn new<S1, S2>(
        stderr_handle: reload::Handle<EnvFilter, S1>,
        channel_handle: reload::Handle<EnvFilter, S2>,
    ) -> Self
    where
        S1: Subscriber + 'static,
        S2: Subscriber + 'static,
    {
        let reload_stderr: ReloadFn = Arc::new(move |filter: &str| {
            let f = EnvFilter::try_new(filter).map_err(|e| format!("invalid filter: {e}"))?;
            stderr_handle
                .reload(f)
                .map_err(|e| format!("reload failed: {e}"))
        });
        let reload_channel: ReloadFn = Arc::new(move |filter: &str| {
            let f = EnvFilter::try_new(filter).map_err(|e| format!("invalid filter: {e}"))?;
            channel_handle
                .reload(f)
                .map_err(|e| format!("reload failed: {e}"))
        });
        Self {
            reload_stderr,
            reload_channel,
        }
    }

    /// Create a no-op controller for tests.
    ///
    /// Still validates the filter string but doesn't apply it to any subscriber.
    pub fn noop() -> Self {
        Self {
            reload_stderr: Arc::new(|filter: &str| {
                EnvFilter::try_new(filter).map_err(|e| format!("invalid filter: {e}"))?;
                Ok(())
            }),
            reload_channel: Arc::new(|filter: &str| {
                EnvFilter::try_new(filter).map_err(|e| format!("invalid filter: {e}"))?;
                Ok(())
            }),
        }
    }

    /// Reload the stderr tracing filter.
    pub fn set_stderr_filter(&self, filter: &str) -> Result<(), String> {
        (self.reload_stderr)(filter)
    }

    /// Reload the channel-forwarding tracing filter.
    pub fn set_channel_filter(&self, filter: &str) -> Result<(), String> {
        (self.reload_channel)(filter)
    }
}

/// A tracing layer that sends events to an mpsc channel as [`NotificationEvent::Trace`].
pub struct TracingChannelLayer {
    tx: mpsc::Sender<NotificationEvent>,
}

impl TracingChannelLayer {
    /// Create the layer and its receiving end.
    ///
    /// The receiver yields `NotificationEvent::Trace` values that should be
    /// forwarded to the MCP notification stream alongside Discord events.
    pub fn new() -> (Self, mpsc::Receiver<NotificationEvent>) {
        let (tx, rx) = mpsc::channel(256);
        (Self { tx }, rx)
    }
}

impl<S> Layer<S> for TracingChannelLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let metadata = event.metadata();
        let level = metadata.level().to_string();
        let target = metadata.target().to_string();

        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        let message = visitor.message.unwrap_or_default();

        let trace_event = NotificationEvent::Trace {
            level,
            target,
            message,
            fields: visitor.fields,
        };

        // Best-effort send — don't block the tracing pipeline.
        let _ = self.tx.try_send(trace_event);
    }
}

/// Visitor that extracts the message field and any additional key-value pairs.
#[derive(Default)]
struct MessageVisitor {
    message: Option<String>,
    fields: Vec<(String, String)>,
}

impl tracing::field::Visit for MessageVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let formatted = format!("{value:?}");
        if field.name() == "message" {
            self.message = Some(formatted);
        } else {
            self.fields.push((field.name().to_string(), formatted));
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.to_string());
        } else {
            self.fields
                .push((field.name().to_string(), value.to_string()));
        }
    }
}
