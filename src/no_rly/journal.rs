//! The durable audit journal for the no_rly consent gate.
//!
//! Every bounce ends here exactly once, when its outcome is known: released,
//! rephrased, or expired. Records are JSONL — one JSON object per line,
//! append-only — in `<state_dir>/no_rly_journal.jsonl`, so the history
//! survives process restarts and context clears. Chained bounces (a rephrase
//! that bounced again) link to their parent handle, preserving the full
//! (original, reason, replacement) lineage.
//!
//! # Storage and retention story
//!
//! Bounces are low-volume (a handful per day) and records are small (≤ ~2 KB
//! with truncated message text), so the journal is cheap to keep raw for a
//! long time — a year of raw records is well under a megabyte. Two
//! maintenance tools keep it bounded anyway:
//!
//! - **condense** folds raw bounce records older than the raw-retention
//!   window (default 365 days) into per-day/per-reason/per-outcome
//!   [`SummaryRecord`]s. Aggregate counts and latency survive; message text
//!   and chain links do not — that is the documented trade of detail for
//!   space, which is why the raw window defaults to generous.
//! - **vacuum** drops summaries older than the summary-retention window
//!   (default 730 days) and any malformed lines, then compacts the file.
//!
//! Both rewrite atomically (temp file + rename), so a crash mid-maintenance
//! leaves the original journal intact.

use crate::{
    no_rly::{judge::RejectReason, queue::HoldHandle},
    timestamp::Timestamp,
    util::truncate_chars,
};
use camino::{Utf8Path, Utf8PathBuf};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    fs, io,
    io::Write,
};
use tokio::sync::{mpsc, oneshot};

/// Name of the journal file, created under the channel state directory
/// (`~/.claude/channels/dione/no_rly_journal.jsonl`).
pub const JOURNAL_FILE_NAME: &str = "no_rly_journal.jsonl";

/// Maximum number of characters of message text retained per record. Longer
/// messages are truncated with an ellipsis to bound line size.
pub const JOURNAL_MAX_MESSAGE_LEN: usize = 2000;

/// Terminal outcome of a bounce. Every held message resolves to exactly one
/// of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    /// The construct sent the byte-identical held message.
    Released,
    /// The construct provided replacement text. Whether the replacement
    /// itself went out is told by the chain: a re-bounced replacement shows
    /// up as a child record with `parent` set to this record's handle.
    Rephrased,
    /// The handle timed out (or the process shut down) with no decision —
    /// the abandonment is data too.
    Expired,
}

impl Outcome {
    /// Every variant, for iterating the wire forms.
    const ALL: [Self; 3] = [Self::Released, Self::Rephrased, Self::Expired];

    /// Parse a tool-argument string (`"released"`, `"rephrased"`,
    /// `"expired"`). Defined as the inverse of [`Outcome::as_str`], so the
    /// serde `rename_all` annotation stays the single source of truth for
    /// valid strings (see the `outcome_wire_forms_agree_with_serde` test).
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|o| o.as_str() == s)
    }

    /// The lowercase wire form.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Released => "released",
            Self::Rephrased => "rephrased",
            Self::Expired => "expired",
        }
    }
}

/// One resolved bounce — a single JSONL line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BounceRecord {
    /// The handle the bounce was held under. `HoldHandle` is serde-transparent
    /// (a bare string on the wire), so the type carries through the journal
    /// boundary without changing the serialized shape.
    pub handle: HoldHandle,
    /// The handle of the bounce this one chains from, when a rephrase
    /// re-bounced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<HoldHandle>,
    /// The held message text (truncated to [`JOURNAL_MAX_MESSAGE_LEN`]).
    pub message: String,
    /// The rules that fired.
    pub reason: RejectReason,
    /// How the bounce resolved.
    pub outcome: Outcome,
    /// The replacement text for [`Outcome::Rephrased`] (truncated).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement: Option<String>,
    /// When the message bounced.
    pub bounced_at: Timestamp,
    /// When the outcome landed.
    pub resolved_at: Timestamp,
    /// Bounce-to-action latency in milliseconds — the habituation signal.
    /// Sub-second releases mean the reflex is reforming.
    pub latency_ms: u64,
}

/// A condensed day of bounces: everything raw records carry except message
/// text and chain links, aggregated per (day, reason patterns, outcome).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SummaryRecord {
    /// UTC date of resolution. Serializes as `YYYY-MM-DD`.
    pub date: NaiveDate,
    /// The comma-joined pattern key ([`RejectReason::patterns`]).
    pub patterns: String,
    /// The shared outcome of the aggregated bounces.
    pub outcome: Outcome,
    /// How many bounces this summary stands for.
    pub count: u64,
    /// Sum of the aggregated latencies (mean = total / count).
    pub latency_ms_total: u64,
    /// Largest single latency in the group.
    pub latency_ms_max: u64,
    /// When the condense that produced (or last merged into) this summary
    /// ran.
    pub condensed_at: Timestamp,
}

/// A journal line. The `kind` tag keeps raw records and summaries
/// distinguishable in one file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JournalRecord {
    /// One resolved bounce.
    Bounce(BounceRecord),
    /// One condensed aggregate.
    Summary(SummaryRecord),
}

/// Everything a full read of the journal yields: parsed records, plus lines
/// kept verbatim so no data is silently lost — split into two kinds:
///
/// - **unknown**: valid JSON whose `kind` this build does not recognize (a
///   record type from a newer schema, seen after a downgrade or across a
///   shared state dir). These are preserved by *every* maintenance op —
///   vacuum must never delete a well-formed record just because it is newer
///   than this binary.
/// - **malformed**: lines that are not valid JSON at all (genuine corruption).
///   Vacuum drops these; they carry no recoverable meaning.
#[derive(Debug, Default)]
pub struct JournalScan {
    /// Successfully parsed records, in file order.
    pub records: Vec<JournalRecord>,
    /// Valid JSON with an unrecognized shape (e.g. a future `kind`), kept
    /// verbatim and never dropped.
    pub unknown: Vec<String>,
    /// Lines that are not valid JSON, kept verbatim until vacuum drops them.
    pub malformed: Vec<String>,
}

/// Filter for [`Journal::stats`]. All fields are conjunctive; `None` means
/// "don't filter on this".
#[derive(Debug, Default, Clone)]
pub struct StatsFilter {
    /// Only bounces resolved at or after this instant.
    pub since: Option<DateTime<Utc>>,
    /// Only bounces with this outcome.
    pub outcome: Option<Outcome>,
    /// Only bounces whose reason includes this pattern.
    pub pattern: Option<String>,
}

