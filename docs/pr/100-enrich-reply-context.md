# PR: Enrich inbound reply context (#100)

## Summary

When a Discord message is a reply, inbound MCP notifications now include three
best-effort fields in `params.meta` alongside the existing `reply_to_message_id`:

- `reply_to_user_id` — author snowflake of the replied-to message
- `reply_to_user` — author username of the replied-to message
- `reply_to_content_preview` — first 100 characters of the parent content (UTF-8-safe, with `…` when truncated)

All three are omitted when the message is not a genuine reply, when Discord did
not hydrate `referenced_message`, or when the preview content is empty
(attachment-only parent). Extraction uses zero additional API calls.

Reply-only gating matches #99: only `MessageReferenceKind::Default` references
(forwards/crossposts excluded). Fields apply to `NotificationEvent::Message`
only, not `MessageEdit`.

## Linear / issue

Closes #100

## Test plan

- [x] `cargo fmt --check` on all 6 in-scope Rust files (xfmt config)
- [x] `cargo clippy -- -W clippy::all` — clean
- [x] `cargo nextest run` — 309 passed, 1 skipped
- [x] `cargo build --release` — clean
- [x] Unit tests: `reply_preview_*`, `reply_context_*`, `build_message_event_populates_reply_context_for_reply`
- [x] Unit tests: notification serialization (`includes`, `omits_when_none`, `omits_preview_but_keeps_author`)
- [x] Snapshot: `test_notification_message_reply_snapshot` includes all three enriched fields

## Files changed

| File | Change |
|------|--------|
| `src/discord/events.rs` | Domain fields, `is_reply_reference` / `reply_context` / `reply_preview`, unit tests |
| `src/mcp/notifications.rs` | Conditional wire serialization + unit tests |
| `src/delivery_buffer.rs` | Test fixture fields |
| `src/mcp/server.rs` | Test fixture fields |
| `tests/delivery_pipeline.rs` | Test fixture fields |
| `tests/mcp_protocol.rs` | Fixtures + enriched reply snapshot test |
| `tests/snapshots/mcp_protocol__notification_message_reply_snapshot.snap` | Updated snapshot |
| `CHANGELOG.md` | `[Unreleased] > Added` entry |

## Known limitations

- Fields are best-effort: `referenced_message` may be absent even when
  `reply_to_message_id` is present (deleted parent, etc.).
- Preview capped at 100 characters; uses `author.name` for consistency with the
  top-level `user` field.
- Not emitted on `message_edit` notifications (Discord does not reliably
  populate `referenced_message` on edits).

## Proposed commit message

```
feat: enrich inbound reply notifications with author and preview (#100)

Surface reply_to_user_id, reply_to_user, and reply_to_content_preview in
MCP message meta when Discord hydrates referenced_message on replies.
Best-effort, zero extra API calls; Message-only (not MessageEdit).
```
