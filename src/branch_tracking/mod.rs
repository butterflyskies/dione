//! Conversation branch tracking for Discord channels.
//!
//! Identifies, annotates, and manages the lifecycle of concurrent conversation
//! branches within a channel. Three layers work together:
//!
//! 1. **Feature vector** (classify) — scores incoming messages against active
//!    branches using reply chains, topic similarity, temporal proximity, and
//!    participant overlap.
//! 2. **EWMA / windowed median** (track) — tracks per-branch message rate
//!    using interchangeable estimators behind the [`RateEstimator`] trait.
//! 3. **Bayesian CPD** (detect) — detects conversation change-points using
//!    the [`ChangePointDetector`] trait, with both stateful (Bayesian) and
//!    lightweight implementations.
//!
//! # Pipeline
//!
//! ```text
//! MessageInput -> FeatureVector (classify) -> BranchTracker (annotate) -> CPD (detect)
//! ```

pub mod cpd;
pub mod ewma;
pub mod feature_vector;
pub mod pipeline;
mod tracker;
mod types;

pub use tracker::BranchTracker;
pub use types::{
    Branch, BranchAnnotation, BranchId, BranchState, BranchTrackerConfig, MessageAnnotator,
    MessageInput, ParticipantSet, RateEstimator,
};
