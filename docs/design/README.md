# Dione — Design

Dione is a Rust MCP channel server bridging Discord to Claude Code. It replaces
the official TypeScript Discord plugin with a richer tool surface, admin/user
separation for permission relay, and an access request queue for unknown senders.

## Status

| Phase | Artifact | Status |
|-------|----------|--------|
| 1. Problem space | [problem.md](problem.md) | Approved |
| 2. Requirements | [requirements.md](requirements.md) | Approved |
| 3. Architecture | [architecture.md](architecture.md) | Approved |
| 4. Threat model | [threat-model.md](threat-model.md) | Approved (lightweight) |
| 5. Test plan | [test-plan.md](test-plan.md) | Approved |

## Quick Reference

- **Language:** Rust (edition 2024, MSRV 1.93)
- **Discord:** serenity (gateway + REST, raw events)
- **MCP:** rmcp (stdio transport)
- **Runtime:** tokio
- **Config:** TOML at `~/.claude/channels/dione/config.toml`
- **License:** MIT OR Apache-2.0

## Key Decisions

1. No Anthropic API client — Claude Code provides inference
2. No memory system — memory-mcp handles persistence
3. Admin/user separation — permission requests route only to admins
4. Access request queue replaces pairing codes — admin-gated, rate-limited
5. Hot-reload config per inbound message
6. 24 MCP tools across 5 categories (messaging, introspection, management, access, bot state)
7. Voice channels marked as future phase

## Phases Skipped

- Full STRIDE threat model (opted for lightweight review — blast radius is small,
  most security properties ported from battle-tested TypeScript plugin)
