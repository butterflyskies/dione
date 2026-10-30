# Dione — Design

Dione is a Rust Discord bridge for Claude Code, Codex, and Oh My Pi. This directory holds
design history and the adopted target; [ARCHITECTURE.md](../../ARCHITECTURE.md)
maps the current implementation.

## Status

| Phase | Artifact | Status |
|-------|----------|--------|
| 1. Problem space | [problem.md](problem.md) | Approved |
| 2. Requirements | [requirements.md](requirements.md) | Approved |
| 3. Architecture target | [architecture.md](architecture.md) | Adopted target; not an implementation map |
| 4. Threat model | [threat-model.md](threat-model.md) | Approved (lightweight) |
| 5. Test plan | [test-plan.md](test-plan.md) | Approved |

## Architectural direction

- [Three-machine target](architecture.md) separates inbound custody, resident
  turns, and outbound effects, with an independent supervisor.
- [Historical R1 through R6 messaging roadmap](refactor-plan.md) records the earlier
  boundary proposal and its supersession by PR551's vertical slices. It does
  not authorize another implementation pass.

## Quick Reference

- **Language:** Rust (edition 2024, MSRV 1.98 per [Cargo.toml](../../Cargo.toml))
- **Discord:** serenity (gateway + REST, raw events)
- **MCP server:** line-delimited JSON-RPC over stdio in
  [server.rs](../../src/mcp/server.rs); `rmcp` is also used as a client dependency
- **Runtime:** tokio
- **Config:** TOML at `~/.claude/channels/dione/config.toml`
- **License:** MIT OR Apache-2.0

The older requirements and test plan record historical design choices. Check
the [implementation map](../../ARCHITECTURE.md) and source before treating a
design note as current behavior.
