//! The production admission state machine. Network evaluation never owns the ingress loop.

use super::{config::AttentionMode, types::*};
use crate::{
    config::LoadedConfig,
    discord::events::{MessageEvent, NotificationEvent},
};
use serenity::model::id::ChannelId;
use std::collections::{BTreeMap, VecDeque};
use tokio_util::sync::CancellationToken;

/// Source-bound evaluation work with cancellation tied to its admission lifetime.
pub struct JudgmentWork {
    pub record: DecisionRecord,
    pub mode: AttentionMode,
    pub cancel: CancellationToken,
}

/// Routing result that keeps ordinary delivery separate from optional judgment work.
pub enum Submission {
    Ordinary(NotificationEvent),
    Unknown {
        event: NotificationEvent,
        reason: &'static str,
    },
    Judge {
        work: Box<JudgmentWork>,
        ordinary: Option<NotificationEvent>,
    },
    Duplicate,
    Saturated {
        event: NotificationEvent,
    },
}

/// A ready event paired with the decision that must be persisted before routing.
pub struct AdmissionResult {
    pub event: MessageEvent,
    pub record: DecisionRecord,
}

struct Pending {
    event: MessageEvent,
    record: DecisionRecord,
    cancel: CancellationToken,
    ready: bool,
}

/// Process-local work ownership and per-channel ordering for already-authorized ingress.
pub struct AdmissionController {
    incarnation: IncarnationId,
    sequence: u64,
    pending: BTreeMap<RecordId, Pending>,
    jobs: BTreeMap<RecordId, (SourceKey, CancellationToken)>,
    channels: BTreeMap<ChannelId, VecDeque<RecordId>>,
}

impl Default for AdmissionController {
    fn default() -> Self {
        Self {
            incarnation: uuid::Uuid::new_v4().to_string().into(),
            sequence: 0,
            pending: BTreeMap::new(),
            jobs: BTreeMap::new(),
            channels: BTreeMap::new(),
        }
    }
}

impl AdmissionController {
    /// Identifies this controller lifetime so stale completions cannot release new work.
    pub fn incarnation(&self) -> &IncarnationId {
        &self.incarnation
    }
    /// Counts outstanding evaluation jobs, including cancelled jobs not yet reconciled.
    pub fn in_flight(&self) -> usize {
        self.jobs.len()
    }
    /// Checks whether the controller still owns the named evaluation job.
    pub fn owns_work(&self, id: &RecordId) -> bool {
        self.jobs.contains_key(id)
    }

    /// Checks both evaluating and committed-but-deferred ownership for a source.
    pub fn owns_source(&self, key: &SourceKey) -> bool {
        self.jobs.values().any(|(owned, _)| owned == key)
            || self.pending.values().any(|pending| {
                pending
                    .record
                    .sources
                    .iter()
                    .any(|source| &source.key == key)
            })
    }

    /// The caller supplies events only after the existing ingress and rate-limit gates.
    /// Off/direct/ineligible work does not allocate a request or wait for a judgment.
    pub fn submit(
        &mut self,
        event: NotificationEvent,
        config: &LoadedConfig,
        now: u64,
        enforcement_supported: bool,
    ) -> Submission {
        let NotificationEvent::Message(message) = event else {
            return Submission::Ordinary(event);
        };
        let key = SourceKey {
            channel_id: message.chat_id,
            message_id: message.message_id,
        };
        if self.owns_source(&key) {
            return Submission::Duplicate;
        }
        let attention = &config.raw.attention;
        let configured_mode = attention.effective_mode(message.chat_id);
        if configured_mode == AttentionMode::Off
            || message.targeting.is_directed()
            || attention.direct_room(message.chat_id)
            || !attention.provider_eligible(message.chat_id)
        {
            return Submission::Ordinary(NotificationEvent::Message(message));
        }
        let unknown = if attention.validate().is_err() {
            Some("invalid attention configuration")
        } else if !attention
            .brief
            .as_ref()
            .is_some_and(|brief| brief.usable(now))
        {
            Some("attention brief missing, stale, or provider-ineligible")
        } else if !message.attachments.is_empty() {
            Some("attachment evidence unavailable")
        } else {
            None
        };
        if let Some(reason) = unknown {
            return Submission::Unknown {
                event: NotificationEvent::Message(message),
                reason,
            };
        }
        // A push-only adapter cannot enforce ambient safe-turn scheduling. Keep
        // ordinary delivery while evaluating shadows; metadata is not a capability.
        let mode = if configured_mode == AttentionMode::On && !enforcement_supported {
            AttentionMode::Log
        } else {
            configured_mode
        };
        if self.jobs.len() >= attention.max_in_flight {
            return Submission::Saturated {
                event: NotificationEvent::Message(message),
            };
        }
        self.sequence = self.sequence.saturating_add(1);
        let record = DecisionRecord {
            id: format!("{}-{:016x}", self.incarnation, self.sequence).into(),
            recipient: attention.recipient.clone(),
            sources: vec![SourceVersion {
                key,
                author_id: message.user_id,
                author_kind: message.author_kind,
                conversation: format!("channel:{}", message.chat_id),
                content_hash: content_hash(&message.content),
                observed_at_ms: now,
            }],
            compatibility: attention.compatibility(),
            config_generation: config.generation(),
            incarnation: self.incarnation.clone(),
            created_at_ms: now,
            expires_at_ms: now.saturating_add(attention.retention_ms),
            judgment: None,
            policy_digest: None,
            hypothetical: Admission::Unknown,
            actual: if mode == AttentionMode::Log {
                Admission::Ordinary
            } else {
                Admission::Unknown
            },
            delivery: if mode == AttentionMode::Log {
                DeliveryState::Observed
            } else {
                DeliveryState::Held
            },
            selection_probability: None,
        };
        let cancel = CancellationToken::new();
        let work = JudgmentWork {
            record: record.clone(),
            mode,
            cancel: cancel.clone(),
        };
        self.jobs.insert(record.id.clone(), (key, cancel.clone()));
        if mode == AttentionMode::On {
            self.channels
                .entry(message.chat_id)
                .or_default()
                .push_back(record.id.clone());
            self.pending.insert(
                record.id.clone(),
                Pending {
                    event: message,
                    record,
                    cancel,
                    ready: false,
                },
            );
            Submission::Judge {
                work: Box::new(work),
                ordinary: None,
            }
        } else {
            Submission::Judge {
                work: Box::new(work),
                ordinary: Some(NotificationEvent::Message(message)),
            }
        }
    }

