//! Typed commands on the authenticated recipient's local control connection.
//! Discord content is never parsed as one of these commands.

use super::{
    config::AttentionConfig,
    learning::{EvaluationPlan, FitOptions, FixedThresholdPolicy, TemporalPartitionSpec},
    runtime::AttentionRuntime,
    source::now_ms,
    types::{ArtifactDigest, DecisionRecord, EvaluationId, Feedback, FeedbackLabel, RecordId},
};
use crate::config::{ConfigDurability, ConfigRuntime, attention_effect_guard, load_config};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;

/// Typed operations accepted only through the authenticated local control surface.
#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(Serialize))]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum AttentionCommand {
    Status,
    Configure {
        settings: AttentionConfig,
    },
    Retrieve {
        record_id: RecordId,
    },
    Review {
        requested: usize,
        seed: u64,
    },
    Feedback {
        record_id: RecordId,
        label: FeedbackLabel,
    },
    Partitions {
        spec: TemporalPartitionSpec,
    },
    Replay {
        record_ids: Vec<RecordId>,
        policy: FixedThresholdPolicy,
    },
    Fit {
        record_ids: Vec<RecordId>,
        options: FitOptions,
    },
    OpenEvaluation {
        plan: EvaluationPlan,
    },
    Evaluate {
        evaluation_id: EvaluationId,
        digest: ArtifactDigest,
    },
    Promote {
        evaluation_id: EvaluationId,
        digest: ArtifactDigest,
    },
    Rollback {
        digest: ArtifactDigest,
    },
}

impl AttentionCommand {
    /// Identifies commands requiring mutation authority, including durable review selection.
    pub fn is_mutating(&self) -> bool {
        !matches!(
            self,
            Self::Status | Self::Retrieve { .. } | Self::Partitions { .. } | Self::Replay { .. }
        )
    }
}

fn encoded<T: Serialize>(value: T) -> Result<Value, String> {
    serde_json::to_value(value).map_err(|_| "attention result serialization failed".to_owned())
}

async fn records(
    runtime: &Arc<AttentionRuntime>,
    ids: Vec<RecordId>,
) -> Result<Vec<DecisionRecord>, String> {
    let recipient = load_config(&runtime.resolver.state_dir)
        .raw
        .attention
        .recipient
        .clone();
    let selected: Vec<DecisionRecord> = runtime
        .with_store(move |store| {
            if ids.len() > store.limits().max_members_per_artifact {
                return Err("attention record selection exceeds its bound".to_owned());
            }
            ids.iter()
                .map(|id| {
                    store
                        .record(id)
                        .filter(|record| record.recipient == recipient)
                        .cloned()
                        .ok_or_else(|| "attention source unavailable".to_owned())
                })
                .collect()
        })
        .await?;
    if selected
        .iter()
        .any(|record| record.expires_at_ms <= now_ms())
    {
        return Err("attention source unavailable".to_owned());
    }
    let sources: Vec<_> = selected
        .iter()
        .flat_map(|record| record.sources.iter().cloned())
        .collect();
    runtime
        .validate_sources(&sources)
        .await
        .map_err(|_| "attention source unavailable".to_owned())?;
    Ok(selected)
}

async fn validate_artifact(
    runtime: &Arc<AttentionRuntime>,
    digest: ArtifactDigest,
    evaluation: Option<EvaluationId>,
) -> Result<(), String> {
    let recipient = load_config(&runtime.resolver.state_dir)
        .raw
        .attention
        .recipient
        .clone();
    let ids = runtime
        .with_store(move |store| {
            let artifact = store
                .artifact(&digest)
                .filter(|artifact| artifact.recipient == recipient)
                .ok_or_else(|| "attention artifact unavailable".to_owned())?;
            let mut ids = artifact.training_records.clone();
            if let Some(evaluation) = evaluation {
                let evaluation = store
                    .opened_evaluation(&evaluation)
                    .filter(|evaluation| evaluation.recipient == recipient)
                    .ok_or_else(|| "attention evaluation unavailable".to_owned())?;
                ids.extend(evaluation.record_ids.iter().cloned());
            }
            // Rollback must also validate the held-out sources supporting its earlier promotion.
            for record in store.list_records(&recipient) {
                if record.sources.iter().any(|source| {
                    artifact.promotion_sources.iter().any(|member| {
                        member.key == source.key && member.content_hash == source.content_hash
                    })
                }) {
                    ids.push(record.id.clone());
                }
            }
            ids.sort_unstable();
            ids.dedup();
            Ok(ids)
        })
        .await?;
    records(runtime, ids).await.map(|_| ())
}