/// Latency aggregates over the filtered bounces, in milliseconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LatencyStats {
    /// Smallest observed bounce-to-action latency.
    pub min_ms: u64,
    /// Median bounce-to-action latency.
    pub p50_ms: u64,
    /// Mean bounce-to-action latency.
    pub mean_ms: u64,
    /// Largest observed bounce-to-action latency.
    pub max_ms: u64,
}

/// What [`Journal::stats`] reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct JournalStats {
    /// Raw bounce records matching the filter.
    pub bounces: u64,
    /// Bounce counts keyed by outcome.
    pub by_outcome: BTreeMap<String, u64>,
    /// Bounce counts keyed by individual matched pattern.
    pub by_pattern: BTreeMap<String, u64>,
    /// Latency aggregates, absent when nothing matched.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency: Option<LatencyStats>,
    /// Filtered bounces that chain from a parent (rephrase re-bounces).
    pub chained: u64,
    /// Audit validation: parents referenced by any record in the journal
    /// that no record documents. Non-zero means the chain story has holes
    /// (e.g. records lost to a crash before resolution).
    pub dangling_parents: u64,
    /// Summary records in the journal (unfiltered).
    pub summaries: u64,
    /// Total bounces represented by those summaries.
    pub summarized_bounces: u64,
    /// Audit validation: valid-JSON lines with an unrecognized shape (e.g. a
    /// record `kind` from a newer schema). Preserved across maintenance, never
    /// dropped — non-zero means this binary is reading a journal it does not
    /// fully understand.
    pub unknown_lines: u64,
    /// Audit validation: lines that are not valid JSON at all.
    pub malformed_lines: u64,
}

/// What [`Journal::condense`] did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CondenseReport {
    /// Raw bounce records folded into summaries.
    pub condensed_bounces: u64,
    /// Summary records in the file after the rewrite.
    pub summaries: u64,
    /// Raw bounce records kept (newer than the cutoff).
    pub kept_bounces: u64,
    /// File size before, in bytes.
    pub bytes_before: u64,
    /// File size after, in bytes.
    pub bytes_after: u64,
}

/// What [`Journal::vacuum`] did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VacuumReport {
    /// Summaries dropped for being older than the cutoff.
    pub dropped_summaries: u64,
    /// Malformed lines dropped.
    pub dropped_malformed: u64,
    /// Records kept.
    pub kept: u64,
    /// File size before, in bytes.
    pub bytes_before: u64,
    /// File size after, in bytes.
    pub bytes_after: u64,
}

/// The on-disk journal file and its synchronous operations.
///
/// This is the *file owner*: `append`, `condense`, and `vacuum` mutate the
/// file; `load` and `stats` read it. It is deliberately not shared across
/// writers — at runtime a single [`JournalHandle`] task owns the one instance
/// that writes, so there is never a second concurrent writer to race. Reads
/// go straight to the path and are safe against the atomic temp-file rename
/// (a reader sees the old or new file whole, never a splice).
#[derive(Debug, Clone)]
pub struct Journal {
    path: Utf8PathBuf,
}

impl Journal {
    /// A journal living in `dir` (the channel state directory).
    pub fn new(dir: &Utf8Path) -> Self {
        Self {
            path: dir.join(JOURNAL_FILE_NAME),
        }
    }

    /// The journal file path.
    pub fn path(&self) -> &Utf8Path {
        &self.path
    }

