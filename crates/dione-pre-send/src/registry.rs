//! Process-wide outbound pipeline installation.

use crate::PreSendPipeline;
use std::sync::{Arc, OnceLock, RwLock};

static INSTALLED_PIPELINE: OnceLock<RwLock<Option<Arc<PreSendPipeline>>>> = OnceLock::new();

/// Installs or explicitly disables the process-wide outbound pipeline.
pub fn install_pipeline(pipeline: Option<Arc<PreSendPipeline>>) {
    *INSTALLED_PIPELINE
        .get_or_init(|| RwLock::new(None))
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = pipeline;
}

/// Borrows a snapshot of the currently installed process-wide pipeline.
pub fn installed_pipeline() -> Option<Arc<PreSendPipeline>> {
    INSTALLED_PIPELINE
        .get_or_init(|| RwLock::new(None))
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}