async fn control_write<R, F>(
    runtime: &Arc<AttentionRuntime>,
    expected_generation: u64,
    operation: F,
) -> Result<R, String>
where
    R: Send + 'static,
    F: FnOnce(&mut super::store::AttentionStore) -> Result<R, String> + Send + 'static,
{
    let publication = attention_effect_guard().await;
    let current = load_config(&runtime.resolver.state_dir);
    if current.generation() != expected_generation || current.access.admin_only_mutations {
        return Err("attention control authority changed during validation".to_owned());
    }
    runtime
        .with_store(move |store| {
            // The disk writer, not its cancellable async waiter, owns this permit.
            let _publication = publication;
            operation(store)
        })
        .await
}

/// Executes a seat-owned command, rechecking mutation authority at durable write boundaries.
pub async fn execute(
    runtime: Arc<AttentionRuntime>,
    command: AttentionCommand,
    admin_allowed: bool,
) -> Result<Value, String> {
    if command.is_mutating() && !admin_allowed {
        return Err(
            "admin_only_mutations is enabled; attention mutation tools are disabled".to_owned(),
        );
    }
    let initial = load_config(&runtime.resolver.state_dir);
    let recipient = initial.raw.attention.recipient.clone();
    match command {
        AttentionCommand::Status => {
            let owner = recipient.clone();
            let compatibility = initial.raw.attention.compatibility();
            let state = runtime
                .with_store(move |store| {
                    Ok(json!({
                        "records": store.list_records(&owner).len(),
                        "active_digest": store.active_artifact(&owner)
                            .filter(|artifact| artifact.compatibility == compatibility)
                            .map(|artifact| artifact.digest.clone()),
                        "limits": store.limits(),
                    }))
                })
                .await;
            Ok(
                json!({"settings": initial.raw.attention, "generation": initial.generation(),
                "safe_delivery_supported": runtime.enforcement_supported, "health": runtime.health(),
                "store": state.as_ref().ok(), "store_error": state.err()}),
            )
        }
        AttentionCommand::Configure { settings } => {
            settings.validate()?;
            settings.notice_destination(initial.raw.attention_notice_route.as_ref())?;
            if settings.recipient != recipient {
                return Err(
                    "recipient identity cannot be changed through its attention tool".to_owned(),
                );
            }
            let path = runtime.resolver.state_dir.clone();
            let outcome = ConfigRuntime::new(path.clone())
                .mutate(move |editor| {
                    let current = load_config(&path);
                    if current.access.admin_only_mutations
                        || current.raw.attention.recipient != settings.recipient
                    {
                        return Err(
                            std::io::Error::other("attention control authority changed").into()
                        );
                    }
                    settings
                        .notice_destination(current.raw.attention_notice_route.as_ref())
                        .map_err(std::io::Error::other)?;
                    editor.set_attention(&settings)
                })
                .await
                .map_err(|error| error.to_string())?;
            let (durable, warning) = match outcome.durability {
                ConfigDurability::Durable => (true, None),
                ConfigDurability::Unknown { warning } => (false, Some(warning)),
            };
            Ok(json!({"generation": outcome.generation, "durable": durable, "warning": warning}))
        }
        AttentionCommand::Retrieve { record_id } => runtime.retrieve(record_id).await,
        AttentionCommand::Review { requested, seed } => {
            let owner = recipient.clone();
            let batch = control_write(&runtime, initial.generation(), move |store| {
                store
                    .sample_review_batch(&owner, requested, seed)
                    .map_err(|error| error.to_string())
            })
            .await?;
            let mut visible = Vec::new();
            let mut unavailable = 0;
            for item in &batch.items {
                match runtime.retrieve(item.record_id.clone()).await {
                    Ok(source) => visible.push(json!({"selection": item, "source": source})),
                    Err(_) => unavailable += 1,
                }
            }
            Ok(
                json!({"items": visible, "unavailable_selected": unavailable,
                "coverage": {"requested": requested, "selected": batch.items.len(), "returned": visible.len()},
                "sampling": "stratified; each returned selection carries its actual inclusion probability"}),
            )
        }
        AttentionCommand::Feedback { record_id, label } => {
            let mut selected = records(&runtime, vec![record_id.clone()]).await?;
            let record = selected
                .pop()
                .ok_or_else(|| "attention source unavailable".to_owned())?;
            let feedback = Feedback {
                record_id,
                annotator: recipient.clone(),
                label,
                assessed_at_ms: now_ms(),
                source_versions: record.sources,
            };
            control_write(&runtime, initial.generation(), move |store| {
                store
                    .label(&recipient, feedback)
                    .map(|_| json!({"recorded": true}))
                    .map_err(|error| error.to_string())
            })
            .await
        }
        AttentionCommand::Partitions { spec } => {
            let owner = recipient.clone();
            let ids = runtime
                .with_store(move |store| {
                    Ok(store
                        .list_records(&owner)
                        .iter()
                        .map(|record| record.id.clone())
                        .collect())
                })
                .await?;
            records(&runtime, ids).await?;
            runtime
                .with_store(move |store| {
                    encoded(
                        store
                            .temporal_partitions(&recipient, spec)
                            .map_err(|error| error.to_string())?,
                    )
                })
                .await
        }
        AttentionCommand::Replay { record_ids, policy } => {
            records(&runtime, record_ids.clone()).await?;
            runtime.with_store(move |store| {
                let mut output = Vec::with_capacity(record_ids.len());
                for id in record_ids { output.push(json!({"record_id": id, "admission": store.replay_fixed(&id, policy).map_err(|error| error.to_string())?})); }
                Ok(json!({"decisions": output, "provider_requests": 0}))
            }).await
        }
        AttentionCommand::Fit {
            record_ids,
            options,
        } => {
            records(&runtime, record_ids.clone()).await?;
            let compatibility = initial.raw.attention.compatibility();
            control_write(&runtime, initial.generation(), move |store| {
                encoded(
                    store
                        .fit(
                            &recipient,
                            &recipient,
                            &record_ids,
                            compatibility,
                            options,
                            now_ms(),
                        )
                        .map_err(|error| error.to_string())?,
                )
            })
            .await
        }
        AttentionCommand::OpenEvaluation { mut plan } => {
            records(&runtime, plan.record_ids.clone()).await?;
            // The authenticated operation supplies the opening time, never a backdated claim.
            plan.opened_at_ms = now_ms();
            control_write(&runtime, initial.generation(), move |store| {
                encoded(
                    store
                        .open_evaluation(&recipient, &recipient, plan)
                        .map_err(|error| error.to_string())?,
                )
            })
            .await
        }
        AttentionCommand::Evaluate {
            evaluation_id,
            digest,
        } => {
            validate_artifact(&runtime, digest.clone(), Some(evaluation_id.clone())).await?;
            control_write(&runtime, initial.generation(), move |store| {
                encoded(
                    store
                        .evaluate(&recipient, &recipient, &evaluation_id, &digest, now_ms())
                        .map_err(|error| error.to_string())?,
                )
            })
            .await
        }
        AttentionCommand::Promote {
            evaluation_id,
            digest,
        } => {
            validate_artifact(&runtime, digest.clone(), Some(evaluation_id.clone())).await?;
            let compatibility = initial.raw.attention.compatibility();
            control_write(&runtime, initial.generation(), move |store| {
                encoded(
                    store
                        .promote(
                            &recipient,
                            &recipient,
                            &digest,
                            &evaluation_id,
                            &compatibility,
                            now_ms(),
                        )
                        .map_err(|error| error.to_string())?,
                )
            })
            .await
        }
        AttentionCommand::Rollback { digest } => {
            validate_artifact(&runtime, digest.clone(), None).await?;
            let compatibility = initial.raw.attention.compatibility();
            control_write(&runtime, initial.generation(), move |store| {
                encoded(
                    store
                        .rollback(&recipient, &recipient, &digest, &compatibility, now_ms())
                        .map_err(|error| error.to_string())?,
                )
            })
            .await
        }
    }
}

