# Dione — Architecture

## Deployment topology

```
┌─────────────────────────────────────────────────────┐
│  K8s Cluster                                        │
│                                                     │
│  ┌──────────────────────────────────────────────┐   │
│  │  Dione Pod (sandboxed)                       │   │
│  │  ┌────────────────┐  ┌───────────────────┐   │   │
│  │  │ dione binary   │  │ ort (embeddings)  │   │   │
│  │  │ (Rust)         │  │ in-process        │   │   │
│  │  └───────┬────────┘  └───────────────────┘   │   │
│  └──────────┼───────────────────────────────────┘   │
│             │                                       │
│       ┌─────┼──────────┬──────────────┐             │
│       │     │          │              │             │
│  ┌────▼──┐ ┌▼──────┐ ┌▼──────────┐ ┌─▼──────────┐  │
│  │Qdrant │ │SQLite  │ │MCP Srv A  │ │MCP Srv B   │  │
│  │(vec)  │ │(PVC)   │ │(tools)    │ │(tools)     │  │
│  └───────┘ └───────┘ └───────────┘ └────────────┘  │
│                                                     │
│  Anthropic API ◄──── HTTPS ────► dione              │
│  Discord API   ◄──── WSS ─────► dione              │
└─────────────────────────────────────────────────────┘
```

## External services

| Service | Protocol | Purpose |
|---------|----------|---------|
| Anthropic Messages API | HTTPS | Model inference (Haiku, Sonnet, Opus). |
| Discord Gateway | WSS | Bot events, messages, interactions. |
| Qdrant | gRPC (cluster-internal) | Vector similarity search for semantic memory. |
| MCP Servers | Streamable HTTP (cluster-internal) | Tool execution. |

## Data stores

| Store | Technology | Contents |
|-------|-----------|----------|
| SQLite (PVC) | `sqlx` | User profiles, permissions, message logs, metering, config, personality facts. |
| Qdrant (deployment) | `qdrant-client` | Embedded conversation chunks, memory vectors, personality vectors. |

## Embeddings

Local inference via `ort` (ONNX Runtime). Model runs in-process in the Dione
pod. No external API calls for embeddings. Zero marginal cost.

Candidate models (384 dimensions):
- `all-MiniLM-L6-v2`
- `bge-small-en-v1.5`

Final choice TBD after benchmarking.

## Crate dependencies

| Crate | Purpose |
|-------|---------|
| `poise` | Discord framework (slash commands, prefix, events). Built on serenity. |
| `reqwest` | HTTP client for Anthropic Messages API. |
| `rmcp` ≥ 0.15 | MCP client. Official Rust SDK. Streamable HTTP transport. |
| `qdrant-client` | Vector DB client (gRPC). |
| `ort` | ONNX Runtime for local embedding inference. |
| `sqlx` + SQLite | Structured data persistence. |
| `tokio` | Async runtime. |
| `serde` / `serde_json` | Serialization. |
| `thiserror` | Domain error types. |
| `color-eyre` | Rich error reports in `main()`. |
| `camino` | UTF-8 path types. |
| `tracing` / `tracing-subscriber` | Structured logging. |
| `insta` | Snapshot testing (dev). |
| `test-case` | Parameterized testing (dev). |
| `pretty_assertions` | Better test diffs (dev). |

## Module structure

```
src/
  main.rs              — entry point. Minimal. Calls into lib.
  lib.rs               — crate root. Re-exports public API.

  bot/
    mod.rs             — re-exports.
    handler.rs         — poise framework setup, event dispatch.
    commands/
      mod.rs           — command registration.
      chat.rs          — conversation commands (slash + mention).
      admin.rs         — admin commands (budget, permissions, tools, model).
      memory.rs        — memory inspection / management commands.
      thread.rs        — thread lifecycle commands.

  claude/
    mod.rs             — re-exports.
    client.rs          — Anthropic Messages API client.
    messages.rs        — request/response types.
    routing.rs         — model selection (Haiku / Sonnet / Opus).
    streaming.rs       — SSE streaming for responses.

  memory/
    mod.rs             — re-exports. Memory system facade.
    store.rs           — SQLite-backed structured storage.
    embeddings.rs      — ort-based local embedding inference.
    semantic.rs        — Qdrant-backed vector search.
    context.rs         — context assembly (what goes into the prompt).
    global.rs          — global facts / knowledge base.
    user.rs            — per-user profiles and facts.
    channel.rs         — channel/thread topical context.

  personality/
    mod.rs             — re-exports.
    core.rs            — personality state, system prompt assembly.
    reflection.rs      — self-reflection / personality evolution.

  permissions/
    mod.rs             — re-exports.
    roles.rs           — role checks, admin detection, universal access flag.
    tools.rs           — tool allowlist management.

  metering/
    mod.rs             — re-exports.
    tracker.rs         — token counting, budget tracking.
    limits.rs          — rate limiting, per-user quotas.

  mcp/
    mod.rs             — re-exports.
    registry.rs        — MCP server connections, tool discovery.
    executor.rs        — tool invocation, result handling.

  config.rs            — config types, TOML loading, env var overlay.
```

## Model routing

Automatic complexity detection routes messages to the appropriate model:

| Model | Use case | Access |
|-------|----------|--------|
| Haiku 4.5 | Quick acknowledgments, simple Q&A, casual chat. | All users. |
| Sonnet 4.5 | Complex reasoning, code, long-form, technical. | All users. |
| Opus 4.6 | Deep analysis, nuanced conversation, creative. | Role-gated. |

Routing signals: message length, code/technical content, conversation depth,
explicit user override via slash command.

## Context assembly

When a message arrives, the context builder assembles:

1. **System prompt** — bot identity, personality state, guardrails.
2. **Global memories** — persistent facts about the world and users.
3. **User context** — what we know about this specific person.
4. **Channel/thread context** — recent conversation + semantically relevant
   older messages retrieved from Qdrant.
5. **The message** — the actual user input.

Token budget is managed per-request to stay within model context limits while
maximizing relevant context.

## MCP tool execution

Dione acts as an MCP client connecting to MCP servers deployed as separate K8s
services via Streamable HTTP transport.

- Tool discovery via `tools/list` on connection.
- Tool invocation via `tools/call` when the model requests it.
- Tool allowlisting and role-based access control managed in Discord by admins.
- Each MCP server has its own ServiceAccount with minimal RBAC permissions.
