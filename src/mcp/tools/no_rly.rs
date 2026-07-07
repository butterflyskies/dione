//! MCP tools for the no_rly audit journal: stats, condense, and vacuum.
//!
//! Release and rephrase live in [`super::messaging`] (they send messages);
//! these tools only read and maintain the journal, plus report the live
//! queue depth.

use std::sync::Arc;

use chrono::{Days, Utc};
use serde_json::{Value, json};

use crate::{
    config::LoadedConfig,
    no_rly::{
        consent::ConsentGate,
        journal::{Outcome, StatsFilter},
    },
};

/// Context for the journal tools.
pub struct NoRlyCtx {
    pub gate: Arc<ConsentGate>,
    pub config: Arc<LoadedConfig>,
}

/// The `no_rly_stats` tool: counts, timing, reasons, outcomes, chain and
/// parse validation over the audit journal, plus current queue depth.
pub async fn no_rly_stats(
    ctx: &NoRlyCtx,
    since_days: Option<u64>,
    outcome: Option<&str>,
    pattern: Option<&str>,
) -> Value {
    let outcome = match outcome {
        Some(s) => match Outcome::parse(s) {
            Some(o) => Some(o),
            None => {
                return json!({
                    "error": format!("invalid outcome: {s}; must be one of: released, rephrased, expired")
                });
            }
        },
        None => None,
    };
    let since = match since_days.map(days_ago) {
        Some(Some(since)) => Some(since),
        Some(None) => return json!({ "error": "since_days is out of range" }),
        None => None,
    };
    let filter = StatsFilter {
        since,
        outcome,
        pattern: pattern.map(str::to_string),
    };

    match ctx.gate.journal().stats(&filter) {
        Ok(stats) => {
            let mut value = serde_json::to_value(stats)
                .unwrap_or_else(|e| json!({ "error": format!("failed to serialize stats: {e}") }));
            value["pending"] = json!(ctx.gate.pending().await);
            value
        }
        Err(e) => json!({ "error": format!("failed to read journal: {e}") }),
    }
}

/// The `no_rly_condense` tool: fold raw bounce records older than the cutoff
/// (default: the configured raw retention window) into daily summaries.
pub async fn no_rly_condense(ctx: &NoRlyCtx, older_than_days: Option<u64>) -> Value {
    let days = older_than_days
        .unwrap_or(u64::from(ctx.config.raw.contradictionary.journal_raw_retention_days));
    let Some(cutoff) = days_ago(days) else {
        return json!({ "error": "older_than_days is out of range" });
    };
    match ctx.gate.journal().condense(cutoff) {
        Ok(report) => serde_json::to_value(report)
            .unwrap_or_else(|e| json!({ "error": format!("failed to serialize report: {e}") })),
        Err(e) => json!({ "error": format!("condense failed: {e}") }),
    }
}

/// The `no_rly_vacuum` tool: drop summaries older than the cutoff (default:
/// the configured summary retention window) and malformed lines, then
/// compact the journal file.
pub async fn no_rly_vacuum(ctx: &NoRlyCtx, older_than_days: Option<u64>) -> Value {
    let days = older_than_days.unwrap_or(u64::from(
        ctx.config.raw.contradictionary.journal_summary_retention_days,
    ));
    let Some(cutoff) = days_ago(days) else {
        return json!({ "error": "older_than_days is out of range" });
    };
    match ctx.gate.journal().vacuum(cutoff) {
        Ok(report) => serde_json::to_value(report)
            .unwrap_or_else(|e| json!({ "error": format!("failed to serialize report: {e}") })),
        Err(e) => json!({ "error": format!("vacuum failed: {e}") }),
    }
}

/// `now - days`, or `None` when the subtraction leaves the calendar.
fn days_ago(days: u64) -> Option<chrono::DateTime<Utc>> {
    Utc::now().checked_sub_days(Days::new(days))
}
