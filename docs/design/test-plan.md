<!-- design-meta
status: draft
last-updated: 2026-05-17
phase: 5
-->

# Test Plan

Derived from the SRTM in [requirements.md](requirements.md) and the
lightweight threat model in [threat-model.md](threat-model.md).

## Test Strategy

- **Unit tests** for pure logic: gate decisions, chunking, config parsing,
  queue management, filename sanitization, mention detection.
- **Integration tests** using mock Discord HTTP + mock MCP transport to verify
  end-to-end flows without a real Discord connection.
- **No live Discord tests in CI** — requires a bot token and real gateway.
  Manual testing against a dev bot covers these paths.

## Test Cases

### Access Control (gate.rs)

| ID | Requirement | Type | Description | Automated |
|----|-------------|------|-------------|-----------|
| TC-01 | R-03 | Unit | DM from allowlisted user → Deliver | Yes |
| TC-02 | R-03 | Unit | DM from unknown user (policy=queue) → Queue | Yes |
| TC-03 | R-03 | Unit | DM when policy=disabled → Drop (even if allowlisted) | Yes |
| TC-04 | R-04 | Unit | Message in opted-in channel with @mention → Deliver | Yes |
| TC-05 | R-04 | Unit | Message in non-opted channel → Drop | Yes |
| TC-06 | R-04 | Unit | Message without mention in require_mention channel → Drop | Yes |
| TC-06a | R-04 | Unit | Message in channel with per-channel allow_from, sender not in list → Drop | Yes |
| TC-10 | R-13 | Unit | Outbound to allowlisted DM channel → Allow | Yes |
| TC-11 | R-13 | Unit | Outbound to non-opted guild channel → Reject | Yes |
| TC-11a | R-38 | Unit | Outbound gate reads the current published snapshot rather than retaining an inbound snapshot | Yes |

### Mention Detection (gate.rs)

| ID | Requirement | Type | Description | Automated |
|----|-------------|------|-------------|-----------|
| TC-30 | R-05 | Unit | @mention detected via mentions set | Yes |
| TC-31 | R-05 | Unit | Reply-to-bot detected via recent sent IDs | Yes |
| TC-32 | R-05 | Unit | Regex pattern match triggers mention | Yes |
| TC-33 | R-05 | Unit | Invalid regex in config does not crash (skip + log) | Yes |

### Access Request Queue (queue.rs)

| ID | Requirement | Type | Description | Automated |
|----|-------------|------|-------------|-----------|
| TC-07 | R-08d | Unit | Request expires after configured timeout | Yes |
| TC-08 | R-08 | Unit | Queue cap (50) enforced; excess dropped silently | Yes |
| TC-09 | R-08a | Unit | Admin notification respects rate limit (cooldown) | Yes |
| TC-34 | R-36 | Unit | Duplicate user_id replaces existing entry (dedup) | Yes |
| TC-35 | R-37 | Unit | Message preview truncated to 100 chars | Yes |
| TC-36 | R-40 | Unit | Queue persists via atomic write (tmp + rename) | Yes |
| TC-37 | R-08b | Integration | list_access_requests tool returns pending entries | Yes |
| TC-38 | R-08c | Integration | approve_access adds user to allow_from | Yes |
| TC-39 | R-08c | Integration | deny_access removes from queue without adding | Yes |

### Permission Relay (permissions.rs)

| ID | Requirement | Type | Description | Automated |
|----|-------------|------|-------------|-----------|
| TC-12 | R-14 | Unit | Permission request sent to admin user(s) | Yes |
| TC-13 | R-14 | Unit | Permission request NOT sent to non-admin allowlisted user | Yes |
| TC-14 | R-15 | Unit | Button click from admin → accepted (forwarded to MCP) | Yes |
| TC-15 | R-15 | Unit | Button click from non-admin → rejected (ephemeral error) | Yes |
| TC-40 | R-39 | Unit | Stale permissions pruned after 5-minute sweep | Yes |

### File Handling

| ID | Requirement | Type | Description | Automated |
|----|-------------|------|-------------|-----------|
| TC-16 | R-30 | Unit | Attach file from inbox/ → allowed | Yes |
| TC-17 | R-30 | Unit | Attach file from state dir (non-inbox) → rejected | Yes |
| TC-18 | R-30 | Unit | Attach file outside state dir → allowed | Yes |
| TC-41 | R-41 | Unit | Symlink into state dir → rejected after canonicalize | Yes |
| TC-19 | R-31 | Unit | Filename with `[\r\n;[]` chars → sanitized | Yes |

### Message Chunking (discord/chunker.rs)

| ID | Requirement | Type | Description | Automated |
|----|-------------|------|-------------|-----------|
| TC-50 | R-11 | Unit | Text under limit → single chunk returned | Yes |
| TC-51 | R-11 | Unit | Text over limit → split at ceil_char_boundary | Yes |
| TC-52 | R-11 | Unit | Paragraph mode prefers double-newline split | Yes |
| TC-53 | R-11 | Unit | No valid split point → hard cut at limit | Yes |
| TC-54 | R-11 | Unit | Multi-byte characters never split mid-codepoint | Yes |
| TC-55 | R-12 | Unit | reply_to_mode=first threads only first chunk | Yes |
| TC-56 | R-12 | Unit | reply_to_mode=all threads every chunk | Yes |
| TC-57 | R-12 | Unit | reply_to_mode=off never threads | Yes |

