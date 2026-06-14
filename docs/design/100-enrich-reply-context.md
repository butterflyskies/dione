# Plan: Enrich inbound reply context (author + content preview)

Issue: #100 — enrich the inbound notification payload with reply author and a
content preview. Builds directly on #99 (reply threading inbound).

## Goal

When a Discord message is a reply, the inbound notification already carries
`reply_to_message_id` (from #99). Add three more fields so the MCP client
reconstructs the same threading context a human sees in the Discord UI — *which*
message, *who* sent it, and a *preview* of what it said — with **zero additional
API calls**:

| Wire field (in `params.meta`) | Source | Type on wire |
|---|---|---|
| `reply_to_user_id` | `referenced_message.author.id` | string (decimal snowflake) |
| `reply_to_user` | `referenced_message.author.name` | string |
| `reply_to_content_preview` | first ~100 chars of `referenced_message.content` | string |

All three are conditionally serialized — omitted entirely when the message is
not a reply or the data is unavailable (same pattern as `thread_parent_id` /
`reply_to_message_id`).

## Prerequisite / base branch (must resolve first)

#99 was implemented by **PR #102** (`feat/99-reply-threading-inbound` → `main`),
which is **merged** into `upstream/main` (merge commit `75d74ce`, now the top of
`upstream/main`). So the `reply_to_message_id` / `reply_to_id` foundation is on
`main`.

Current state to reconcile:

- `upstream/main` **has** #102/#99 (`reply_to_id`, `reply_to_message_id` on
  `Message` + `MessageEdit`, tests, snapshots).
- `origin/main` (the paceheart fork) has **not** synced #102 yet — it still tops
  out at #103.
- The working branch `feat/100-enrich-reply` is based on the pre-#102 tree, so it
  does **not** yet have the #99 code this plan depends on.

**Action:** rebase `feat/100-enrich-reply` onto the updated `upstream/main`
(and/or sync `origin/main` from `upstream/main` first). All line numbers below
refer to the post-#102 tree.

## Key design constraint: two different Discord sources

#99 deliberately extracts `reply_to_message_id` from **`msg.message_reference`**
(IDs only, more reliable) via the `reply_to_id` helper that ignores forwards
(`MessageReferenceKind::Default` only).

The three new fields **cannot** come from `message_reference` — it carries only
IDs, not author or content. They must come from **`msg.referenced_message:
Option<Box<Message>>`**, which Discord populates with the full parent message on
`MESSAGE_CREATE` for replies.

Consequences to design around:

