use std::{
    fs::{File, OpenOptions},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use camino::{Utf8Path, Utf8PathBuf};
use fs2::FileExt as _;
use serde::{Deserialize, Serialize};
use serenity::model::id::{InteractionId, MessageId};
use thiserror::Error;
use tokio::io::AsyncWriteExt as _;

use crate::dice::{
    DiceExpression, ENTROPY_SEED_BYTES, EntropyProvenance, RollResult, SeedSource, render_modifier,
    roll_from_seed,
};

/// Bound disk use without silently erasing the only deduplication fact.
const MAX_RECEIPTS: usize = 4_096;
const MAX_RETIRED_INTERACTIONS: usize = 65_536;
const RETIRED_INTERACTION_TTL: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicationState {
    Prepared,
    Bound { message_id: MessageId },
    Published { message_id: MessageId },
}

impl PublicationState {
    pub fn message_id(self) -> Option<MessageId> {
        match self {
            Self::Prepared => None,
            Self::Bound { message_id } | Self::Published { message_id } => Some(message_id),
        }
    }

    pub fn is_published(self) -> bool {
        matches!(self, Self::Published { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollReceipt {
    interaction_id: InteractionId,
    expression: DiceExpression,
    dice: Vec<u32>,
    total: i64,
    entropy_source: EntropyProvenance,
    seed: [u8; ENTROPY_SEED_BYTES],
    publication: PublicationState,
}

impl RollReceipt {
    pub fn publication(&self) -> PublicationState {
        self.publication
    }

    pub fn render(&self) -> Result<String, RollReceiptError> {
        let message_id = self
            .publication
            .message_id()
            .ok_or(RollReceiptError::NotBound(self.interaction_id))?;
        Ok(format!(
            "🎲 `{}` → {:?}{} = **{}**\n-# entropy: `{}` · receipt: `discord:{}`",
            self.expression,
            self.dice,
            render_modifier(self.expression.modifier().get()),
            self.total,
            self.entropy_source,
            message_id,
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum StoredPublicationState {
    Prepared,
    Bound { message_id: u64 },
    Published { message_id: u64 },
}

impl From<PublicationState> for StoredPublicationState {
    fn from(value: PublicationState) -> Self {
        match value {
            PublicationState::Prepared => Self::Prepared,
            PublicationState::Bound { message_id } => Self::Bound {
                message_id: message_id.get(),
            },
            PublicationState::Published { message_id } => Self::Published {
                message_id: message_id.get(),
            },
        }
    }
}

impl TryFrom<StoredPublicationState> for PublicationState {
    type Error = String;

    fn try_from(value: StoredPublicationState) -> Result<Self, Self::Error> {
        match value {
            StoredPublicationState::Prepared => Ok(Self::Prepared),
            StoredPublicationState::Bound { message_id } if message_id != 0 => Ok(Self::Bound {
                message_id: MessageId::new(message_id),
            }),
            StoredPublicationState::Published { message_id } if message_id != 0 => {
                Ok(Self::Published {
                    message_id: MessageId::new(message_id),
                })
            }
            _ => Err("Discord message ID must be nonzero".into()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredRollReceipt {
    interaction_id: u64,
    expression: String,
    entropy_source: EntropyProvenance,
    seed: [u8; ENTROPY_SEED_BYTES],
    dice: Option<Vec<u32>>,
    total: Option<i64>,
    publication: StoredPublicationState,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct RetiredInteraction {
    interaction_id: u64,
    retired_at_unix: u64,
}

enum LoadedReceipt {
    Claimed {
        expression: DiceExpression,
        entropy_source: EntropyProvenance,
        seed: [u8; ENTROPY_SEED_BYTES],
    },
    Rolled(RollReceipt),
}

impl StoredRollReceipt {
    fn validate(
        self,
        expected_interaction_id: InteractionId,
        path: &Utf8Path,
    ) -> Result<LoadedReceipt, RollReceiptError> {
        let invalid = |reason: String| RollReceiptError::Invalid {
            path: path.to_owned(),
            reason,
        };
        if self.interaction_id != expected_interaction_id.get() {
            return Err(invalid(format!(
                "interaction ID {} does not match filename ID {}",
                self.interaction_id, expected_interaction_id
            )));
        }
        let expression = DiceExpression::parse(&self.expression)
            .map_err(|error| invalid(format!("invalid expression: {error}")))?;
        if expression.to_string() != self.expression {
            return Err(invalid("expression is not canonical".into()));
        }
        let publication = PublicationState::try_from(self.publication).map_err(invalid)?;
        match (self.dice, self.total) {
            (None, None) if publication == PublicationState::Prepared => {
                Ok(LoadedReceipt::Claimed {
                    expression,
                    entropy_source: self.entropy_source,
                    seed: self.seed,
                })
            }
            (Some(dice), Some(total)) => {
                if dice.len() != usize::from(expression.count().get()) {
                    return Err(invalid("face count does not match expression".into()));
                }
                if dice
                    .iter()
                    .any(|face| *face == 0 || *face > expression.sides().get())
                {
                    return Err(invalid("face falls outside the die range".into()));
                }
                let expected_total = dice.iter().map(|&face| i64::from(face)).sum::<i64>()
                    + expression.modifier().get();
                if total != expected_total {
                    return Err(invalid("total does not match faces and modifier".into()));
                }
                let recomputed = roll_from_seed(expression.clone(), self.seed, self.entropy_source);
                if recomputed.dice != dice || recomputed.total != total {
                    return Err(invalid(
                        "stored result does not match committed seed".into(),
                    ));
                }
                Ok(LoadedReceipt::Rolled(RollReceipt {
                    interaction_id: expected_interaction_id,
                    expression,
                    dice,
                    total,
                    entropy_source: self.entropy_source,
                    seed: self.seed,
                    publication,
                }))
            }
            _ => Err(invalid("claim/result fields are inconsistent".into())),
        }
    }
}

pub struct RollReceiptStore {
    directory: Utf8PathBuf,
    lock: tokio::sync::Mutex<()>,
    capacity: usize,
    retired_capacity: usize,
}

impl RollReceiptStore {
    pub fn new(state_dir: &Utf8Path) -> Self {
        Self {
            directory: state_dir.join("roll-receipts"),
            lock: tokio::sync::Mutex::new(()),
            capacity: MAX_RECEIPTS,
            retired_capacity: MAX_RETIRED_INTERACTIONS,
        }
    }

    #[cfg(test)]
    fn with_capacity(state_dir: &Utf8Path, capacity: usize) -> Self {
        Self {
            directory: state_dir.join("roll-receipts"),
            lock: tokio::sync::Mutex::new(()),
            capacity,
            retired_capacity: MAX_RETIRED_INTERACTIONS,
        }
    }

    #[cfg(test)]
    fn with_capacities(state_dir: &Utf8Path, capacity: usize, retired_capacity: usize) -> Self {
        Self {
            directory: state_dir.join("roll-receipts"),
            lock: tokio::sync::Mutex::new(()),
            capacity,
            retired_capacity,
        }
    }

    pub async fn get_or_roll(
        &self,
        interaction_id: InteractionId,
        expression: DiceExpression,
        seed_source: &mut impl SeedSource,
    ) -> Result<RollReceipt, RollReceiptError> {
        let _guard = self.lock.lock().await;
        let _process_guard = self.acquire_process_lock().await?;
        if let Some(loaded) = self.load(interaction_id).await? {
            return self.finish_loaded(interaction_id, expression, loaded).await;
        }
        if self.is_retired(interaction_id).await? {
            return Err(RollReceiptError::Retired(interaction_id));
        }
        if self.receipt_count().await? >= self.capacity {
            self.reclaim_oldest_published().await?;
        }
        if self.is_retired(interaction_id).await? {
            return Err(RollReceiptError::Retired(interaction_id));
        }
        if self.receipt_count().await? >= self.capacity {
            return Err(RollReceiptError::Capacity(self.capacity));
        }

        // The seed is not itself a roll. Persisting and syncing it first makes
        // every later derivation deterministic across cancellation and crash.
        let generated = seed_source.generate_seed()?;
        let stored = StoredRollReceipt {
            interaction_id: interaction_id.get(),
            expression: expression.to_string(),
            entropy_source: generated.provenance,
            seed: generated.bytes,
            dice: None,
            total: None,
            publication: StoredPublicationState::Prepared,
        };
        self.persist(&stored).await?;
        self.finish_claim(
            interaction_id,
            expression,
            stored.entropy_source,
            stored.seed,
        )
        .await
    }

    async fn finish_loaded(
        &self,
        interaction_id: InteractionId,
        requested: DiceExpression,
        loaded: LoadedReceipt,
    ) -> Result<RollReceipt, RollReceiptError> {
        match loaded {
            LoadedReceipt::Claimed {
                expression,
                entropy_source,
                seed,
            } => {
                Self::check_expression(interaction_id, &expression, &requested)?;
                self.finish_claim(interaction_id, expression, entropy_source, seed)
                    .await
            }
            LoadedReceipt::Rolled(receipt) => {
                Self::check_expression(interaction_id, &receipt.expression, &requested)?;
                Ok(receipt)
            }
        }
    }

    async fn finish_claim(
        &self,
        interaction_id: InteractionId,
        expression: DiceExpression,
        entropy_source: EntropyProvenance,
        seed: [u8; ENTROPY_SEED_BYTES],
    ) -> Result<RollReceipt, RollReceiptError> {
        let RollResult {
            expression,
            dice,
            total,
            entropy_source,
        } = roll_from_seed(expression, seed, entropy_source);
        let receipt = RollReceipt {
            interaction_id,
            expression,
            dice,
            total,
            entropy_source,
            seed,
            publication: PublicationState::Prepared,
        };
        self.persist(&self.to_stored(&receipt)).await?;
        Ok(receipt)
    }

    fn check_expression(
        interaction_id: InteractionId,
        recorded: &DiceExpression,
        requested: &DiceExpression,
    ) -> Result<(), RollReceiptError> {
        if recorded != requested {
            return Err(RollReceiptError::InteractionConflict {
                interaction_id,
                recorded: recorded.to_string(),
                requested: requested.to_string(),
            });
        }
        Ok(())
    }

    pub async fn bind_response(
        &self,
        interaction_id: InteractionId,
        message_id: MessageId,
    ) -> Result<RollReceipt, RollReceiptError> {
        self.update_publication(interaction_id, message_id, PublicationTransition::Bind)
            .await
    }

    pub async fn mark_published(
        &self,
        interaction_id: InteractionId,
        message_id: MessageId,
    ) -> Result<RollReceipt, RollReceiptError> {
        self.update_publication(interaction_id, message_id, PublicationTransition::Publish)
            .await
    }

    async fn update_publication(
        &self,
        interaction_id: InteractionId,
        message_id: MessageId,
        transition: PublicationTransition,
    ) -> Result<RollReceipt, RollReceiptError> {
        let _guard = self.lock.lock().await;
        let _process_guard = self.acquire_process_lock().await?;
        let LoadedReceipt::Rolled(mut receipt) = self
            .load(interaction_id)
            .await?
            .ok_or(RollReceiptError::Missing(interaction_id))?
        else {
            return Err(RollReceiptError::NotRolled(interaction_id));
        };
        if let Some(recorded) = receipt.publication.message_id()
            && recorded != message_id
        {
            return Err(RollReceiptError::PublicationConflict {
                interaction_id,
                recorded,
                requested: message_id,
            });
        }
        receipt.publication = match (transition, receipt.publication) {
            (PublicationTransition::Publish, _) => PublicationState::Published { message_id },
            (PublicationTransition::Bind, state) if state.is_published() => state,
            (PublicationTransition::Bind, _) => PublicationState::Bound { message_id },
        };
        self.persist(&self.to_stored(&receipt)).await?;
        Ok(receipt)
    }

    async fn acquire_process_lock(&self) -> Result<ProcessLock, RollReceiptError> {
        tokio::fs::create_dir_all(&self.directory)
            .await
            .map_err(|error| RollReceiptError::Io {
                path: self.directory.clone(),
                error,
            })?;
        let path = self.directory.join(".store.lock");
        tokio::task::spawn_blocking(move || {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
                .map_err(|error| RollReceiptError::Io {
                    path: path.clone(),
                    error,
                })?;
            file.lock_exclusive()
                .map_err(|error| RollReceiptError::Io {
                    path: path.clone(),
                    error,
                })?;
            Ok(ProcessLock(file))
        })
        .await
        .map_err(|error| RollReceiptError::LockTask(error.to_string()))?
    }

    async fn load(
        &self,
        interaction_id: InteractionId,
    ) -> Result<Option<LoadedReceipt>, RollReceiptError> {
        let path = self.receipt_path(interaction_id);
        let bytes = match tokio::fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(RollReceiptError::Io { path, error }),
        };
        let stored: StoredRollReceipt =
            serde_json::from_slice(&bytes).map_err(|error| RollReceiptError::Decode {
                path: path.clone(),
                error,
            })?;
        stored.validate(interaction_id, &path).map(Some)
    }

    fn to_stored(&self, receipt: &RollReceipt) -> StoredRollReceipt {
        StoredRollReceipt {
            interaction_id: receipt.interaction_id.get(),
            expression: receipt.expression.to_string(),
            entropy_source: receipt.entropy_source,
            seed: receipt.seed,
            dice: Some(receipt.dice.clone()),
            total: Some(receipt.total),
            publication: receipt.publication.into(),
        }
    }

    async fn persist(&self, receipt: &StoredRollReceipt) -> Result<(), RollReceiptError> {
        let interaction_id = InteractionId::new(receipt.interaction_id);
        let path = self.receipt_path(interaction_id);
        let temporary =
            self.directory
                .join(format!(".{}.{}.tmp", interaction_id, std::process::id()));
        let bytes =
            serde_json::to_vec_pretty(receipt).map_err(|error| RollReceiptError::Encode {
                path: path.clone(),
                error,
            })?;
        let mut file =
            tokio::fs::File::create(&temporary)
                .await
                .map_err(|error| RollReceiptError::Io {
                    path: temporary.clone(),
                    error,
                })?;
        file.write_all(&bytes)
            .await
            .map_err(|error| RollReceiptError::Io {
                path: temporary.clone(),
                error,
            })?;
        file.sync_all()
            .await
            .map_err(|error| RollReceiptError::Io {
                path: temporary.clone(),
                error,
            })?;
        tokio::fs::rename(&temporary, &path)
            .await
            .map_err(|error| RollReceiptError::Io {
                path: path.clone(),
                error,
            })?;
        let directory = self.directory.clone();
        tokio::task::spawn_blocking(move || {
            File::open(&directory)
                .and_then(|file| file.sync_all())
                .map_err(|error| RollReceiptError::Io {
                    path: directory,
                    error,
                })
        })
        .await
        .map_err(|error| RollReceiptError::LockTask(error.to_string()))?
    }

    async fn receipt_count(&self) -> Result<usize, RollReceiptError> {
        let mut count = 0;
        let mut directory = tokio::fs::read_dir(&self.directory)
            .await
            .map_err(|error| RollReceiptError::Io {
                path: self.directory.clone(),
                error,
            })?;
        while let Some(entry) =
            directory
                .next_entry()
                .await
                .map_err(|error| RollReceiptError::Io {
                    path: self.directory.clone(),
                    error,
                })?
        {
            if entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "json")
                && entry.path().file_stem().is_some_and(|stem| {
                    stem.to_string_lossy()
                        .bytes()
                        .all(|byte| byte.is_ascii_digit())
                })
            {
                count += 1;
            }
        }
        Ok(count)
    }

    async fn reclaim_oldest_published(&self) -> Result<(), RollReceiptError> {
        let mut published = Vec::new();
        let mut directory = tokio::fs::read_dir(&self.directory)
            .await
            .map_err(|error| RollReceiptError::Io {
                path: self.directory.clone(),
                error,
            })?;
        while let Some(entry) =
            directory
                .next_entry()
                .await
                .map_err(|error| RollReceiptError::Io {
                    path: self.directory.clone(),
                    error,
                })?
        {
            let path = Utf8PathBuf::from_path_buf(entry.path())
                .map_err(|path| RollReceiptError::NonUtf8Path(path.display().to_string()))?;
            if path.extension() != Some("json") {
                continue;
            }
            let Some(stem) = path.file_stem() else {
                continue;
            };
            if !stem.bytes().all(|byte| byte.is_ascii_digit()) {
                continue;
            }
            let raw_id = stem.parse::<u64>().map_err(|_| RollReceiptError::Invalid {
                path: path.clone(),
                reason: "receipt filename is not a Discord interaction ID".into(),
            })?;
            let interaction_id = InteractionId::new(raw_id);
            let bytes = tokio::fs::read(&path)
                .await
                .map_err(|error| RollReceiptError::Io {
                    path: path.clone(),
                    error,
                })?;
            let stored: StoredRollReceipt =
                serde_json::from_slice(&bytes).map_err(|error| RollReceiptError::Decode {
                    path: path.clone(),
                    error,
                })?;
            let loaded = stored.validate(interaction_id, &path)?;
            if matches!(
                loaded,
                LoadedReceipt::Rolled(RollReceipt {
                    publication: PublicationState::Published { .. },
                    ..
                })
            ) {
                published.push(interaction_id);
            }
        }
        let Some(oldest) = published.into_iter().min_by_key(|id| id.get()) else {
            return Ok(());
        };

        // Persist the exact tombstone before deleting the full receipt. A
        // crash may temporarily retain both, but can never erase both facts.
        let now = unix_now()?;
        let mut retired = self.load_retired().await?;
        retired.retain(|entry| {
            now.saturating_sub(entry.retired_at_unix) <= RETIRED_INTERACTION_TTL.as_secs()
        });
        let already_retired = retired
            .iter()
            .any(|entry| entry.interaction_id == oldest.get());
        if !already_retired && retired.len() >= self.retired_capacity {
            return Err(RollReceiptError::RetiredCapacity(self.retired_capacity));
        }
        if !already_retired {
            retired.push(RetiredInteraction {
                interaction_id: oldest.get(),
                retired_at_unix: now,
            });
        }
        self.persist_retired(&retired).await?;
        let path = self.receipt_path(oldest);
        tokio::fs::remove_file(&path)
            .await
            .map_err(|error| RollReceiptError::Io { path, error })?;
        self.sync_directory().await
    }

    async fn is_retired(&self, interaction_id: InteractionId) -> Result<bool, RollReceiptError> {
        let now = unix_now()?;
        Ok(self.load_retired().await?.iter().any(|entry| {
            entry.interaction_id == interaction_id.get()
                && now.saturating_sub(entry.retired_at_unix) <= RETIRED_INTERACTION_TTL.as_secs()
        }))
    }

    async fn load_retired(&self) -> Result<Vec<RetiredInteraction>, RollReceiptError> {
        let path = self.directory.join(".retired.json");
        let bytes = match tokio::fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(RollReceiptError::Io { path, error }),
        };
        let retired: Vec<RetiredInteraction> =
            serde_json::from_slice(&bytes).map_err(|error| RollReceiptError::Decode {
                path: path.clone(),
                error,
            })?;
        if retired.len() > MAX_RETIRED_INTERACTIONS
            || retired.iter().any(|entry| entry.interaction_id == 0)
        {
            return Err(RollReceiptError::Invalid {
                path,
                reason: "retired interaction set exceeds bounds or contains a zero ID".into(),
            });
        }
        Ok(retired)
    }

    async fn persist_retired(
        &self,
        retired: &[RetiredInteraction],
    ) -> Result<(), RollReceiptError> {
        let path = self.directory.join(".retired.json");
        let temporary = self
            .directory
            .join(format!(".retired.{}.tmp", std::process::id()));
        let bytes = serde_json::to_vec(retired).map_err(|error| RollReceiptError::Encode {
            path: path.clone(),
            error,
        })?;
        let mut file =
            tokio::fs::File::create(&temporary)
                .await
                .map_err(|error| RollReceiptError::Io {
                    path: temporary.clone(),
                    error,
                })?;
        file.write_all(&bytes)
            .await
            .map_err(|error| RollReceiptError::Io {
                path: temporary.clone(),
                error,
            })?;
        file.sync_all()
            .await
            .map_err(|error| RollReceiptError::Io {
                path: temporary.clone(),
                error,
            })?;
        tokio::fs::rename(&temporary, &path)
            .await
            .map_err(|error| RollReceiptError::Io {
                path: path.clone(),
                error,
            })?;
        self.sync_directory().await
    }

    async fn sync_directory(&self) -> Result<(), RollReceiptError> {
        let directory = self.directory.clone();
        tokio::task::spawn_blocking(move || {
            File::open(&directory)
                .and_then(|file| file.sync_all())
                .map_err(|error| RollReceiptError::Io {
                    path: directory,
                    error,
                })
        })
        .await
        .map_err(|error| RollReceiptError::LockTask(error.to_string()))?
    }

    fn receipt_path(&self, interaction_id: InteractionId) -> Utf8PathBuf {
        self.directory.join(format!("{interaction_id}.json"))
    }
}

#[derive(Debug, Clone, Copy)]
enum PublicationTransition {
    Bind,
    Publish,
}

fn unix_now() -> Result<u64, RollReceiptError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|error| RollReceiptError::Clock(error.to_string()))
}

struct ProcessLock(File);

impl Drop for ProcessLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

#[derive(Debug, Error)]
pub enum RollReceiptError {
    #[error(transparent)]
    Roll(#[from] crate::dice::RollError),
    #[error("failed to access roll receipt `{path}`")]
    Io {
        path: Utf8PathBuf,
        #[source]
        error: std::io::Error,
    },
    #[error("failed to decode roll receipt `{path}`")]
    Decode {
        path: Utf8PathBuf,
        #[source]
        error: serde_json::Error,
    },
    #[error("roll receipt `{path}` failed validation: {reason}")]
    Invalid { path: Utf8PathBuf, reason: String },
    #[error("failed to encode roll receipt `{path}`")]
    Encode {
        path: Utf8PathBuf,
        #[source]
        error: serde_json::Error,
    },
    #[error("receipt-lock worker failed: {0}")]
    LockTask(String),
    #[error("system clock is before the Unix epoch: {0}")]
    Clock(String),
    #[error("receipt path is not valid UTF-8: {0}")]
    NonUtf8Path(String),
    #[error("roll receipt capacity ({0}) reached; refusing to erase deduplication state")]
    Capacity(usize),
    #[error("retired interaction capacity ({0}) reached; refusing unsafe receipt cleanup")]
    RetiredCapacity(usize),
    #[error("interaction {0} is older than the retained deduplication horizon")]
    Retired(InteractionId),
    #[error("interaction {interaction_id} was already bound to `{recorded}`, not `{requested}`")]
    InteractionConflict {
        interaction_id: InteractionId,
        recorded: String,
        requested: String,
    },
    #[error("roll receipt {0} does not exist")]
    Missing(InteractionId),
    #[error("roll receipt {0} has not finished deterministic derivation")]
    NotRolled(InteractionId),
    #[error("roll receipt {0} is not bound to a Discord response")]
    NotBound(InteractionId),
    #[error(
        "interaction {interaction_id} was already published as message {recorded}, not {requested}"
    )]
    PublicationConflict {
        interaction_id: InteractionId,
        recorded: MessageId,
        requested: MessageId,
    },
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;
    use crate::dice::{GeneratedSeed, RollError};

    struct CountingSeedSource {
        seed: [u8; ENTROPY_SEED_BYTES],
        calls: Arc<AtomicUsize>,
    }

    impl SeedSource for CountingSeedSource {
        fn generate_seed(&mut self) -> Result<GeneratedSeed, RollError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(GeneratedSeed {
                bytes: self.seed,
                provenance: EntropyProvenance::TestSequence,
            })
        }
    }

    fn source(seed: u8, calls: &Arc<AtomicUsize>) -> CountingSeedSource {
        CountingSeedSource {
            seed: [seed; ENTROPY_SEED_BYTES],
            calls: Arc::clone(calls),
        }
    }

    #[tokio::test]
    async fn replay_after_restart_returns_the_persisted_roll_without_resampling() {
        let directory = tempfile::tempdir().unwrap();
        let state_dir = Utf8Path::from_path(directory.path()).unwrap();
        let expression = DiceExpression::parse("2d6+1").unwrap();
        let store = RollReceiptStore::new(state_dir);
        let calls = Arc::new(AtomicUsize::new(0));
        let first = store
            .get_or_roll(
                InteractionId::new(42),
                expression.clone(),
                &mut source(1, &calls),
            )
            .await
            .unwrap();
        let restarted_store = RollReceiptStore::new(state_dir);
        let replay = restarted_store
            .get_or_roll(InteractionId::new(42), expression, &mut source(2, &calls))
            .await
            .unwrap();
        assert_eq!(replay, first);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn durable_claim_recovers_without_requesting_another_seed() {
        let directory = tempfile::tempdir().unwrap();
        let state_dir = Utf8Path::from_path(directory.path()).unwrap();
        let store = RollReceiptStore::new(state_dir);
        tokio::fs::create_dir_all(&store.directory).await.unwrap();
        let claim = StoredRollReceipt {
            interaction_id: 55,
            expression: "1d6".into(),
            entropy_source: EntropyProvenance::TestSequence,
            seed: [9; ENTROPY_SEED_BYTES],
            dice: None,
            total: None,
            publication: StoredPublicationState::Prepared,
        };
        store.persist(&claim).await.unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        store
            .get_or_roll(
                InteractionId::new(55),
                DiceExpression::parse("d6").unwrap(),
                &mut source(3, &calls),
            )
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn independent_stores_racing_one_interaction_generate_one_seed() {
        let directory = tempfile::tempdir().unwrap();
        let state_dir = Utf8Path::from_path(directory.path()).unwrap();
        let first_store = RollReceiptStore::new(state_dir);
        let second_store = RollReceiptStore::new(state_dir);
        let calls = Arc::new(AtomicUsize::new(0));
        let expression = DiceExpression::parse("d6").unwrap();
        let mut first_source = source(1, &calls);
        let mut second_source = source(2, &calls);
        let (first, second) = tokio::join!(
            first_store.get_or_roll(
                InteractionId::new(77),
                expression.clone(),
                &mut first_source,
            ),
            second_store.get_or_roll(InteractionId::new(77), expression, &mut second_source,),
        );
        assert_eq!(first.unwrap(), second.unwrap());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn publication_state_is_monotonic_and_rendering_is_exact() {
        let directory = tempfile::tempdir().unwrap();
        let state_dir = Utf8Path::from_path(directory.path()).unwrap();
        let store = RollReceiptStore::new(state_dir);
        let calls = Arc::new(AtomicUsize::new(0));
        store
            .get_or_roll(
                InteractionId::new(7),
                DiceExpression::parse("d6+2").unwrap(),
                &mut source(3, &calls),
            )
            .await
            .unwrap();
        let bound = store
            .bind_response(InteractionId::new(7), MessageId::new(99))
            .await
            .unwrap();
        let expected = format!(
            "🎲 `1d6+2` → {:?} + 2 = **{}**\n-# entropy: `test-sequence` · receipt: `discord:99`",
            bound.dice, bound.total
        );
        assert_eq!(bound.render().unwrap(), expected);
        let published = store
            .mark_published(InteractionId::new(7), MessageId::new(99))
            .await
            .unwrap();
        assert_eq!(
            published.publication(),
            PublicationState::Published {
                message_id: MessageId::new(99)
            }
        );
        assert!(
            store
                .bind_response(InteractionId::new(7), MessageId::new(100))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn rejects_semantically_invalid_persisted_receipt() {
        let directory = tempfile::tempdir().unwrap();
        let state_dir = Utf8Path::from_path(directory.path()).unwrap();
        let store = RollReceiptStore::new(state_dir);
        tokio::fs::create_dir_all(&store.directory).await.unwrap();
        let invalid = StoredRollReceipt {
            interaction_id: 8,
            expression: "1d6".into(),
            entropy_source: EntropyProvenance::TestSequence,
            seed: [1; ENTROPY_SEED_BYTES],
            dice: Some(vec![7]),
            total: Some(7),
            publication: StoredPublicationState::Prepared,
        };
        tokio::fs::write(
            store.receipt_path(InteractionId::new(8)),
            serde_json::to_vec(&invalid).unwrap(),
        )
        .await
        .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let error = store
            .get_or_roll(
                InteractionId::new(8),
                DiceExpression::parse("d6").unwrap(),
                &mut source(0, &calls),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, RollReceiptError::Invalid { .. }));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn capacity_retires_published_receipts_without_resampling_them() {
        let directory = tempfile::tempdir().unwrap();
        let state_dir = Utf8Path::from_path(directory.path()).unwrap();
        let store = RollReceiptStore::with_capacity(state_dir, 2);
        let calls = Arc::new(AtomicUsize::new(0));
        let expression = DiceExpression::parse("d6").unwrap();

        for id in [10, 20] {
            store
                .get_or_roll(
                    InteractionId::new(id),
                    expression.clone(),
                    &mut source(id as u8, &calls),
                )
                .await
                .unwrap();
            store
                .bind_response(InteractionId::new(id), MessageId::new(id + 100))
                .await
                .unwrap();
            store
                .mark_published(InteractionId::new(id), MessageId::new(id + 100))
                .await
                .unwrap();
        }
        store
            .get_or_roll(
                InteractionId::new(30),
                expression.clone(),
                &mut source(30, &calls),
            )
            .await
            .unwrap();
        assert!(!store.receipt_path(InteractionId::new(10)).exists());
        assert!(store.is_retired(InteractionId::new(10)).await.unwrap());
        assert!(!store.is_retired(InteractionId::new(9)).await.unwrap());
        let before_retry = calls.load(Ordering::SeqCst);
        let error = store
            .get_or_roll(InteractionId::new(10), expression, &mut source(99, &calls))
            .await
            .unwrap_err();
        assert!(matches!(error, RollReceiptError::Retired(id) if id.get() == 10));
        assert_eq!(calls.load(Ordering::SeqCst), before_retry);

        // An older snowflake that was never handled is not conflated with the
        // exact retired interaction, even if gateway delivery was reordered.
        store
            .get_or_roll(
                InteractionId::new(9),
                DiceExpression::parse("d6").unwrap(),
                &mut source(9, &calls),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn capacity_never_evicts_nonterminal_receipts() {
        let directory = tempfile::tempdir().unwrap();
        let state_dir = Utf8Path::from_path(directory.path()).unwrap();
        let store = RollReceiptStore::with_capacity(state_dir, 1);
        let calls = Arc::new(AtomicUsize::new(0));
        store
            .get_or_roll(
                InteractionId::new(10),
                DiceExpression::parse("d6").unwrap(),
                &mut source(1, &calls),
            )
            .await
            .unwrap();
        let error = store
            .get_or_roll(
                InteractionId::new(20),
                DiceExpression::parse("d6").unwrap(),
                &mut source(2, &calls),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, RollReceiptError::Capacity(1)));
        assert!(store.receipt_path(InteractionId::new(10)).exists());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn maximum_roll_rendering_fits_one_discord_message() {
        let directory = tempfile::tempdir().unwrap();
        let state_dir = Utf8Path::from_path(directory.path()).unwrap();
        let store = RollReceiptStore::new(state_dir);
        let calls = Arc::new(AtomicUsize::new(0));
        store
            .get_or_roll(
                InteractionId::new(500),
                DiceExpression::parse("100d1000000+1000000").unwrap(),
                &mut source(8, &calls),
            )
            .await
            .unwrap();
        let receipt = store
            .bind_response(InteractionId::new(500), MessageId::new(u64::MAX))
            .await
            .unwrap();
        assert!(receipt.render().unwrap().len() <= 2_000);
    }

    #[tokio::test]
    async fn expired_exact_tombstone_does_not_block_an_unreplayable_interaction_forever() {
        let directory = tempfile::tempdir().unwrap();
        let state_dir = Utf8Path::from_path(directory.path()).unwrap();
        let store = RollReceiptStore::new(state_dir);
        tokio::fs::create_dir_all(&store.directory).await.unwrap();
        store
            .persist_retired(&[RetiredInteraction {
                interaction_id: 600,
                retired_at_unix: unix_now().unwrap() - RETIRED_INTERACTION_TTL.as_secs() - 1,
            }])
            .await
            .unwrap();
        assert!(!store.is_retired(InteractionId::new(600)).await.unwrap());
    }

    #[tokio::test]
    async fn full_tombstone_set_can_finish_a_crash_interrupted_reclamation() {
        let directory = tempfile::tempdir().unwrap();
        let state_dir = Utf8Path::from_path(directory.path()).unwrap();
        let store = RollReceiptStore::with_capacities(state_dir, 1, 2);
        let calls = Arc::new(AtomicUsize::new(0));
        store
            .get_or_roll(
                InteractionId::new(700),
                DiceExpression::parse("d6").unwrap(),
                &mut source(7, &calls),
            )
            .await
            .unwrap();
        store
            .bind_response(InteractionId::new(700), MessageId::new(701))
            .await
            .unwrap();
        store
            .mark_published(InteractionId::new(700), MessageId::new(701))
            .await
            .unwrap();
        let now = unix_now().unwrap();
        store
            .persist_retired(&[
                RetiredInteraction {
                    interaction_id: 699,
                    retired_at_unix: now,
                },
                RetiredInteraction {
                    interaction_id: 700,
                    retired_at_unix: now,
                },
            ])
            .await
            .unwrap();

        store.reclaim_oldest_published().await.unwrap();
        assert!(!store.receipt_path(InteractionId::new(700)).exists());
        assert!(store.is_retired(InteractionId::new(700)).await.unwrap());
    }
}
