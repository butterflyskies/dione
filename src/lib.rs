pub mod batch;
pub mod bell_rings;
pub mod coalesce;
pub mod codex;
pub mod config;
pub mod config_store;
pub mod config_watcher;
pub mod contradictionary;
pub mod delivery_buffer;
pub mod discord;
pub mod gaie;
pub mod gate;
pub mod mcp;
pub mod nameplates;
pub mod permissions;
pub mod pre_send;
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
