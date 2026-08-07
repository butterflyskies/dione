# Dione Coding Standards

Informed by Rain's (sunshowers) conventions from
[cargo-nextest](https://github.com/nextest-rs/nextest) and
[Rust CLI Recommendations](https://rust-cli-recommendations.sunshowers.io/).

---

## Workspace & Cargo.toml

Centralize all dependency versions in the root `Cargo.toml` under
`[workspace.dependencies]`. Individual crates reference them with
`.workspace = true`. This eliminates version drift across crates.

```toml
[workspace]
resolver = "2"
members = ["dione", "dione-core"]

[workspace.package]
edition = "2024"

[workspace.dependencies]
tokio = { version = "1", features = ["rt-multi-thread", "macros", "signal"] }
serde = { version = "1.0", features = ["derive"] }
thiserror = "2.0"
tracing = "0.1"

[workspace.lints.clippy]
format_push_string = "warn"
ref_option = "warn"
```

Crate-level `Cargo.toml` references workspace settings:

```toml
[package]
edition.workspace = true

[dependencies]
tokio.workspace = true
serde.workspace = true

[lints]
workspace = true
```

Optimize heavy dev dependencies even in debug mode:

```toml
[profile.dev.package.backtrace]
opt-level = 3
```

Release profile uses stripped binaries with LTO:

```toml
[profile.release]
lto = true
strip = true
```

---

## Error handling

### Error struct + ErrorKind enum pattern

The struct carries context (file path, config key, user ID, etc.), the kind
enum carries the variant. This gives callers both structured access to what went
wrong and rich display output.

```rust
#[derive(Debug, Error)]
#[error("failed to load config from `{path}`")]
pub struct ConfigError {
    pub path: Utf8PathBuf,
    #[source]
    pub kind: ConfigErrorKind,
}

#[derive(Debug, Error)]
pub enum ConfigErrorKind {
    #[error(transparent)]
    Io(std::io::Error),
    #[error(transparent)]
    Deserialize(Box<serde_path_to_error::Error<toml::de::Error>>),
}
```

### Rules

- `thiserror` for all error types. Each module defines its own.
- `color_eyre` in `main()` for rich developer-facing error reports with
  backtraces.
- Never `unwrap()` or `expect()` in production code. Only acceptable in tests
  and in `main()` for startup-time config that genuinely cannot proceed without.
- Error propagation with `?` everywhere. Functions return `Result<T, E>`.
- Error display messages are **lowercase sentence fragments** suitable for
  chaining: `"failed to parse config"`, not `"Failed to parse config"` or
  `"Config parsing error"`.
- Graceful degradation: a failed API call or DB query logs and responds to the
  user with a friendly error. Never crash the bot.
- No `#[non_exhaustive]` — public error enums stay exhaustive (banned across all
  projects, 2026-06-27). Adding a variant is an honest breaking change, caught by
  `cargo-semver-checks` in CI and exhaustiveness lints, and versioned deliberately.

---

## Async patterns

- All I/O through `tokio`. No blocking calls on the async runtime.
- `tokio::spawn` for truly independent background work (periodic metering
  flush, personality reflection, etc.).
- Prefer structured concurrency — `tokio::select!`, `JoinSet` — over
  fire-and-forget spawns.
- Channel-based communication (`tokio::sync::mpsc`, `broadcast`) between
  subsystems rather than shared mutable state.
- No `Arc<Mutex<T>>` unless absolutely necessary. Prefer message-passing.
- Be selective with async. Use it for I/O and concurrency; keep business logic
  synchronous where possible.
- For coordination: dedicated channels per task, not broadcast.

---

## Module boundaries & visibility

- **`mod.rs` files only for re-exports.** No nontrivial logic. Put logic in
  named submodules (`imp.rs`, `routing.rs`, etc.).
- Internal types are `pub(crate)` or `pub(super)`. Only what command handlers
  need is fully `pub`.
- No module reaches into another module's internals. Go through the public
  facade.
- Traits for cross-cutting abstractions only where there's a real second
  implementation (e.g., testing mocks). Not speculative.
- **Binary vs library separation**: `main.rs` is minimal, calls into lib.
  Actual logic lives in modules under the library crate.

---

## Naming conventions

| Kind | Convention | Example |
|------|-----------|---------|
| Types | `PascalCase` | `ContextBuilder`, `ModelRoute` |
| Functions/methods | `snake_case` | `assemble_context`, `route_model` |
| Constants | `SCREAMING_SNAKE_CASE` | `MAX_CONTEXT_TOKENS` |
| Modules | `snake_case` | `memory`, `permissions` |
| Enum variants | `PascalCase` | `ModelRoute::Haiku` |

- No abbreviations unless universally understood. `msg` and `ctx` are fine.
  `perm_mgr` is not.
- Builder pattern for complex config structs (e.g., `ContextBuilder`).
- Newtypes for domain types when it aids clarity. Don't pass raw `String` when
  `ChannelTopic` or `UserFact` is meant.

---

## Imports

- Always import at the top of the module. Never inside function bodies
  (exception: `#[cfg]`-gated imports).
- Import `std::fmt` as a module and use `fmt::Display`. Never write
  `std::fmt::Display` as a fully qualified path.
- Prefer importing types directly. Import modules when conventional
  (e.g., `use std::fmt`).
- `cargo xfmt` handles grouping (see [Formatting & linting](#formatting--linting)).

---

## Testing

### Runner

Use `cargo nextest run` as the test runner. Not `cargo test`.

### Organization

- `#[cfg(test)] mod tests` in the same file for unit tests.
- Integration tests in a separate `tests/` directory (or `integration-tests/`
  crate if complex).
- Fixture data in dedicated modules or files. Prefer models over hand-written
  spot checks.

### Practices

- Unit tests for pure logic: routing decisions, permission checks, context
  assembly.
- Integration tests for cross-module behavior.
- Mock external services (Anthropic API, Discord, Qdrant) via traits. Tests
  never hit real APIs.
- `#[tokio::test]` for async tests.
- Test **behavior**, not implementation details.

### Testing crates

| Crate | Purpose |
|-------|---------|
| `insta` | Snapshot testing. Context assembly output, serialized responses. |
| `test-case` | Parameterized tests. Routing logic, permission matrix. |
| `pretty_assertions` | Better diff output on assertion failures. |

Snapshot testing configuration (`.config/insta.yaml`):

```yaml
test:
  runner: nextest
```

---

## Logging & observability

- `tracing` with structured fields, not string interpolation.
- Every external API call (Anthropic, Qdrant, MCP, Discord) logged at `debug`
  with timing.
- Token usage logged at `info` on every Claude response.
- Span-based tracing: follow a request from Discord message through context
  assembly, API call, and response.

### Log levels

| Level | Use |
|-------|-----|
| `error` | Failures needing attention. Bot cannot fulfill request. |
| `warn` | Degraded behavior. Fallback used, retry needed. |
| `info` | Operational events. Bot started, command received, tokens spent. |
| `debug` | Troubleshooting. API call details, context assembly steps. |
| `trace` | Wire-level. Raw HTTP bodies, full prompt text. |

---

## Configuration

- TOML config file for static settings (URLs, budget limits, role mappings).
  Use the `.config/` directory convention where appropriate.
- Environment variables for secrets (tokens, API keys). Never in config files.
- `config.rs` deserializes into typed structs with `serde`.
- Sensible defaults for everything that can have one.
- Validate at startup. Fail fast with clear error messages if config is wrong.

---

## Formatting & linting

### Formatting

Use `cargo xfmt` via a `.cargo/config.toml` alias:

```toml
[alias]
xfmt = "fmt -- --config imports_granularity=Crate --config group_imports=One --config format_code_in_doc_comments=true"
```

This gives:
- **`imports_granularity=Crate`**: groups imports by crate, merging individual
  items from the same crate.
- **`group_imports=One`**: all imports in a single group (no blank lines between
  std/external/crate imports).
- **`format_code_in_doc_comments=true`**: formats Rust code blocks inside doc
  comments.

### Linting

- `cargo clippy -- -W clippy::all` — fix all warnings before code is complete.
- Use `#[expect(...)]` instead of `#[allow(...)]` when suppressing lints. The
  `expect` attribute warns if the lint is no longer triggered, keeping the
  codebase clean of stale suppressions.
- Targeted clippy lints in `[workspace.lints.clippy]`. Not `clippy::pedantic`.
- `RUSTFLAGS: -D warnings` in CI to treat warnings as errors.

---

## Documentation

- `///` doc comments on all public types and functions.
- `#![warn(missing_docs)]` on library crates.
- `//!` module-level docs explaining purpose and key concepts.
- Comments explain **"why"**, not "what". Always end with periods. Sentence
  case in headings. Oxford comma.
- Don't omit articles ("a", "an", "the").
- No inline comments except where the *why* isn't obvious from the code.
- Do not add narrative comments in function bodies. Only comment when something
  is non-obvious or needs a deeper "why" explanation.
- README.md for project setup, architecture overview, and deployment.

---

## Serde discipline

- **Never** `#[serde(flatten)]`. It breaks `serde_ignored` due to internal
  buffering.
- **Never** `#[serde(untagged)]` for deserializers. It produces poor error
  messages. Write custom visitors instead.
- Use `serde_path_to_error` to get precise error paths in deserialization
  failures.
- Use `serde_ignored` to detect and warn about unrecognized config keys.

---

## Paths

Use `camino::Utf8PathBuf` instead of `PathBuf` when paths are known to be
UTF-8. This covers config paths, data directories, and most application-level
path handling.

---

## Type system patterns

- **Newtypes** for domain types. Don't pass raw `String` or `u64` when a
  `UserId` or `GuildId` is meant. (Discord IDs are already typed via
  serenity/poise.)
- **Builder patterns** for complex construction.
- **Exhaustive public enums** — `#[non_exhaustive]` is banned (all projects,
  2026-06-27). Breaking changes are managed via `cargo-semver-checks` in CI and
  exhaustiveness lints, not hedged with the attribute.
- **Lifetimes** used to avoid cloning when data has natural tree structure.
  But don't add lifetime annotations where they aren't needed.

---

## Dependencies

- Prefer well-maintained crates with active GitHub repos.
- Pin major versions in `Cargo.toml` (e.g., `tokio = "1"`, not `"*"`).
- Minimal feature flags. Only enable what we use.
- `cargo audit` periodically for security vulnerabilities.

---

## Commits

- **Atomic commits**: each commit is a logical unit of change.
- **Bisectable history**: every commit must build and pass all checks.
- **Separate concerns**: formatting fixes and refactoring in separate commits
  from feature changes.
- Commit and push after each successful change (per workflow preferences).

---

## Anti-patterns to avoid

1. `.clone()` instead of borrowing — unnecessary allocations.
2. `.unwrap()` / `.expect()` overuse — use `unwrap_or`, `unwrap_or_default`,
   or propagate errors.
3. `.collect()` too early — prefer lazy iteration. Only collect when multiple
   passes are needed.
4. `unsafe` without clear need.
5. Over-abstracting with traits/generics — keep code concrete and readable.
6. Global mutable state — breaks testability and thread safety.
7. Macros that hide logic — keep logic visible and debuggable.
8. Ignoring lifetime annotations — but don't add them where not needed.
9. Premature optimization — correctness first.

---

## Positive patterns

- `const` slices for rule sets (zero-cost, no allocation).
- `.contains()` over `.iter().any()` for slice membership.
- Stripped + LTO release builds for deployment.
- Lazy iteration; only collect when multiple passes needed.
- Two-pass iteration over short collections preferred to collecting into `Vec`
  when both `.count()` and `.any()` are needed.

---

## Task completion checklist

After every code change, run these steps in order:

1. `cargo xfmt` — format all code.
2. `cargo clippy -- -W clippy::all` — fix all warnings.
3. `cargo nextest run` — all tests must pass.
4. `cargo build --release` — confirm release build succeeds.
5. Commit and push.

Do **not** consider a change complete until all five steps pass.
