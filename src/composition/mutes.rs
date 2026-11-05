//! Process-global mute-store assembly.

use dione_mutes::MuteStore;
use std::sync::Arc;

/// Process-global mute store, initialized at startup.
static MUTE_STORE: std::sync::OnceLock<Arc<MuteStore>> = std::sync::OnceLock::new();

/// Initialize the global mute store. Called once at startup.
pub fn init_global(store: MuteStore) {
    MUTE_STORE.set(Arc::new(store)).ok();
}

/// Load the global mute store. Returns `None` if not yet initialized.
pub fn global() -> Option<Arc<MuteStore>> {
    MUTE_STORE.get().cloned()
}