### Config (config.rs)

| ID | Requirement | Type | Description | Automated |
|----|-------------|------|-------------|-----------|
| TC-23 | NF-04 | Unit | Corrupt TOML → file left untouched, last valid config retained | Yes |
| TC-24 | NF-04 | Unit | Missing config file → defaults (queue policy, empty lists) | Yes |
| TC-60 | R-16 | Unit | File-watcher/explicit reload publishes updated values; gate reads observe the new snapshot without per-call disk I/O | Yes |
| TC-61 | Config | Unit | Env var DISCORD_BOT_TOKEN overrides config file token | Yes |
| TC-62 | Config | Unit | Empty allow_from + admins → functional (everything gated) | Yes |

### Security Event Logging (tracing)

| ID | Requirement | Type | Description | Automated |
|----|-------------|------|-------------|-----------|
| TC-20 | SC-07 | Integration | Gate denial emits tracing event at warn level | Yes |
| TC-21 | SC-07 | Integration | Access request queued emits info-level event | Yes |
| TC-22 | SC-07 | Integration | Permission decision (allow/deny) emits info-level event | Yes |

### Reaction Notifications

| ID | Requirement | Type | Description | Automated |
|----|-------------|------|-------------|-----------|
| TC-70 | R-32 | Integration | Reaction on bot message → notification emitted | Yes |
| TC-71 | R-32 | Integration | Reaction on non-bot message → no notification | Yes |

### Voice Messages

| ID | Requirement | Type | Description | Automated |
|----|-------------|------|-------------|-----------|
| TC-75 | R-34 | Unit | Inbound attachment with voice flag → voice metadata in notification | Yes |
| TC-76 | R-33 | Integration | Reply with audio file + voice flag → IS_VOICE_MESSAGE set | Yes |

### Shutdown

| ID | Requirement | Type | Description | Automated |
|----|-------------|------|-------------|-----------|
| TC-80 | R-29 | Integration | stdin EOF → clean shutdown within 2s | Yes |
| TC-81 | R-29 | Integration | SIGTERM → clean shutdown within 2s | Yes |

## Coverage Summary

| Category | Test Count | Automated |
|----------|-----------|-----------|
| Access control (gate) | 11 | All |
| Mention detection | 4 | All |
| Access queue | 9 | All |
| Permission relay | 5 | All |
| File handling | 5 | All |
| Message chunking | 8 | All |
| Config | 5 | All |
| Security logging | 3 | All |
| Reactions | 2 | All |
| Voice messages | 2 | All |
| Shutdown | 2 | All |
| **Total** | **56** | **56** |

## Test Infrastructure

- Unit tests: standard `#[cfg(test)]` modules within each source file
- Integration tests: `tests/` directory with mock transports
- Mock Discord HTTP: record/replay or trait-based mock of `serenity::Http`
- Mock MCP transport: in-memory channel pair simulating stdio
- Snapshot testing via `insta` for notification payloads and tool responses
- Parameterized tests via `test-case` for gate decision matrices

## Public package privacy

The release workflow runs `scripts/verify-public-package-privacy.sh` against the
exact `.crate` archive produced by every `cargo package` invocation for both
`auspex-core` and `dione`. Integration fixtures independently prove that each
structural private-dependency class is rejected, that binary payloads are
skipped, and that the public historical Cingulate name remains allowed. A
separate synthetic fixture proves that an external forbidden marker is
rejected without printing the marker. Real values are never checked into the
repository; CI can require a non-empty House-managed
marker file when that overlay is provisioned. Two explicitly consented public
name canaries are also rejected as whole tokens, case-insensitively; their
literal spellings do not occur in the checked-in package inputs. Tree and
archive checks consume the same structural-rule table. Archive traversal
preserves newline-bearing member names, scans normalized names as well as
non-binary contents, reports only redacted rule IDs, and fails closed on
archive, traversal, read, or matcher errors.
## GAIE archive Atom 1b acceptance tests

| Test | Requirement | Independent oracle |
|---|---|---|
| Default root captures parent plus verified active/archived threads | GAIE-1B-R1–R5 | Scripted route transcript and child-fetch call log |
| Wrong-parent candidate never reaches message endpoint | GAIE-1B-R6 | Empty child-fetch call log |
| Exact v1 checkpoint migrates to exact v2 parent-only shape | GAIE-1B-R8–R9 | Literal input and expected JSON fixtures |
| Stream cursors resume independently after commit-before-checkpoint fault | GAIE-1B-R10 | Committed-event fixture plus call log and pure stream-map model |
| Discovery permutations produce identical plan and output | GAIE-1B-R4 | Numeric `BTreeSet` target model and pre/post archive bytes |
| A later run discovers a new thread without replaying old streams | GAIE-1B-R10 | Literal prior checkpoint and per-stream request cursors |
| Parent/thread starter alias deduplicates globally and retains relation | GAIE-1B-R7 | Literal duplicate payloads and relation assertions |
| Identical rerun appends nothing and does not churn checkpoint | GAIE-1B-R10 | Byte-for-byte archive and checkpoint snapshots |

The first phase installs these as executable red contracts. Production
discovery, verified targets, stream checkpointing, and migration are implemented
only after the coordinator observes the intended failures.