/// Describes the same closed command surface advertised by the MCP attention tool.
pub fn schema() -> Value {
    json!({"type": "object", "required": ["operation"], "additionalProperties": false,
    "properties": {
        "operation": {"type": "string", "enum": ["status", "configure", "retrieve", "review", "feedback", "partitions", "replay", "fit", "open_evaluation", "evaluate", "promote", "rollback"]},
        "settings": {"type": "object", "description": "Complete attention settings from status. Replacement preserves no omitted values; recipient cannot change."},
        "record_id": {"type": "string"}, "record_ids": {"type": "array", "items": {"type": "string"}},
        "requested": {"type": "integer", "minimum": 1, "maximum": 128}, "seed": {"type": "integer", "minimum": 0},
        "label": {"type": "string", "enum": ["wanted_promptly", "wanted_later", "not_needed", "unsure"]},
        "spec": {"type": "object", "required": ["training_end_ms", "validation_end_ms"], "properties": {
            "training_end_ms": {"type": "integer"}, "validation_end_ms": {"type": "integer"}}},
        "policy": {"type": "object", "required": ["wanted", "prompt", "participation", "change"], "properties": {
            "wanted": {"type": "number"}, "prompt": {"type": "number"}, "participation": {"type": "number"}, "change": {"type": "number"}}},
        "options": {"type": "object", "description": "Explicit FitOptions: minimum_labels, minimum_wanted, minimum_not_needed, minimum_promptly, minimum_later, regularization, iterations, learning_rate, seed, environment, wanted_threshold, timely_threshold, candidate_ttl_ms."},
        "plan": {"type": "object", "description": "EvaluationPlan: id, record_ids, fixed_baseline, predeclared limits, opened_at_ms (server replaces time)."},
        "evaluation_id": {"type": "string"}, "digest": {"type": "string"}
    }})
}