    /// Completion is metadata, not permission. The runtime fences current source/access
    /// and durably records the chosen outcome before routing returned events to sinks.
    pub fn complete(
        &mut self,
        record: DecisionRecord,
        config: &LoadedConfig,
    ) -> Vec<AdmissionResult> {
        if record.incarnation != self.incarnation {
            return Vec::new();
        }
        self.jobs.remove(&record.id);
        let Some(pending) = self.pending.get_mut(&record.id) else {
            return Vec::new();
        };
        if record.incarnation != self.incarnation
            || record.config_generation != pending.record.config_generation
        {
            return Vec::new();
        }
        if record.config_generation != config.generation()
            || record.compatibility != config.raw.attention.compatibility()
        {
            pending.record.actual = Admission::Ordinary;
            pending.record.judgment = None;
        } else {
            pending.record = record;
        }
        pending.ready = true;
        self.drain_ready()
    }

    /// Requests cancellation without releasing pending events; configuration reconciliation owns release.
    pub fn cancel_work(&self) {
        for (_, cancel) in self.jobs.values() {
            cancel.cancel();
        }
    }

    /// Retains committed outcomes and releases remaining undecided work ordinarily in order.
    pub fn configuration_changed(
        &mut self,
        committed: Vec<DecisionRecord>,
    ) -> Vec<AdmissionResult> {
        for record in committed {
            if let Some(pending) = self.pending.get_mut(&record.id)
                && record.incarnation == self.incarnation
                && record.config_generation == pending.record.config_generation
            {
                pending.record = record;
                pending.ready = true;
            }
        }
        for (_, cancel) in self.jobs.values() {
            cancel.cancel();
        }
        self.jobs.clear();
        for pending in self.pending.values_mut() {
            pending.cancel.cancel();
            if pending.ready {
                continue;
            }
            pending.ready = true;
            pending.record.actual = Admission::Ordinary;
            pending.record.hypothetical = Admission::Unknown;
            pending.record.judgment = None;
        }
        self.drain_ready()
    }

    /// Deleted/edited old versions are never released merely because cancellation
    /// completed. Invalidating a blocked head also frees later conversation work.
    pub fn invalidate(&mut self, key: SourceKey) -> Vec<AdmissionResult> {
        let mut ids: Vec<_> = self
            .pending
            .iter()
            .filter(|(_, pending)| {
                pending
                    .record
                    .sources
                    .iter()
                    .any(|source| source.key == key)
            })
            .map(|(id, _)| id.clone())
            .collect();
        ids.extend(
            self.jobs
                .iter()
                .filter(|(_, (source, _))| *source == key)
                .map(|(id, _)| id.clone()),
        );
        ids.sort_unstable();
        ids.dedup();
        for id in ids {
            if let Some((_, cancel)) = self.jobs.remove(&id) {
                cancel.cancel();
            }
            if let Some(pending) = self.pending.remove(&id) {
                pending.cancel.cancel();
            }
            for queue in self.channels.values_mut() {
                queue.retain(|queued| queued != &id);
            }
        }
        self.drain_ready()
    }

    /// Restores committed live-incarnation work when its final authority check was
    /// temporarily unavailable. Recovery-owned rows remain in the durable store and
    /// are retried by the existing recovery cycle instead.
    pub fn defer(&mut self, results: Vec<AdmissionResult>) {
        for result in results.into_iter().rev() {
            if result.record.incarnation != self.incarnation {
                continue;
            }
            let id = result.record.id.clone();
            let channel = result.event.chat_id;
            self.channels
                .entry(channel)
                .or_default()
                .push_front(id.clone());
            self.pending.insert(
                id,
                Pending {
                    event: result.event,
                    record: result.record,
                    cancel: CancellationToken::new(),
                    ready: true,
                },
            );
        }
    }

    /// Releases temporarily deferred work through the ordinary per-channel FIFO.
    pub fn retry_ready(&mut self) -> Vec<AdmissionResult> {
        self.drain_ready()
    }

    fn drain_ready(&mut self) -> Vec<AdmissionResult> {
        let mut ready = Vec::new();
        for queue in self.channels.values_mut() {
            while let Some(id) = queue.front() {
                if !self.pending.get(id).is_some_and(|pending| pending.ready) {
                    break;
                }
                let Some(id) = queue.pop_front() else {
                    break;
                };
                self.jobs.remove(&id);
                if let Some(pending) = self.pending.remove(&id) {
                    ready.push(AdmissionResult {
                        event: pending.event,
                        record: pending.record,
                    });
                }
            }
        }
        self.channels.retain(|_, queue| !queue.is_empty());
        ready
    }
}

impl Drop for AdmissionController {
    fn drop(&mut self) {
        for (_, cancel) in self.jobs.values() {
            cancel.cancel();
        }
    }
}
