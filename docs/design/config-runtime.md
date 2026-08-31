# Config Runtime — Contract, Recovery Design, Current State

Status: contract accepted house-wide 2026-08-27; this repo is the first
implementation, and the recovery machinery is now IMPLEMENTED in
`src/config.rs` (LKG, bounded quarantine, restore, single-writer pipeline —
see "Implementation notes"). This document records the target contract, the
accepted recovery design, the implementation notes, and the open decisions.
The pre-implementation survey of the config machinery is retained below as
an explicitly historical record (see "Historical: pre-implementation
survey").

## Target contract

- **One discoverable config.** A single `config.toml` (default
  `$DIONE_STATE_DIR/config.toml`, overridable via `--config` /
  `set_config_path`).
- **Typed config, shared via ArcSwap.** Readers take `Arc<LoadedConfig>`
  snapshots; no reader ever sees a partially applied config.
- **One canonical pipeline.** Every producer — startup, file watcher,
  explicit reload, MCP mutation, recovery — goes through the same
  validate → persist → publish pipeline. No caller shortcuts.
- **Tool success = durable AND live.** A config-mutating tool call succeeds
  only when the change is durably written to disk and live in the published
  snapshot, at the same generation.
- **Serialized tool mutations over the latest document.** Tool mutations are
  serialized; each operates on the latest on-disk document, later-wins per
  field. Explicitly decided: a concurrent human file edit **loses** to a
  serialized tool mutation. This is a chosen semantic, not an accident — the
  serialized mutation stream is authoritative during its window.
- **Single writer, no broad locks (🦋, 2026-08-28).** Config has exactly one
  serialization authority: one writer owns validate → persist → publish, and
  every producer hands it requests. Serialization falls out of the
  architecture — lock-free by construction; no file locks, no cross-process
  machinery, and no caller-grown locks. Any path that can write around the
  authority is the defect. Acceptance test: concurrent public mutations
  serialize through the one writer and cannot lose updates.
- **Exactly one persistent last-known-good (LKG).** Non-consumed: restoring
  from it does not delete or invalidate it.
- **Immediate quarantine + restore on invalid edits.** An invalid on-disk
  config is quarantined at once and the canonical path restored from LKG;
  the seat never runs indefinitely against a bad file.
- **Monotonic generations.** Every live publication carries a generation
  strictly greater than the previous one.

## Recovery design (accepted 2026-08-27, 🦋)

Syscall-level mechanism, recorded verbatim:

- **Recovery path:** write `good.tmp` → `fsync(good.tmp)` →
  `linkat(config.toml, unique_bad_name)` → `renameat(good.tmp, config.toml)`
  → `fsync(parent dir)`.
- **Success path** maintains the single LKG the same way:
  `linkat(config.toml, lkg.tmp)` → `renameat(lkg.tmp, config.toml.lkg)`
  after a config validates and publishes. "Exactly one LKG" falls out of
  `renameat` replacing rather than accumulating.
  *(Note: the `linkat` wording above was superseded for the LKG by the
  owner's 2026-08-28 byte-copy decision — see the finding resolution under
  Implementation notes; quarantine keeps `linkat`.)*
- **`linkat` ENOENT** (no `config.toml` — first run or deleted) means
  nothing to quarantine: skip the link, proceed with the rename. Do **not**
  abort seat startup.
- **Crash-state analysis:** before `linkat`, the old canonical exists; after
  `linkat`, both names share one inode; after `renameat`, good canonical +
  quarantined old inode; after the directory fsync, both are durable. Every
  boundary retains a usable config.
- **The security property is structural, not procedural:** quarantine via
  `linkat` shares the *same inode*, so file mode and owner are shared, never
  copied. There is no operation in the sequence that can produce a
  weaker-permissioned second artifact.

### Implementation notes (commit 5)

Implemented in `src/config.rs` (`promote_lkg_from`, `quarantine_canonical`,
`restore_from_lkg`, `persist_canonical`; wired through
`ConfigRuntime::{reload, startup_load, mutate}`).

- Rust's `std::fs::hard_link` and `std::fs::rename` ARE `linkat` /
  `renameat` on Linux, so std is faithful to the accepted mechanism — no
  extra crate.
- Secret-bearing staging files are opened with exclusive creation
  (`O_CREAT | O_EXCL`) and their final protected mode is installed through
  the opened handle before any bytes are written. A pre-created path,
  including a symlink, therefore fails closed rather than being followed or
  truncated.
- fsync placements: LKG promotion fsyncs its byte-copy temp before the
  rename exposes it; `good.tmp` is fsynced before any rename exposes it
  (recovery); the mutation path fsyncs its `mut.tmp` unique temp sibling
  before renaming it over the canonical; the parent directory is fsynced
  after every completed link/rename sequence. Every fresh temp that will be
  renamed over the canonical or the LKG carries the source file's owner mode
  with group/other access stripped (0600 when no canonical exists) before
  any bytes are written and before the rename exposes it.
- **RESOLVED 2026-08-28: owner chose byte-copy LKG (robustness);
  quarantine remains same-inode.** `promote_lkg_from` receives the bytes and
  permission mode captured from the same opened canonical handle during
  validation, writes them to an exclusively created temp sibling, fsyncs the
  temp, renames it over `config.toml.lkg`, and fsyncs the parent directory.
  It never reopens the canonical pathname after validation. Chosen after the in-place edit hazard
  finding below: a torn or in-place-rewritten canonical can no longer
  corrupt the LKG through a shared inode.
- **DELIBERATE DEVIATION — review point.** The accepted thread said
  `unique_bad_name` for the quarantine. The implementation instead uses a
  BOUNDED quarantine: hard-link the bad canonical to a unique temp, then
  rename it over a single `config.toml.bad` (atomic replace). Rationale: the
  quarantine artifact is secret-bearing (it carries the token — see the
  threat model) and retention of quarantine links is the surviving concern,
  so bounded-by-construction (at most one quarantine artifact ever) was
  chosen over unique-accumulating. Same-inode linking is preserved, so
  permissions remain shared, never copied. If review prefers
  unique-accumulating names plus a retention policy, the change is local to
  `quarantine_canonical`.
- **In-place edit hazard — FINDING for the owner (🦋), discovered while
  testing commit 5.** The same-inode LKG shares the canonical's inode, so
  any *in-place* write to `config.toml` (open + truncate + write, e.g.
  `std::fs::write`, `echo >`, or an editor configured to save in place)
  rewrites the LKG through the shared inode at the same moment it corrupts
  the canonical — leaving nothing to restore from. The mechanism is safe
  exactly for replace-by-rename saves (atomic editors, our own pipeline).
  The commit-5 tests model editor saves as rename-replace for this reason,
  and `write_default_config` was changed to replace by rename so our own
  template regeneration never writes through a quarantine/LKG-shared inode.
  The alternative — LKG as a byte copy (tmp + fsync + rename, with the
  canonical's permissions copied explicitly) — survives in-place edits but
  gives up the structural same-inode permission guarantee for the LKG. This
  is a genuine fork in the accepted design and is left to the owner; the
  change would be local to `promote_lkg_from`.
  **RESOLVED 2026-08-28: owner chose byte-copy LKG (robustness); quarantine
  remains same-inode.** (🦋, after reviewing this finding — robustness over
  same-inode elegance for the LKG; the change was local to `promote_lkg_from` as
  predicted.)

### Implementation notes (post-review fixes, 2026-08-28)

- **Every canonical writer and publisher runs under the single writer.**
  The adversarial review found `ConfigRuntime::reload` (and its
  quarantine/restore path) writing `config.toml` and publishing to the
  ArcSwap without the writer lock only `mutate` took — allowing a racing
  restore to quarantine a freshly-acked mutation, a reload to republish
  stale content over a fresh mutation, and generation inversion between
  racing publishes. `reload` and `startup_load` now acquire the same
  `CONFIG_WRITER` mutex as `mutate`, before any disk read and held across
  the publish; the reload entry is async and runs its file I/O on the
  blocking pool under the lock (startup takes the lock trivially —
  uniformity over special-casing). Guarded by the looped
  `racing_mutations_serialize_and_both_land` stress, the mid-mutation
  corruption race test, and the generation-monotonicity observer test in
  `src/config.rs`.
- **Crash hygiene (P2 leak + P3s).** `startup_load` sweeps leftover
  `config.toml.<tag>.<pid>.<seq>` temp siblings from the `unique_sibling`
  naming family (`.bad.tmp.*`, `.lkg.tmp.*`, `.good.tmp.*`,
  `.template.tmp.*`, `.mut.tmp.*`) plus the legacy fixed mutation temp
  name `config.toml.tmp` — a crash between link/copy and rename leaks
  them, and all but the template temp can carry the token; a warn logs the
  swept count.
  `write_default_config` fsyncs its temp before the rename, matching every
  other canonical writer. The `reload_config` MCP tool rides the async
  reload entry (blocking-pool file I/O) instead of blocking the async
  runtime. The startup-error surface
  (`StartupConfigError::InvalidConfigNoLkg.parse_error`) and the recovery
  note store a sanitized one-line message — error location, no
  source-line snippet.

### Implementation notes (post-review fixes, 2026-08-30)

- **Restrictive permissions on every fresh canonical/LKG artifact.**
  `persist_canonical` carries the canonical's owner mode onto its temp
  (0600 when no canonical exists), `restore_from_lkg` carries the LKG owner
  mode (read from the same opened handle as the bytes) onto the restored
  canonical, and LKG promotion carries the captured canonical owner mode.
  Group/other bits are stripped, so neither a permissive source nor the
  process umask can mint a second readable token-bearing artifact.
- **LKG promotion uses the validated bytes.** The pipeline captures a
  `CanonicalSnapshot` (contents + permissions, from ONE opened handle) at
  parse time and promotes exactly those bytes; an external rename/write in
  the parse→promote gap can no longer make live config A while the LKG
  captures unvalidated B. The mutation path promotes the serialized document
  it persisted, same principle.
- **Mutation temp joined the owned sweep family.** The mutation persist
  writes a `mut.tmp` unique sibling (swept at startup after a crash) instead
  of the fixed `config.toml.tmp`; the legacy fixed name is also swept.
- **Default-config persistence surfaces failures truthfully.**
  `write_default_config` returns pre-rename failures and represents a
  post-rename parent-fsync failure as applied with unknown durability. Every
  caller (startup missing-file, reload missing-file, `RegenerateDefaults`)
  reports the matching state. The `RegenerateDefaults` warning also uses the
  sanitized one-line parse error (no source-line snippet).
- **Applied and durable are separate facts.** If a canonical rename succeeds
  but its parent-directory fsync fails, mutation, recovery, and default
  generation all proceed from the rename-applied bytes and surface unknown
  durability. Mutation publishes the validated bytes and returns
  `durability = unknown`; recovery reloads and publishes the restored
  canonical. No path reports non-application while leaving changed disk
  policy and a stale live snapshot.
- **Cancellation cannot release the writer around a non-cancellable effect.**
  A spawned owner task holds the process-local writer across the complete
  blocking load → validate → persist → publish pipeline. Dropping the caller
  only detaches that owner; a successor cannot overlap its in-flight write.

## Threat-model notes

- **The quarantine artifact is secret-bearing.** `config.toml` carries the
  Discord bot token. The `linkat` design means no second artifact exists —
  but the `.bad` name keeps the inode (and the token) alive exactly as long
  as that link exists. Retention of quarantine links is the surviving
  concern.
- **Parse-error strings can embed config source lines.** `toml::de::Error`'s
  display quotes the errored source line, which for `config.toml` can carry
  the token. The startup-error surface and the recovery note are sanitized
  locally (one line, no snippet); the pre-existing parse-error-snippet class
  everywhere else is owned by #371 (central redaction), not patched
  piecemeal here.
- **Regenerate-defaults cannot regenerate the token.** The failure mode it
  produces is a mute-but-running seat: looks alive, hears nothing.
- **Watcher self-healing masks the sidecar dual-publish bug.** The file
  watcher re-merges sidecar entries within seconds of any write, so naive
  regression tests go green regardless. Regression tests must assert on the
  *immediate* post-mutation snapshot without running `notify`.

## Historical: pre-implementation survey (2026-08-28)

**This section is a historical record of the state BEFORE the config-runtime
implementation landed.** The dual-pipeline asymmetry described in item 2 no
longer exists: every producer now runs through the single-writer
`ConfigRuntime` pipeline, and `ConfigStore` can no longer persist or publish
on its own. The "Additional current-state facts" below were updated during
implementation and remain accurate.

At survey time, two pipelines published to the `LAST_VALID_CONFIG` ArcSwap
(`src/config.rs`):

1. **`reload_config`** (`src/config.rs`) — used by startup
   (`src/main.rs`), the file watcher (`src/config_watcher.rs`), and the
   `reload_config` MCP tool (`src/mcp/dispatch.rs`). Reads `config.toml`,
   merges contradictionary sidecar entries via `load_sidecar_entries`,
   builds `LoadedConfig`, publishes.
2. **`ConfigStore::save`** (`src/config_store.rs`) — used by every MCP
   config mutation (`src/mcp/dispatch.rs`, `src/mcp/tools/access.rs`).
   Round-trip validates, writes `config.toml.tmp`, renames, then publishes
   directly via `store_loaded_config` **without** merging the sidecar. Every
   MCP config write therefore republishes a snapshot missing the sidecar
   contradictionary entries (~70 in production) until the watcher's reload
   re-merges them. This asymmetry is a known bug the canonical pipeline
   eliminates.

Additional current-state facts:

- Fsync now exists on these paths (commit 5): the mutation persist
  (tmp fsync + parent-dir fsync), LKG promotion, and recovery (`good.tmp`
  fsync + parent-dir fsync). Reads of the canonical outside these paths
  remain unchanged.
- No cross-process write lock; in-process tool mutations, reloads, and
  startup are all serialized by the runtime's single writer (see the
  post-review fixes above).
- On-disk LKG (`config.toml.lkg`), bounded quarantine (`config.toml.bad`),
  and restore now exist (commit 5). Parse error on reload with an LKG
  present quarantines the bad file and restores + publishes the LKG
  (`repeated_bad_edits_stay_bounded_and_canonical_stays_valid` in
  `src/config.rs`); without an LKG the pre-existing behavior remains — last
  valid *in-memory* config, file left untouched
  (`test_corrupt_config_without_lkg_keeps_file_and_falls_back`). Defaults
  are used only for a missing file (template is written) or an I/O error.
- Generations are process-monotonic (`AtomicU64`, in-memory) and reset on
  restart.

## Open decisions (UNRESOLVED, owner 🦋)

1. **Bad main config at startup with no LKG:** typed startup failure vs
   regenerate defaults. (Regenerate-defaults produces the mute-but-running
   seat described above.) **Provisional default shipped (commit 5):**
   `NoLkgPolicy::FailStartup` — a typed `StartupConfigError` — isolated in
   the `NoLkgPolicy` enum (`src/config.rs`). Still UNRESOLVED; the owner's
   answer flips exactly one enum variant at the `startup_load` call site in
   `src/main.rs` (`RegenerateDefaults` quarantines the bad file first, then
   writes the template). A missing file entirely is not this decision — it
   keeps the existing first-boot template path.
2. **Legacy contradictionary conflict identity — RESOLVED (🦋, 2026-08-29,
   goddess-infra thread, "do the pair"):** an entry is identified by
   `(pattern, match_mode)`. The same text under different matching semantics
   is two distinct rules and both survive; when an inline entry and a sidecar
   entry share the full identity, the sidecar copy supersedes the inline one
   (the sidecar is the preferred store). Implemented in `compose_candidate`
   (`src/config_candidate.rs`); within-source duplicates are deliberately out
   of scope — the ruling was about the inline↔sidecar boundary only.

## Planned deviation

This slice ships success semantics (durable file + live publish) **without**
the audit-append/config-commit shared success boundary from the full
contract. Audit is deliberately deferred — NOT NOW — and retrofitting it
later changes mutation semantics: a planned second crossing, not scope
creep. Related: generation persistence across restart (in-memory monotonic
that resets on boot vs stored-with-LKG) is currently UNSPECIFIED and must be
settled before the audit retrofit.
