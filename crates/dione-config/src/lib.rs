//! Validated configuration, durable editing, and one process-wide publication authority.

//!
//! Construction and publication helpers remain private.
//!
//! ```compile_fail
//! use dione_config::ConfigRuntime;
//! let runtime = ConfigRuntime { state_dir: "state".into() };
//! ```
//!
//! ```compile_fail
//! use dione_config::{Config, LoadedConfig};
//! let mut loaded = LoadedConfig::try_from_raw(Config::default()).unwrap();
//! loaded.generation = 1;
//! ```
//!
//! ```compile_fail
//! use dione_config::{Config, LoadedConfig};
//! let loaded = LoadedConfig::from_raw(Config::default());
//! ```
//!
//! ```compile_fail
//! use dione_config::{Config, LoadedConfig};
//! use std::sync::atomic::AtomicU64;
//! let loaded = LoadedConfig::try_from_raw_with_counter(Config::default(), &AtomicU64::new(1));
//! ```
//!
//! ```compile_fail
//! use dione_config::ChannelPolicy;
//! fn weaken(policy: &mut ChannelPolicy) { policy.raw_had_identity_entries = false; }
//! ```
//!
//! ```compile_fail
//! use dione_config::ConfigStore;
//! let store = ConfigStore { doc: Default::default() };
//! ```
//!
//! ```compile_fail
//! use dione_config::ConfigStore;
//! let store = ConfigStore::load_blocking("state".into());
//! ```
//!
//! ```compile_fail
//! let cache = &dione_config::config::LAST_VALID_CONFIG;
//! ```

#![warn(missing_docs)]

mod config;
mod config_candidate;
mod config_store;

pub use config::{
    AccessConfig, AccessRequestsConfig, ArchiveConfig, BellChannelOverride, BellMode,
    BellRingsConfig, BellTrigger, ChannelConfig, ChannelPolicy, ChunkMode, Config,
    ConfigCompositionError, ConfigDurability, ConfigError, ConfigGenerationError,
    ConfigMutationOutcome, ConfigRuntime, ContradictionaryConfig, DeliveryConfig, DmPolicy,
    EvidenceMarkerVersion, LoadedConfig, MAX_PREAMBLE_BYTES, MentionConfig, NameplateConfig,
    NoLkgPolicy, PhantomCanaryConfig, PreSendConfig, PreambleMode, PreambleTemplate, PronounConfig,
    RateLimitTomlConfig, ReadOnlyConfigError, ReplyToMode, StartupConfigError, TeamsConfig,
    TrustedWebhookCreators, VoiceConfig, attention_effect_guard, config_path, lkg_path,
    load_config, load_config_readonly, quarantine_path, reload_config, resolve_token,
    set_config_path, state_dir, subscribe_generation,
};
pub use config_store::{BoxError, ConfigStore, DiscordId};
