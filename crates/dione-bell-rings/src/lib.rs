//! Memory recall with reconnectable MCP sessions, deterministic ranking, and compact rendering.
#![warn(missing_docs)]

mod backend;
mod diagnostic_http;
mod http_bounds;
mod rendering;
mod values;

pub use backend::{Bell, BellEvaluation, BellEvaluator, BellStatus, BellTimbre};
pub use rendering::render_bells;
pub use values::{
    BellProviderConfig, BellProviderUrl, BellScope, ProviderValueError, ProviderValueErrorKind,
    RecallOptions, RecallOptionsError, RecallOptionsErrorKind,
};

#[cfg(test)]
mod transport_tests;
