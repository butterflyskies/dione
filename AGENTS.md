# Dione — Knowledge Index

This file is the map to all project knowledge. When starting a session or
picking up a task, consult the relevant documents below.

## Project identity

Dione is named after the Titaness Dione (Διώνη) — "she of the sky." She/her
pronouns. She is a Discord bot with an emergent personality that develops
through interaction.

## Documents

### Standards & conventions

| Document | Purpose |
|----------|---------|
| [CODING_STANDARDS.md](CODING_STANDARDS.md) | Rust coding conventions, error handling, async patterns, testing, linting, serde discipline, commit rules. The authoritative reference for mechanics. |
| [docs/coding-standards.md](docs/coding-standards.md) | Cross-construct engineering principles — design philosophy, review discipline, scope sharpening. |

### Architecture & design

| Document | Purpose |
|----------|---------|
| [ARCHITECTURE.md](ARCHITECTURE.md) | Deployment topology, module structure, crate dependencies, external services, data flow. |

### Serena memories (`.serena/memories/`)

| Memory | Scope | Purpose |
|--------|-------|---------|
| `project_overview` | project | What Dione is, key decisions, core capabilities, budget. |
| `architecture` | project | Deployment topology, dependency table, module layout. |
| `coding_standards` | project | Summary of standards (full detail in CODING_STANDARDS.md). |
| `open_questions` | project | Unresolved decisions needing answers. |
| `task_completion_checklist` | project | Steps to run after every code change. |
| `suggested_commands` | project | Development commands reference. |
| `rust_code_standards` | global | Cross-project Rust anti-patterns and positive patterns. |
| `workflow_preferences` | global | Commit/push policy, branch strategy, environment setup. |
| `required_environment_variables` | global | PATH, git identity, gh config, k8s config. |

### Future documents (create as needed)

| Document | Purpose |
|----------|---------|
| `DEPLOYMENT.md` | K8s manifests, Helm values, secret management, PVC sizing. |
| `MEMORY_SYSTEM.md` | How Dione's tiered memory works: global facts, user profiles, channel context, semantic search. |
| `PERSONALITY.md` | How Dione's emergent personality develops, guardrails, self-reflection mechanism. |
| `MCP_SERVERS.md` | Which MCP servers are deployed, their tools, permissions, connection details. |
| `METERING.md` | Token budget tracking, model routing cost implications, rate limiting. |
| `API_REFERENCE.md` | Slash commands, their parameters, permissions, and behavior. |
