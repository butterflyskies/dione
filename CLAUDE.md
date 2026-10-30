# Dione — Agent Instructions

Dione is a Rust Discord bridge for Claude Code, Codex, and Oh My Pi. The agent host owns
inference and conversation state; Dione owns Discord access policy, event
delivery, and MCP tools. The [architecture map](ARCHITECTURE.md) describes the
current implementation, while the [design target](docs/design/architecture.md)
records the adopted direction.

See [AGENTS.md](AGENTS.md) for the full knowledge index.

## Quick reference

- **Coding standards**: [CODING_STANDARDS.md](CODING_STANDARDS.md)
- **Architecture**: [ARCHITECTURE.md](ARCHITECTURE.md)
- **Task completion**: format → clippy → nextest → release build → commit+push

## Commands

```bash
cargo xfmt                     # format (custom import grouping)
cargo clippy -- -W clippy::all # lint
cargo nextest run              # test
cargo build --release          # release build
```

## Serena memories

Project-specific memories are stored in `.serena/memories/`. Read them at
session start for full context. Key memories:

- `project_overview` — what Dione is and key decisions
- `architecture` — deployment topology and module structure
- `coding_standards` — summary (full detail in CODING_STANDARDS.md)
- `open_questions` — unresolved decisions
- `task_completion_checklist` — what to run after every change
- `suggested_commands` — development commands