    /// Append one record as a JSONL line, creating the file and any missing
    /// parent directories on demand.
    pub fn append(&self, record: &JournalRecord) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut line = serde_json::to_string(record).map_err(io::Error::other)?;
        line.push('\n');
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(line.as_bytes())
    }

    /// Read and parse the whole journal. A missing file is an empty journal.
    pub fn load(&self) -> io::Result<JournalScan> {
        let contents = match fs::read_to_string(&self.path) {
            Ok(c) => c,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(JournalScan::default()),
            Err(e) => return Err(e),
        };
        let mut scan = JournalScan::default();
        for line in contents.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<JournalRecord>(line) {
                Ok(record) => scan.records.push(record),
                // Valid JSON we can't map onto a known record (an unrecognized
                // `kind` from a newer schema) is preserved as unknown, not
                // destroyed as garbage. Only non-JSON is malformed.
                Err(_) if serde_json::from_str::<serde_json::Value>(line).is_ok() => {
                    scan.unknown.push(line.to_string())
                }
                Err(_) => scan.malformed.push(line.to_string()),
            }
        }
        Ok(scan)
    }

    /// Aggregate the journal: counts, timing, reasons, outcomes, chain and
    /// parse validation.
    pub fn stats(&self, filter: &StatsFilter) -> io::Result<JournalStats> {
        let scan = self.load()?;

        let bounces: Vec<&BounceRecord> = scan
            .records
            .iter()
            .filter_map(|r| match r {
                JournalRecord::Bounce(b) => Some(b),
                JournalRecord::Summary(_) => None,
            })
            .collect();

        // Chain validation runs over the full record set, not the filtered
        // view — a parent outside the filter window is not dangling.
        let known_handles: HashSet<&str> = bounces.iter().map(|b| b.handle.as_str()).collect();
        let dangling_parents = bounces
            .iter()
            .filter_map(|b| b.parent.as_ref().map(HoldHandle::as_str))
            .filter(|parent| !known_handles.contains(parent))
            .count() as u64;

        let matched: Vec<&&BounceRecord> = bounces
            .iter()
            .filter(|b| {
                filter
                    .since
                    .is_none_or(|since| *b.resolved_at.as_datetime() >= since)
            })
            .filter(|b| filter.outcome.is_none_or(|o| b.outcome == o))
            .filter(|b| {
                filter
                    .pattern
                    .as_deref()
                    .is_none_or(|p| b.reason.matches.iter().any(|m| m.pattern == p))
            })
            .collect();

        let mut by_outcome: BTreeMap<String, u64> = BTreeMap::new();
        let mut by_pattern: BTreeMap<String, u64> = BTreeMap::new();
        for b in &matched {
            *by_outcome
                .entry(b.outcome.as_str().to_string())
                .or_default() += 1;
            for m in &b.reason.matches {
                *by_pattern.entry(m.pattern.clone()).or_default() += 1;
            }
        }

        let mut latencies: Vec<u64> = matched.iter().map(|b| b.latency_ms).collect();
        latencies.sort_unstable();
        let latency = latencies.split_first().map(|(min, _)| LatencyStats {
            min_ms: *min,
            p50_ms: median(&latencies),
            mean_ms: latencies.iter().sum::<u64>() / latencies.len() as u64,
            max_ms: *latencies.last().expect("non-empty after split_first"),
        });

        let (summaries, summarized_bounces) = scan
            .records
            .iter()
            .filter_map(|r| match r {
                JournalRecord::Summary(s) => Some(s.count),
                JournalRecord::Bounce(_) => None,
            })
            .fold((0u64, 0u64), |(n, total), count| (n + 1, total + count));

        Ok(JournalStats {
            bounces: matched.len() as u64,
            by_outcome,
            by_pattern,
            latency,
            chained: matched.iter().filter(|b| b.parent.is_some()).count() as u64,
            dangling_parents,
            summaries,
            summarized_bounces,
            unknown_lines: scan.unknown.len() as u64,
            malformed_lines: scan.malformed.len() as u64,
        })
    }

    /// Fold raw bounce records resolved before `cutoff` into per-day
    /// summaries, merging into existing summaries with the same
    /// (date, patterns, outcome) key. Malformed lines are preserved verbatim
    /// — dropping them is vacuum's explicitly requested job.
    pub fn condense(&self, cutoff: DateTime<Utc>) -> io::Result<CondenseReport> {
        let bytes_before = self.file_size()?;
        let scan = self.load()?;

        // Split into existing summaries and raw bounces up front, so we can
        // reason about the chain before folding anything.
        let mut existing_summaries: Vec<SummaryRecord> = Vec::new();
        let mut bounces: Vec<BounceRecord> = Vec::new();
        for record in scan.records {
            match record {
                JournalRecord::Summary(s) => existing_summaries.push(s),
                JournalRecord::Bounce(b) => bounces.push(b),
            }
        }

        // A bounce is a fold candidate iff it resolved before the cutoff.
        // But a candidate whose handle is still referenced as the `parent` of
        // a bounce we are keeping must itself be kept raw — otherwise folding
        // it away turns its child into a phantom `dangling_parent`. Promote
        // referenced candidates to kept, transitively (a kept child keeps its
        // parent, which may keep *its* parent), until the set is stable.
        let mut keep: Vec<bool> = bounces
            .iter()
            .map(|b| *b.resolved_at.as_datetime() >= cutoff)
            .collect();
        loop {
            let kept_parent_refs: HashSet<&str> = bounces
                .iter()
                .zip(&keep)
                .filter(|(_, k)| **k)
                .filter_map(|(b, _)| b.parent.as_ref().map(HoldHandle::as_str))
                .collect();
            let mut changed = false;
            for (i, b) in bounces.iter().enumerate() {
                if !keep[i] && kept_parent_refs.contains(b.handle.as_str()) {
                    keep[i] = true;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }

        let mut summaries: BTreeMap<(NaiveDate, String, Outcome), SummaryRecord> = BTreeMap::new();
        for s in existing_summaries {
            merge_summary(&mut summaries, s);
        }
        let mut kept: Vec<JournalRecord> = Vec::new();
        let mut condensed_bounces: u64 = 0;
        for (b, keep_it) in bounces.into_iter().zip(keep) {
            if keep_it {
                kept.push(JournalRecord::Bounce(b));
            } else {
                condensed_bounces += 1;
                merge_summary(
                    &mut summaries,
                    SummaryRecord {
                        date: b.resolved_at.as_datetime().date_naive(),
                        patterns: b.reason.patterns(),
                        outcome: b.outcome,
                        count: 1,
                        latency_ms_total: b.latency_ms,
                        latency_ms_max: b.latency_ms,
                        condensed_at: Timestamp::now(),
                    },
                );
            }
        }

        let summary_count = summaries.len() as u64;
        let kept_count = kept.len() as u64;
        let records = summaries
            .into_values()
            .map(JournalRecord::Summary)
            .chain(kept)
            .collect::<Vec<_>>();
        // Unknown-shape and malformed lines are preserved verbatim — condense
        // folds bounces, it never destroys data (that is vacuum's job, and
        // even vacuum keeps unknown-shape lines).
        self.rewrite(&records, &scan.unknown, &scan.malformed)?;

        Ok(CondenseReport {
            condensed_bounces,
            summaries: summary_count,
            kept_bounces: kept_count,
            bytes_before,
            bytes_after: self.file_size()?,
        })
    }

    /// Drop summaries dated before `cutoff` and every malformed (non-JSON)
    /// line, then compact the file. Valid-JSON lines with an unrecognized
    /// shape are **kept** — vacuum is not licensed to delete a well-formed
    /// record just because this binary predates its schema.
    pub fn vacuum(&self, cutoff: DateTime<Utc>) -> io::Result<VacuumReport> {
        let bytes_before = self.file_size()?;
        let scan = self.load()?;
        let cutoff_date = cutoff.date_naive();

        let (kept, dropped): (Vec<JournalRecord>, Vec<JournalRecord>) =
            scan.records.into_iter().partition(|r| match r {
                JournalRecord::Summary(s) => s.date >= cutoff_date,
                JournalRecord::Bounce(_) => true,
            });

        self.rewrite(&kept, &scan.unknown, &[])?;

        Ok(VacuumReport {
            dropped_summaries: dropped.len() as u64,
            dropped_malformed: scan.malformed.len() as u64,
            kept: kept.len() as u64 + scan.unknown.len() as u64,
            bytes_before,
            bytes_after: self.file_size()?,
        })
    }

    /// Atomically replace the journal with `records`, then `unknown` and
    /// `malformed` lines kept verbatim. Writes a temp file, fsyncs it so the
    /// bytes are durable before the swap, then renames — a crash mid-rewrite
    /// leaves the original journal intact and a crash just after leaves the
    /// fully-written replacement, never a half-synced file.
    fn rewrite(
        &self,
        records: &[JournalRecord],
        unknown: &[String],
        malformed: &[String],
    ) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out = String::new();
        for record in records {
            out.push_str(&serde_json::to_string(record).map_err(io::Error::other)?);
            out.push('\n');
        }
        for line in unknown.iter().chain(malformed) {
            out.push_str(line);
            out.push('\n');
        }
        let tmp = self.path.with_extension("jsonl.tmp");
        let mut file = fs::File::create(&tmp)?;
        file.write_all(out.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, &self.path)
    }

    fn file_size(&self) -> io::Result<u64> {
        match fs::metadata(&self.path) {
            Ok(m) => Ok(m.len()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(e),
        }
    }
}

/// A write request handed to the single [`JournalHandle`] task. Funnelling
/// every mutation through one owner makes the append-vs-rewrite race
/// structurally impossible: the writer processes commands strictly in order,
/// so an append can never land between a rewrite's load and rename.
enum JournalCommand {
    Append(Box<JournalRecord>),
    Condense {
        cutoff: DateTime<Utc>,
        reply: oneshot::Sender<io::Result<CondenseReport>>,
    },
    Vacuum {
        cutoff: DateTime<Utc>,
        reply: oneshot::Sender<io::Result<VacuumReport>>,
    },
    /// Ack once every previously-queued command has been processed — used at
    /// shutdown to guarantee drained-as-expired records reach disk before exit.
    Flush(oneshot::Sender<()>),
}

/// The single owner of journal *writes*. Clone-cheap: every clone shares the
/// one background writer task, so no matter how many gate paths append (the
/// request loop, the sweeper, the shutdown drain) there is exactly one writer.
///
/// Reads (`load`, `stats`, `path`) are served directly off a read-only
/// [`Journal`] clone without touching the writer — they cannot mutate, and the
/// atomic rename keeps them consistent.
#[derive(Debug, Clone)]
pub struct JournalHandle {
    tx: mpsc::UnboundedSender<JournalCommand>,
    /// Read-only view of the same file. Never used to write.
    reader: Journal,
}

impl JournalHandle {
    /// Spawn the writer task for a journal in `dir` and return a handle to it.
    /// Must be called from within a Tokio runtime.
    pub fn spawn(dir: &Utf8Path) -> Self {
        let journal = Journal::new(dir);
        let writer = journal.clone();
        let (tx, mut rx) = mpsc::unbounded_channel::<JournalCommand>();
        tokio::spawn(async move {
            while let Some(cmd) = rx.recv().await {
                match cmd {
                    JournalCommand::Append(record) => {
                        // A journal write failure must not undo or block the
                        // consented action that produced it — log and move on.
                        if let Err(e) = writer.append(&record) {
                            tracing::warn!(error = %e, "failed to append no_rly journal record");
                        }
                    }
                    JournalCommand::Condense { cutoff, reply } => {
                        let _ = reply.send(writer.condense(cutoff));
                    }
                    JournalCommand::Vacuum { cutoff, reply } => {
                        let _ = reply.send(writer.vacuum(cutoff));
                    }
                    JournalCommand::Flush(ack) => {
                        let _ = ack.send(());
                    }
                }
            }
        });
        Self {
            tx,
            reader: journal,
        }
    }

    /// The journal file path.
    pub fn path(&self) -> &Utf8Path {
        self.reader.path()
    }

    /// Queue one record for the writer. Fire-and-forget: enqueue is
    /// non-blocking and a dropped writer is logged, never surfaced, so a
    /// journal problem can never block or undo the consented action.
    pub fn append(&self, record: &JournalRecord) {
        if self
            .tx
            .send(JournalCommand::Append(Box::new(record.clone())))
            .is_err()
        {
            tracing::warn!("no_rly journal writer stopped; dropping record");
        }
    }

    /// Fold old raw bounces into summaries via the writer, awaiting the report.
    pub async fn condense(&self, cutoff: DateTime<Utc>) -> io::Result<CondenseReport> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(JournalCommand::Condense { cutoff, reply })
            .map_err(|_| io::Error::other("journal writer stopped"))?;
        rx.await
            .map_err(|_| io::Error::other("journal writer dropped the request"))?
    }

    /// Drop old summaries and malformed lines via the writer, awaiting the
    /// report.
    pub async fn vacuum(&self, cutoff: DateTime<Utc>) -> io::Result<VacuumReport> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(JournalCommand::Vacuum { cutoff, reply })
            .map_err(|_| io::Error::other("journal writer stopped"))?;
        rx.await
            .map_err(|_| io::Error::other("journal writer dropped the request"))?
    }

    /// Wait until every record queued before this call has been written.
    /// Returns immediately if the writer has already stopped.
    pub async fn flush(&self) {
        let (ack, rx) = oneshot::channel();
        if self.tx.send(JournalCommand::Flush(ack)).is_ok() {
            let _ = rx.await;
        }
    }

    /// Aggregate the journal (read-only, served without the writer).
    pub fn stats(&self, filter: &StatsFilter) -> io::Result<JournalStats> {
        self.reader.stats(filter)
    }

    /// Read and parse the whole journal (read-only, served without the writer).
    pub fn load(&self) -> io::Result<JournalScan> {
        self.reader.load()
    }
}

