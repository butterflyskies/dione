<!-- design-meta
status: approved
last-updated: 2026-05-17
phase: 4
depth: lightweight
-->

# Threat Model (Lightweight)

Lightweight review focused on attack surfaces that are new or different from
the reference TypeScript Discord plugin implementation. The TypeScript plugin
has battle-tested most of the security properties (file send guard, outbound
gate, filename sanitization, admin-only permission responses). This review
focuses on the delta.

## Trust Boundaries

| Boundary | From | To | Data |
|----------|------|----|------|
| TB1→TB2 | Discord Gateway | Dione | User messages, reactions, button interactions |
| TB3→TB2 | Claude Code | Dione | Tool calls (chat_id, text, file paths) |
| TB2→TB4 | Dione | Discord REST | Outbound messages, reactions, uploads |
| TB5↔TB2 | Filesystem | Dione | Config reads, queue persistence, attachments |
| TB1→TB5 | Discord CDN | Filesystem | Downloaded attachment files |

## Findings

### 1. Access Request Queue Flood (NEW)

**Surface:** Unknown senders can DM the bot, filling the 50-slot queue.

**Impact:** Low. Admin gets rate-limited notifications (annoyance, not compromise).

**Mitigations:**
- One slot per user_id — single attacker can't fill the queue (R-36)
- Queue cap is hard (50) — excess silently dropped
- No feedback to attacker on drop (no information leakage)
- Message preview truncated to 100 chars (R-37) — bounds memory usage

### 2. Queue Content as Prompt Injection Vector (NEW)

**Surface:** Message previews in queue.json are untrusted content that will
be shown to Claude via the `list_access_requests` tool.

**Impact:** Low. Claude Code already handles untrusted content in its context
(every Discord message is untrusted). The preview is no different from
`fetch_messages` output.

**Mitigations:**
- Truncation to 100 chars limits injection payload size (R-37)
- Tool response should frame content as untrusted (same as fetch_messages)
- No code execution path from queue content

### 3. Symlink Traversal in File Send Guard (PORTED)

**Surface:** `reply` tool accepts file paths for attachment. Symlinks could
bypass the state-directory exclusion check.

**Impact:** Medium. Could exfiltrate config.toml (contains bot token) or
queue.json via Discord upload.

**Mitigations:**
- Canonicalize (resolve symlinks) before checking against state dir (R-41)
- Ported directly from TypeScript plugin's `realpathSync` pattern
- inbox/ is the only state-dir subdirectory allowed for sends

### 4. Config Hot-Reload Race (DELTA)

**Surface:** Config read on inbound (gate check) may differ from config at
outbound (tool call) if the file was modified between those points.

**Impact:** Low. Could allow a send to a channel that was just removed from
config, or deny a send to a newly-added channel.

**Mitigations:**
- Re-read config at outbound gate (R-38) — same pattern as TypeScript
- No caching between inbound and outbound

### 5. Stale Permission Buttons (PORTED)

**Surface:** Discord button messages persist indefinitely. Admin could click
a button for a long-expired permission request.

**Impact:** Low. Claude Code times out on its side. Stale responses are no-ops.

**Mitigations:**
- Periodic sweep of pending permissions map (R-39, 5-minute interval)
- Unknown request_id responses are silently ignored

### 6. Queue Persistence TOCTOU (NEW)

**Surface:** queue.json written on mutation, read on startup. External
modification could corrupt state.

**Impact:** Low. Queue is a convenience (survives restarts), not a security
boundary. In-memory state is authoritative at runtime.

**Mitigations:**
- Atomic writes via tmp + rename (R-40)
- In-memory state is authoritative; file is crash recovery
- Corrupt file → log + start with empty queue (same as config error handling)

## Ported Security Properties (no change needed)

These are carried directly from the TypeScript plugin:

| Property | Implementation |
|----------|---------------|
| Outbound gate mirrors inbound | Re-check allow_from / channels on every send (R-13) |
| Filename sanitization | Strip `[\[\]\r\n;]` from attachment names (R-31) |
| File send guard | Canonicalize + reject state-dir paths (R-30, R-41) |
| Permission responses from admins only | Check interaction.user.id ∈ admins (R-15) |
| Bot message filtering | Ignore messages from bots (prevent loops) |
| Attachment size cap | 25MB per file, 10 files max (R-12) |

## Architectural Changes

None required. The existing architecture already accounts for these threats.
The new requirements (R-36 through R-41) are implementation-level controls
that fit within the current module structure.
## GAIE Atom 1b threat additions

| Threat | Boundary | Mitigation |
|---|---|---|
| A Discord response injects an unrelated child channel | Discovery to message fetch | Validate guild, exact parent, and admitted thread type; construct a verified target; reject before child fetch |
| Missing permissions are mistaken for complete history | Coverage claim | Describe coverage as principal-visible; default enumeration failure is closed; private-all 403 uses the explicit joined-private fallback |
| Threads appear or archive during discovery | Discord's non-atomic API | Union active snapshot A, archived pages, and active snapshot B; never claim an atomic/global snapshot |
| Route pagination silently truncates on a short page | Archived-thread enumeration | Continue only according to `has_more`; use the route's specified cursor type |
| Checkpoint from another root redirects or suppresses capture | Durable resume boundary | Bind v2 to corpus, guild, and parent; reject foreign, mixed, unknown, or corrupt forms |
| Commit succeeds but checkpoint write fails | Archive/checkpoint ordering | Commit and fsync first; on retry deduplicate by global message ID and repair only the stream cursor |
| Parent/thread starter alias creates duplicate history | Cross-stream identity | Keep Discord-global message identity while retaining the embedded thread relationship |
| Partial mode is mistaken for complete capture | Operator contract | Keep `allow_partial` explicit and parent-only; do not invent durable partial-thread semantics |