1. `referenced_message` can be `None` even when `reply_to_message_id` is `Some`
   (e.g. parent deleted, or Discord didn't hydrate it). So the three new fields
   are **best-effort** and independently optional — never assume they're present
   just because `reply_to_message_id` is.
2. Gate extraction on the **same reply semantics** as #99: only populate when the
   reference kind is `Default` (a true reply, not a forward/crosspost). Reuse the
   existing `reply_to_id` notion of "this is a reply" rather than trusting
   `referenced_message` presence alone.
3. Author name uses `author.name` to stay consistent with the existing top-level
   `user` field (which is `msg.author.name`).

## Scope decision: `Message` only, not `MessageEdit`

#99 added `reply_to_message_id` to both `Message` and `MessageEdit`. This change
applies the new author/preview fields to **`NotificationEvent::Message` only**.
Rationale: edits arrive as `MessageUpdateEvent`, whose `referenced_message` is
not reliably populated, so there's no zero-cost source for author/content there.
Call this asymmetry out in the CHANGELOG.

## Implementation

### 1. Extend the domain event — `src/discord/events.rs`

Add three fields to `NotificationEvent::Message` (after `reply_to_message_id`,
~line 42). Keep IDs as typed newtypes for domain type-safety (consistent with
#103); convert to strings only at the wire boundary.

```rust
NotificationEvent::Message {
    // ... existing fields ...
    reply_to_message_id: Option<MessageId>,
    /// If the message is a reply, the author ID of the replied-to message.
    reply_to_user_id: Option<UserId>,
    /// If the message is a reply, the author name of the replied-to message.
    reply_to_user: Option<String>,
    /// If the message is a reply, a short preview of the replied-to content.
    reply_to_content_preview: Option<String>,
}
```

### 2. Extract in `build_message_event` — `src/discord/events.rs` (~line 601)

Add a small helper next to `reply_to_id` (~line 595) that pulls the enriched
context from `referenced_message`, gated on the message actually being a reply:

```rust
/// Max characters retained from a replied-to message preview.
const REPLY_PREVIEW_MAX_CHARS: usize = 100;

/// Best-effort extraction of reply author + content preview from the parent
/// message embedded by Discord. Returns `(user_id, user, content_preview)`,
/// each independently optional. Only populated for genuine replies
/// (`MessageReferenceKind::Default`); forwards/crossposts yield all `None`.
fn reply_context(msg: &Message) -> (Option<UserId>, Option<String>, Option<String>) {
    let is_reply = msg
        .message_reference
        .as_ref()
        .is_some_and(|r| matches!(r.kind, MessageReferenceKind::Default));
    if !is_reply {
        return (None, None, None);
    }
    let Some(parent) = msg.referenced_message.as_deref() else {
        return (None, None, None);
    };
    let preview = reply_preview(&parent.content);
    (Some(parent.author.id), Some(parent.author.name.clone()), preview)
}

/// UTF-8-safe truncation to `REPLY_PREVIEW_MAX_CHARS` chars, with an ellipsis
/// when truncated. Returns `None` for empty content (e.g. attachment-only parent).
fn reply_preview(content: &str) -> Option<String> {
    if content.is_empty() {
        return None;
    }
    let mut preview: String = content.chars().take(REPLY_PREVIEW_MAX_CHARS).collect();
    if content.chars().count() > REPLY_PREVIEW_MAX_CHARS {
        preview.push('…');
    }
    Some(preview)
}
```

Wire it into the struct construction:

```rust
    let reply_to_message_id = msg.message_reference.as_ref().and_then(reply_to_id);
    let (reply_to_user_id, reply_to_user, reply_to_content_preview) = reply_context(msg);

    NotificationEvent::Message {
        // ... existing fields ...
        reply_to_message_id,
        reply_to_user_id,
        reply_to_user,
        reply_to_content_preview,
    }
```

Note: `referenced_message` is already accessed in the guild path (~line 183) for
mention detection, so the field/import is available; no new Serenity imports
needed beyond what #99 introduced (`MessageReferenceKind`).

### 3. Serialize conditionally — `src/mcp/notifications.rs` (~line 24–47)

Destructure the new fields in the `NotificationEvent::Message` arm and append
each to `meta` only when present, immediately after the `reply_to_message_id`
block (~line 47):

```rust
if let Some(reply_uid) = reply_to_user_id {
    meta["reply_to_user_id"] = json!(reply_uid.get().to_string());
}
if let Some(reply_user) = reply_to_user {
    meta["reply_to_user"] = json!(reply_user);
}
if let Some(preview) = reply_to_content_preview {
    meta["reply_to_content_preview"] = json!(preview);
}
```

## Test plan

### Unit tests — `src/discord/events.rs` (alongside `reply_to_id` tests, ~line 807)

- `reply_preview_returns_none_for_empty`
- `reply_preview_passes_through_short_content`
- `reply_preview_truncates_and_appends_ellipsis` (assert char count == 101 incl. `…`, and multibyte safety, e.g. a string of emoji)
- `reply_context_returns_none_for_non_reply` (no `message_reference`)
- `reply_context_returns_none_for_forward` (`MessageReferenceKind::Forward`)
- `reply_context_returns_none_when_referenced_message_absent` (reference present, parent `None`)

(Building `Message` fixtures may need a small constructor helper or
`serde_json::from_value` against a minimal gateway payload — check how #99's
tests construct messages, or test `reply_preview` in isolation which needs no
`Message`.)

### Unit tests — `src/mcp/notifications.rs` (alongside #99 tests)

- `test_message_includes_reply_context` — all three present → correct string values
- `test_message_omits_reply_context_when_none` — none of the three keys exist in `meta`
- `test_message_omits_preview_but_keeps_author` — author set, preview `None`

### Snapshot tests — `tests/mcp_protocol.rs` + `tests/snapshots/`

- Extend or add a reply snapshot to include the three fields (mirror
  `test_notification_message_reply_snapshot` from #99). Run with `cargo insta`
  and review the new/changed `.snap` files.

### Fixture updates (compile-breaking — every `NotificationEvent::Message { .. }`)

Add `reply_to_user_id: None, reply_to_user: None, reply_to_content_preview: None`
to all existing literal constructions:

- `src/delivery_buffer.rs` (~lines 166, 270, 362, 422, 462)
- `src/mcp/server.rs` (~lines 512, 546, 557; incl. `test_helpers`)
- `tests/mcp_protocol.rs` (~lines 643, 667, 686, 716, 784)
- `tests/delivery_pipeline.rs` (~lines 31, 76+)
- the #99 snapshot/reply test fixtures

A `cargo build` + `cargo nextest run` will surface any missed sites.

## Docs

- **CHANGELOG.md** — under `[Unreleased] > Added`: the three new `meta` fields,
  noting they're best-effort (require `referenced_message`), reply-only
  (`Default` kind), preview capped at 100 chars with ellipsis, and `Message`-only
  (not `MessageEdit`).
- **docs/design/architecture.md** — if it documents the notification `meta`
  shape, add the three fields next to `reply_to_message_id`.

## Risks / open questions

1. **`referenced_message` availability.** Discord usually includes it on
   `MESSAGE_CREATE` for replies, but not guaranteed (deleted parent, deep chains).
   Fields are optional by design; acceptable degradation.
2. **Preview length / privacy.** 100 chars is the proposed cap; confirm it's
   acceptable to forward parent content (it's already visible to the bot).
3. **Display name vs username.** Plan uses `author.name` for consistency with the
   existing `user` field. If `user` ever switches to global/display name, keep
   `reply_to_user` aligned.
4. **Ellipsis character.** Using `…` (U+2026); switch to `...` if the client
   prefers ASCII.

## Definition of done (per CLAUDE.md task-completion checklist)

`cargo xfmt` → `cargo clippy -- -W clippy::all` → `cargo nextest run` →
`cargo build --release` → commit + push.