/// Median of a sorted, non-empty slice. On an even count it averages the two
/// middle elements rather than taking the upper one — `p50` is the habituation
/// signal, and biasing it high on small even samples would mask exactly the
/// sub-second releases the metric exists to catch.
fn median(sorted: &[u64]) -> u64 {
    let n = sorted.len();
    debug_assert!(n > 0, "median of empty slice");
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        // Average without overflow.
        let lo = sorted[n / 2 - 1];
        let hi = sorted[n / 2];
        lo + (hi - lo) / 2
    }
}

/// Merge a summary into the accumulator keyed by (date, patterns, outcome).
fn merge_summary(
    acc: &mut BTreeMap<(NaiveDate, String, Outcome), SummaryRecord>,
    summary: SummaryRecord,
) {
    let key = (summary.date, summary.patterns.clone(), summary.outcome);
    acc.entry(key)
        .and_modify(|existing| {
            existing.count += summary.count;
            existing.latency_ms_total += summary.latency_ms_total;
            existing.latency_ms_max = existing.latency_ms_max.max(summary.latency_ms_max);
            existing.condensed_at = summary.condensed_at.clone();
        })
        .or_insert(summary);
}

/// A resolved bounce on its way into the journal — the caller-side view
/// before truncation and resolution stamping.
pub(crate) struct ResolvedBounce<'a> {
    pub handle: &'a str,
    pub parent: Option<&'a str>,
    pub message: &'a str,
    pub reason: RejectReason,
    pub outcome: Outcome,
    pub replacement: Option<&'a str>,
    pub bounced_at: Timestamp,
    /// Caller-measured monotonic bounce-to-action duration.
    pub latency_ms: u64,
}

