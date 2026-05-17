# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added
- Discord MCP channel server for Claude Code via `--channels`
- Manual JSON-RPC MCP server with `notifications/claude/channel` protocol
- 21 MCP tools across 5 categories:
  - Messaging: reply (chunked, threaded), react, edit_message, fetch_messages,
    download_attachment, get_message
  - Introspection: list_guilds, list_channels, get_channel, get_user,
    get_member, list_roles, list_emojis
  - Management: pin_message, unpin_message, create_thread, delete_message
  - Access: list_access_requests, approve_access, deny_access
  - Bot state: send_typing
- Access control gate with O(1) lookups (LoadedConfig with pre-parsed
  HashSet/HashMap, pre-compiled RegexSet for mention patterns)
- Access request queue for unknown senders (admin-gated, rate-limited
  notifications, 50-slot cap, 24h TTL, JSON persistence with atomic writes)
- Permission relay with Discord buttons routed to admins only
- TOML config with hot-reload (re-read per inbound message, file lock on
  correct fd)
- Message chunking with paragraph-aware splitting via `str::ceil_char_boundary`
- Custom emoji support in react tool (`<:name:id>` and `<a:name:id>`)
- Attachment downloads via reqwest with 25MB size limit (content-length
  pre-check + post-download validation)
- File send guard with symlink-resolving canonicalization
- Filename sanitization with basename extraction (path traversal prevention)
- Clean shutdown via CancellationToken (stdin EOF / SIGTERM / SIGINT)
- Periodic pruning of stale permissions and expired queue entries (60s timer)
- 75 tests (unit + integration + insta snapshots)
- Full CI pipeline: fmt, clippy, nextest, cross-compile, MSRV 1.95, cargo-deny
- Release pipeline: cargo-auditable, macOS universal via lipo, OIDC trusted
  publishing, git-cliff release notes
- Design spec in docs/design/ (problem, requirements, architecture, threat
  model, test plan)
