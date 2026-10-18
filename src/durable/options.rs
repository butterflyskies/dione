//! Immutable owner configuration for storage names and replay retention.
use super::ReplayClass;
use chrono::Duration;
/// Queue configuration is fixed at open and cannot vary with an admission.
#[derive(Clone)]
pub struct QueueOptions<K> {
    /// Snapshot filename relative to the state directory.
    pub file_name: &'static str,
    /// Exclusive lock filename relative to the state directory.
    pub lock_name: &'static str,
    /// Prefix used for newly generated consumer identities.
    pub consumer_prefix: &'static str,
    /// Diagnostic resident registration label; never a credential.
    pub resident_label: &'static str,
    /// Immutable retention classification for each exact key.
    pub replay_class: fn(&K) -> ReplayClass,
    /// Maximum acknowledged rolling keys; queued keys do not consume this limit.
    pub rolling_limit: usize,
    /// Maximum queued plus acknowledged timed keys.
    pub timed_limit: usize,
    /// Acknowledged timed-key lifetime.
    pub timed_retention: Duration,
}