impl ResolvedBounce<'_> {
    /// Build the journal record: message and replacement text are truncated
    /// to bound line size, and the resolution time is stamped now.
    pub(crate) fn into_record(self) -> JournalRecord {
        JournalRecord::Bounce(BounceRecord {
            handle: HoldHandle::new(self.handle),
            parent: self.parent.map(HoldHandle::new),
            message: truncate_chars(self.message, JOURNAL_MAX_MESSAGE_LEN),
            reason: self.reason,
            outcome: self.outcome,
            replacement: self
                .replacement
                .map(|r| truncate_chars(r, JOURNAL_MAX_MESSAGE_LEN)),
            bounced_at: self.bounced_at,
            resolved_at: Timestamp::now(),
            latency_ms: self.latency_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::no_rly::judge::ReasonEntry;
    use camino::Utf8PathBuf;
    use chrono::TimeZone;
    use tempfile::TempDir;

    fn temp_journal() -> (TempDir, Journal) {
        let dir = TempDir::new().unwrap();
        let path = Utf8PathBuf::try_from(dir.path().to_path_buf()).unwrap();
        let journal = Journal::new(&path);
        (dir, journal)
    }

    fn reason(pattern: &str) -> RejectReason {
        RejectReason {
            matches: vec![ReasonEntry {
                pattern: pattern.into(),
                reason: Some("test reason".into()),
            }],
        }
    }

    fn record(handle: &str, outcome: Outcome, resolved_at: &str, latency_ms: u64) -> JournalRecord {
        JournalRecord::Bounce(BounceRecord {
            handle: handle.into(),
            parent: None,
            message: "held text".into(),
            reason: reason("straightforward"),
            outcome,
            replacement: None,
            bounced_at: Timestamp::parse("2026-07-07T10:00:00+00:00").unwrap(),
            resolved_at: Timestamp::parse(resolved_at).unwrap(),
            latency_ms,
        })
    }

    #[test]
    fn append_then_load_round_trips() {
        let (_dir, journal) = temp_journal();
        let rec = record(
            "nr-0001-1",
            Outcome::Released,
            "2026-07-07T10:00:05+00:00",
            5000,
        );
        journal.append(&rec).unwrap();
        let scan = journal.load().unwrap();
        assert_eq!(scan.records, vec![rec]);
        assert!(scan.malformed.is_empty());
    }

    #[test]
    fn missing_file_is_empty_journal() {
        let (_dir, journal) = temp_journal();
        let scan = journal.load().unwrap();
        assert!(scan.records.is_empty());
        assert!(scan.malformed.is_empty());
    }

    #[test]
    fn malformed_lines_are_kept_and_counted() {
        let (_dir, journal) = temp_journal();
        journal
            .append(&record(
                "nr-0001-1",
                Outcome::Expired,
                "2026-07-07T10:03:00+00:00",
                180_000,
            ))
            .unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(journal.path())
            .unwrap()
            .write_all(b"{not json}\n")
            .unwrap();
        let scan = journal.load().unwrap();
        assert_eq!(scan.records.len(), 1);
        assert_eq!(scan.malformed, vec!["{not json}".to_string()]);
    }

    // ── Wire contract: the serialized shape IS the interface ────────────

    #[test]
    fn outcome_wire_forms_agree_with_serde() {
        for outcome in Outcome::ALL {
            assert_eq!(
                serde_json::to_value(outcome).unwrap(),
                serde_json::json!(outcome.as_str()),
                "as_str must be exactly the serde wire form"
            );
            assert_eq!(
                Outcome::parse(outcome.as_str()),
                Some(outcome),
                "parse must be the inverse of as_str"
            );
            assert_eq!(
                serde_json::from_value::<Outcome>(serde_json::json!(outcome.as_str())).unwrap(),
                outcome,
                "the serde annotation accepts every as_str form"
            );
        }
        assert_eq!(Outcome::parse("Released"), None, "wire forms are lowercase");
    }

    #[test]
    fn bounce_record_wire_shape() {
        let rec = JournalRecord::Bounce(BounceRecord {
            handle: "nr-3f92-7".into(),
            parent: Some("nr-3f92-6".into()),
            message: "a straightforward plan".into(),
            reason: reason("straightforward"),
            outcome: Outcome::Rephrased,
            replacement: "a plan".to_string().into(),
            bounced_at: Timestamp::parse("2026-07-07T10:00:00+00:00").unwrap(),
            resolved_at: Timestamp::parse("2026-07-07T10:00:42+00:00").unwrap(),
            latency_ms: 42_000,
        });
        insta::assert_json_snapshot!(rec);
    }

    #[test]
    fn summary_record_wire_shape() {
        let rec = JournalRecord::Summary(SummaryRecord {
            date: NaiveDate::from_ymd_opt(2026, 7, 7).unwrap(),
            patterns: "straightforward".into(),
            outcome: Outcome::Released,
            count: 3,
            latency_ms_total: 12_000,
            latency_ms_max: 8_000,
            condensed_at: Timestamp::parse("2027-07-07T00:00:00+00:00").unwrap(),
        });
        insta::assert_json_snapshot!(rec);
    }

    #[test]
    fn optional_fields_absent_when_none() {
        let rec = record(
            "nr-0001-1",
            Outcome::Released,
            "2026-07-07T10:00:05+00:00",
            5000,
        );
        let value = serde_json::to_value(&rec).unwrap();
        assert!(
            value.get("parent").is_none(),
            "parent: None must not serialize"
        );
        assert!(
            value.get("replacement").is_none(),
            "replacement: None must not serialize"
        );
        assert_eq!(value["kind"], "bounce");
    }

    #[test]
    fn journal_record_round_trips_through_serde() {
        let original = JournalRecord::Bounce(BounceRecord {
            handle: "nr-3f92-7".into(),
            parent: Some("nr-3f92-6".into()),
            message: "text".into(),
            reason: reason("trivial"),
            outcome: Outcome::Expired,
            replacement: None,
            bounced_at: Timestamp::parse("2026-07-07T10:00:00+00:00").unwrap(),
            resolved_at: Timestamp::parse("2026-07-07T10:03:00+00:00").unwrap(),
            latency_ms: 180_000,
        });
        let json = serde_json::to_string(&original).unwrap();
        let parsed: JournalRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);
    }

    // ── Stats ────────────────────────────────────────────────────────────

    #[test]
    fn stats_aggregates_counts_and_latency() {
        let (_dir, journal) = temp_journal();
        journal
            .append(&record(
                "nr-0001-1",
                Outcome::Released,
                "2026-07-07T10:00:02+00:00",
                2000,
            ))
            .unwrap();
        journal
            .append(&record(
                "nr-0001-2",
                Outcome::Released,
                "2026-07-07T11:00:04+00:00",
                4000,
            ))
            .unwrap();
        journal
            .append(&record(
                "nr-0001-3",
                Outcome::Expired,
                "2026-07-07T12:03:00+00:00",
                180_000,
            ))
            .unwrap();

        let stats = journal.stats(&StatsFilter::default()).unwrap();
        assert_eq!(stats.bounces, 3);
        assert_eq!(stats.by_outcome["released"], 2);
        assert_eq!(stats.by_outcome["expired"], 1);
        assert_eq!(stats.by_pattern["straightforward"], 3);
        let latency = stats.latency.unwrap();
        assert_eq!(latency.min_ms, 2000);
        assert_eq!(latency.max_ms, 180_000);
        assert_eq!(latency.mean_ms, 62_000);
        assert_eq!(latency.p50_ms, 4000);
        assert_eq!(stats.malformed_lines, 0);
    }

    #[test]
    fn stats_filters_by_outcome_and_since() {
        let (_dir, journal) = temp_journal();
        journal
            .append(&record(
                "nr-0001-1",
                Outcome::Released,
                "2026-07-01T10:00:00+00:00",
                1000,
            ))
            .unwrap();
        journal
            .append(&record(
                "nr-0001-2",
                Outcome::Expired,
                "2026-07-07T10:00:00+00:00",
                2000,
            ))
            .unwrap();

        let by_outcome = journal
            .stats(&StatsFilter {
                outcome: Some(Outcome::Expired),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(by_outcome.bounces, 1);

        let since = journal
            .stats(&StatsFilter {
                since: Some(Utc.with_ymd_and_hms(2026, 7, 5, 0, 0, 0).unwrap()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(since.bounces, 1);
    }

    #[test]
    fn stats_reports_chains_and_dangling_parents() {
        let (_dir, journal) = temp_journal();
        journal
            .append(&record(
                "nr-0001-1",
                Outcome::Rephrased,
                "2026-07-07T10:00:00+00:00",
                1000,
            ))
            .unwrap();
        let mut child = record(
            "nr-0001-2",
            Outcome::Released,
            "2026-07-07T10:01:00+00:00",
            500,
        );
        if let JournalRecord::Bounce(ref mut b) = child {
            b.parent = Some("nr-0001-1".into());
        }
        journal.append(&child).unwrap();
        let mut orphan = record(
            "nr-0001-9",
            Outcome::Released,
            "2026-07-07T10:02:00+00:00",
            500,
        );
        if let JournalRecord::Bounce(ref mut b) = orphan {
            b.parent = Some("nr-dead-1".into());
        }
        journal.append(&orphan).unwrap();

        let stats = journal.stats(&StatsFilter::default()).unwrap();
        assert_eq!(stats.chained, 2);
        assert_eq!(stats.dangling_parents, 1);
    }

    // ── Condense ─────────────────────────────────────────────────────────

    #[test]
    fn condense_folds_old_bounces_and_keeps_new() {
        let (_dir, journal) = temp_journal();
        journal
            .append(&record(
                "nr-0001-1",
                Outcome::Released,
                "2026-01-01T10:00:00+00:00",
                2000,
            ))
            .unwrap();
        journal
            .append(&record(
                "nr-0001-2",
                Outcome::Released,
                "2026-01-01T11:00:00+00:00",
                4000,
            ))
            .unwrap();
        journal
            .append(&record(
                "nr-0001-3",
                Outcome::Released,
                "2026-07-07T10:00:00+00:00",
                1000,
            ))
            .unwrap();

        let cutoff = Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap();
        let report = journal.condense(cutoff).unwrap();
        assert_eq!(report.condensed_bounces, 2);
        assert_eq!(report.summaries, 1);
        assert_eq!(report.kept_bounces, 1);

        let scan = journal.load().unwrap();
        let summaries: Vec<&SummaryRecord> = scan
            .records
            .iter()
            .filter_map(|r| match r {
                JournalRecord::Summary(s) => Some(s),
                JournalRecord::Bounce(_) => None,
            })
            .collect();
        assert_eq!(summaries.len(), 1);
        assert_eq!(
            summaries[0].date,
            NaiveDate::from_ymd_opt(2026, 1, 1).unwrap()
        );
        assert_eq!(summaries[0].count, 2);
        assert_eq!(summaries[0].latency_ms_total, 6000);
        assert_eq!(summaries[0].latency_ms_max, 4000);
        assert_eq!(summaries[0].patterns, "straightforward");
    }

    #[test]
    fn condense_merges_into_existing_summaries() {
        let (_dir, journal) = temp_journal();
        journal
            .append(&record(
                "nr-0001-1",
                Outcome::Released,
                "2026-01-01T10:00:00+00:00",
                2000,
            ))
            .unwrap();
        let cutoff = Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap();
        journal.condense(cutoff).unwrap();

        journal
            .append(&record(
                "nr-0001-2",
                Outcome::Released,
                "2026-01-01T12:00:00+00:00",
                6000,
            ))
            .unwrap();
        let report = journal.condense(cutoff).unwrap();
        assert_eq!(
            report.summaries, 1,
            "same-key summaries must merge, not duplicate"
        );

        let scan = journal.load().unwrap();
        match &scan.records[0] {
            JournalRecord::Summary(s) => {
                assert_eq!(s.count, 2);
                assert_eq!(s.latency_ms_total, 8000);
                assert_eq!(s.latency_ms_max, 6000);
            }
            other => panic!("expected summary first, got {other:?}"),
        }
    }

    #[test]
    fn condense_recovers_from_stale_tmp_of_a_crashed_rewrite() {
        let (_dir, journal) = temp_journal();
        journal
            .append(&record(
                "nr-0001-1",
                Outcome::Released,
                "2026-01-01T10:00:00+00:00",
                2000,
            ))
            .unwrap();
        journal
            .append(&record(
                "nr-0001-2",
                Outcome::Released,
                "2026-07-07T10:00:00+00:00",
                1000,
            ))
            .unwrap();

        // A crash mid-rewrite leaves a stale temp file behind; the journal
        // itself is intact (rename never happened). The next maintenance run
        // must succeed and must not resurrect any of the stale bytes.
        let stale_tmp = journal.path().with_extension("jsonl.tmp");
        fs::write(&stale_tmp, "half-written garbage from a dead process\n").unwrap();

        let cutoff = Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap();
        let report = journal
            .condense(cutoff)
            .expect("stale tmp must not break condense");
        assert_eq!(report.condensed_bounces, 1);
        assert_eq!(report.kept_bounces, 1);

        let scan = journal.load().unwrap();
        assert_eq!(scan.records.len(), 2, "one summary plus one kept bounce");
        assert!(
            scan.malformed.is_empty(),
            "no stale bytes may leak into the journal"
        );
        assert!(!stale_tmp.exists(), "the rewrite consumes the tmp path");
    }

    #[test]
    fn condense_preserves_malformed_lines() {
        let (_dir, journal) = temp_journal();
        journal
            .append(&record(
                "nr-0001-1",
                Outcome::Released,
                "2026-01-01T10:00:00+00:00",
                2000,
            ))
            .unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(journal.path())
            .unwrap()
            .write_all(b"corrupt line\n")
            .unwrap();

        let cutoff = Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap();
        journal.condense(cutoff).unwrap();
        let scan = journal.load().unwrap();
        assert_eq!(
            scan.malformed,
            vec!["corrupt line".to_string()],
            "condense must not silently destroy unparseable data"
        );
    }

    // ── Vacuum ───────────────────────────────────────────────────────────

    #[test]
    fn vacuum_drops_old_summaries_and_malformed() {
        let (_dir, journal) = temp_journal();
        journal
            .append(&JournalRecord::Summary(SummaryRecord {
                date: NaiveDate::from_ymd_opt(2024, 1, 1).unwrap(),
                patterns: "straightforward".into(),
                outcome: Outcome::Released,
                count: 5,
                latency_ms_total: 10_000,
                latency_ms_max: 4000,
                condensed_at: Timestamp::parse("2025-01-01T00:00:00+00:00").unwrap(),
            }))
            .unwrap();
        journal
            .append(&record(
                "nr-0001-1",
                Outcome::Released,
                "2026-07-07T10:00:00+00:00",
                1000,
            ))
            .unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(journal.path())
            .unwrap()
            .write_all(b"corrupt line\n")
            .unwrap();

        let cutoff = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let report = journal.vacuum(cutoff).unwrap();
        assert_eq!(report.dropped_summaries, 1);
        assert_eq!(report.dropped_malformed, 1);
        assert_eq!(report.kept, 1);
        assert!(report.bytes_after < report.bytes_before);

        let scan = journal.load().unwrap();
        assert_eq!(scan.records.len(), 1);
        assert!(scan.malformed.is_empty());
        assert!(matches!(scan.records[0], JournalRecord::Bounce(_)));
    }

    #[test]
    fn vacuum_never_drops_raw_bounces() {
        let (_dir, journal) = temp_journal();
        journal
            .append(&record(
                "nr-0001-1",
                Outcome::Expired,
                "2020-01-01T10:00:00+00:00",
                1000,
            ))
            .unwrap();
        let cutoff = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let report = journal.vacuum(cutoff).unwrap();
        assert_eq!(
            report.kept, 1,
            "raw retention is condense's job, not vacuum's"
        );
    }

    #[test]
    fn maintenance_on_missing_file_reports_zeros() {
        let (_dir, journal) = temp_journal();
        let cutoff = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let condense = journal.condense(cutoff).unwrap();
        assert_eq!(condense.condensed_bounces, 0);
        let vacuum = journal.vacuum(cutoff).unwrap();
        assert_eq!(vacuum.kept, 0);
        let stats = journal.stats(&StatsFilter::default()).unwrap();
        assert_eq!(stats.bounces, 0);
        assert!(stats.latency.is_none());
    }

    #[test]
    fn bounce_record_truncates_long_messages() {
        let long = "x".repeat(JOURNAL_MAX_MESSAGE_LEN + 100);
        let rec = ResolvedBounce {
            handle: "nr-0001-1",
            parent: None,
            message: &long,
            reason: reason("straightforward"),
            outcome: Outcome::Released,
            replacement: Some(&long),
            bounced_at: Timestamp::parse("2026-07-07T10:00:00+00:00").unwrap(),
            latency_ms: 1000,
        }
        .into_record();
        let JournalRecord::Bounce(b) = rec else {
            panic!("expected bounce");
        };
        assert_eq!(b.message.chars().count(), JOURNAL_MAX_MESSAGE_LEN + 1);
        assert!(b.message.ends_with('\u{2026}'));
        assert_eq!(
            b.replacement.unwrap().chars().count(),
            JOURNAL_MAX_MESSAGE_LEN + 1
        );
    }

    // ── Median ───────────────────────────────────────────────────────────

    #[test]
    fn p50_is_the_true_median_on_even_counts() {
        // Four sorted latencies: the median is the average of the two middle
        // values, not the upper one. A biased-high p50 (2000) would hide the
        // sub-second habituation signal this metric exists to surface.
        let (_dir, journal) = temp_journal();
        for (i, ms) in [500u64, 1000, 2000, 4000].into_iter().enumerate() {
            journal
                .append(&record(
                    &format!("nr-0001-{i}"),
                    Outcome::Released,
                    "2026-07-07T10:00:05+00:00",
                    ms,
                ))
                .unwrap();
        }
        let stats = journal.stats(&StatsFilter::default()).unwrap();
        let latency = stats.latency.unwrap();
        assert_eq!(
            latency.p50_ms, 1500,
            "median of [500,1000,2000,4000] is 1500"
        );
        assert_eq!(latency.min_ms, 500);
        assert_eq!(latency.max_ms, 4000);
    }

    // ── Condense chain integrity ─────────────────────────────────────────

    #[test]
    fn condense_keeps_a_parent_straddling_the_cutoff_so_no_false_dangling() {
        // A parent resolves just before the cutoff; its child just after. If
        // condense folded the parent into a summary, the raw child would read
        // as a dangling parent — a phantom corruption alarm from routine
        // maintenance. The parent must be kept raw to preserve the chain.
        let (_dir, journal) = temp_journal();
        journal
            .append(&record(
                "nr-0001-1",
                Outcome::Rephrased,
                "2026-05-31T10:00:00+00:00",
                1000,
            ))
            .unwrap();
        let mut child = record(
            "nr-0001-2",
            Outcome::Released,
            "2026-06-02T10:00:00+00:00",
            500,
        );
        if let JournalRecord::Bounce(ref mut b) = child {
            b.parent = Some("nr-0001-1".into());
        }
        journal.append(&child).unwrap();

        let cutoff = Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap();
        let report = journal.condense(cutoff).unwrap();
        assert_eq!(
            report.condensed_bounces, 0,
            "the pre-cutoff parent is referenced by a kept child, so it stays raw"
        );
        assert_eq!(report.kept_bounces, 2);

        let stats = journal.stats(&StatsFilter::default()).unwrap();
        assert_eq!(
            stats.dangling_parents, 0,
            "condense must not manufacture a dangling-parent alarm"
        );
    }

    #[test]
    fn condense_transitively_keeps_a_whole_straddling_chain() {
        // grandparent → parent → child, only the child is past the cutoff.
        // Keeping the child pulls in the parent, which pulls in the grandparent.
        let (_dir, journal) = temp_journal();
        journal
            .append(&record(
                "nr-0001-1",
                Outcome::Rephrased,
                "2026-01-01T10:00:00+00:00",
                1000,
            ))
            .unwrap();
        let mut parent = record(
            "nr-0001-2",
            Outcome::Rephrased,
            "2026-01-02T10:00:00+00:00",
            1000,
        );
        if let JournalRecord::Bounce(ref mut b) = parent {
            b.parent = Some("nr-0001-1".into());
        }
        journal.append(&parent).unwrap();
        let mut child = record(
            "nr-0001-3",
            Outcome::Released,
            "2026-07-07T10:00:00+00:00",
            500,
        );
        if let JournalRecord::Bounce(ref mut b) = child {
            b.parent = Some("nr-0001-2".into());
        }
        journal.append(&child).unwrap();

        let cutoff = Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap();
        let report = journal.condense(cutoff).unwrap();
        assert_eq!(
            report.condensed_bounces, 0,
            "the whole live chain is preserved"
        );
        assert_eq!(report.kept_bounces, 3);
        assert_eq!(
            journal
                .stats(&StatsFilter::default())
                .unwrap()
                .dangling_parents,
            0
        );
    }

    // ── Unknown-schema tolerance ─────────────────────────────────────────

    #[test]
    fn unknown_kind_is_preserved_by_condense_and_vacuum() {
        // A record type from a newer schema (valid JSON, unrecognized `kind`)
        // must survive maintenance verbatim — vacuum is not licensed to delete
        // a well-formed record just because this binary predates its schema.
        // Genuine non-JSON garbage is still dropped by vacuum.
        let (_dir, journal) = temp_journal();
        journal
            .append(&record(
                "nr-0001-1",
                Outcome::Released,
                "2020-01-01T10:00:00+00:00",
                1000,
            ))
            .unwrap();
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(journal.path())
            .unwrap();
        file.write_all(b"{\"kind\":\"future_thing\",\"data\":42}\n")
            .unwrap();
        file.write_all(b"not json at all\n").unwrap();
        drop(file);

        let scan = journal.load().unwrap();
        assert_eq!(
            scan.unknown,
            vec!["{\"kind\":\"future_thing\",\"data\":42}".to_string()]
        );
        assert_eq!(scan.malformed, vec!["not json at all".to_string()]);

        let stats = journal.stats(&StatsFilter::default()).unwrap();
        assert_eq!(stats.unknown_lines, 1);
        assert_eq!(stats.malformed_lines, 1);

        // Condense preserves both.
        let cutoff = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        journal.condense(cutoff).unwrap();
        let scan = journal.load().unwrap();
        assert_eq!(scan.unknown.len(), 1, "condense keeps the future record");
        assert_eq!(scan.malformed.len(), 1, "condense keeps non-JSON too");

        // Vacuum drops the non-JSON garbage but keeps the future record.
        let report = journal.vacuum(cutoff).unwrap();
        assert_eq!(report.dropped_malformed, 1);
        let scan = journal.load().unwrap();
        assert_eq!(
            scan.unknown,
            vec!["{\"kind\":\"future_thing\",\"data\":42}".to_string()],
            "vacuum must never delete a valid newer-schema record"
        );
        assert!(scan.malformed.is_empty(), "vacuum drops genuine garbage");
    }

    // ── Single-writer discipline (the P1 race) ───────────────────────────

    /// Regression for the condense/append race: with the single-writer actor,
    /// appends and the condense rewrite are totally ordered on one task, so an
    /// append can never land between condense's load and rename and be lost.
    /// Hammer both concurrently through the handle and assert every appended
    /// record survives.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_appends_and_condense_lose_no_records() {
        use std::sync::Arc;

        let dir = TempDir::new().unwrap();
        let path = Utf8PathBuf::try_from(dir.path().to_path_buf()).unwrap();
        let handle = Arc::new(JournalHandle::spawn(&path));

        // All appends are recent (after the cutoff), so every one must survive
        // as a kept raw bounce no matter how the condense rewrites interleave.
        const N: usize = 200;
        let appender = {
            let handle = handle.clone();
            tokio::spawn(async move {
                for i in 0..N {
                    handle.append(&record(
                        &format!("nr-0001-{i}"),
                        Outcome::Released,
                        "2026-07-07T10:00:05+00:00",
                        i as u64,
                    ));
                    if i % 8 == 0 {
                        tokio::task::yield_now().await;
                    }
                }
            })
        };
        let condenser = {
            let handle = handle.clone();
            tokio::spawn(async move {
                let cutoff = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
                for _ in 0..30 {
                    handle.condense(cutoff).await.unwrap();
                    tokio::task::yield_now().await;
                }
            })
        };
        appender.await.unwrap();
        condenser.await.unwrap();
        handle.flush().await;

        let scan = handle.load().unwrap();
        let handles: HashSet<&str> = scan
            .records
            .iter()
            .filter_map(|r| match r {
                JournalRecord::Bounce(b) => Some(b.handle.as_str()),
                JournalRecord::Summary(_) => None,
            })
            .collect();
        assert_eq!(
            handles.len(),
            N,
            "every appended record must survive concurrent condensing — none clobbered by a rename"
        );
        assert!(scan.malformed.is_empty(), "no torn writes");
    }
}
