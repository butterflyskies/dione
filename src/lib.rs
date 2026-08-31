pub mod batch;
pub mod bell_rings;
pub mod coalesce;
pub mod codex;
pub mod config;
pub(crate) mod config_candidate;
pub mod config_store;
pub mod config_watcher;
pub mod contradictionary;
pub mod delivery_buffer;
pub mod discord;
pub mod drop_ledger;
pub(crate) mod evidence;
pub mod gaie;
pub mod gate;
pub mod ingress_ledger;
pub mod mcp;
pub mod mute_store;
pub mod nameplates;
pub mod no_rly;
pub mod permissions;
pub mod pluralkit;
pub mod pre_send;
pub mod principal_policy;
pub mod pronouns;
pub mod queue;
pub mod rate_limiter;
#[expect(
    dead_code,
    reason = "standalone receipt-gate fixture is runtime-wired in a follow-up PR"
)]
pub(crate) mod receipt_gate;
pub mod state;
pub mod timestamp;
pub mod tracing_channel;
pub mod util;
