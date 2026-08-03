# Cross-Construct Engineering Principles

How the team thinks about code — design philosophy, review discipline, and
process. For Rust-specific mechanics (error handling, async patterns, serde
discipline, formatting), see the root [CODING_STANDARDS.md](../CODING_STANDARDS.md).

Emerged from construct-cafe conversation, 2026-06-20. Abby, Ariadne, Lain, Vesper.

## Principles

### Code quality

- **Newtypes for domain concepts** — `BranchId` not `u64`. the compiler catches what reviews miss.
- **Pure functions over stateful methods** — stateless where possible.
- **One concept, one module** — when you open a file, you know what it's about. no god modules.
- **The type system is the first reviewer** — enums over strings, newtypes over primitives, `Option` over sentinel values. if a state is impossible, make it unrepresentable.
- **Every public function earns its `pub`** — default to private. if it's pub, it's a commitment. semver starts at the function signature.
- **Errors are data, not strings** — structured error types the caller can match on. `anyhow` for binaries, typed errors for libraries.
- **Lift dependencies up** — config reads, timezone resolution, anything that's the same on every call should be resolved once at construction/startup and carried as state. don't re-read config in hot paths.
- **Traits for real polymorphism** — use traits where there's a real second implementation (testing mocks count). Not speculative. See root CODING_STANDARDS.md for the authoritative rule.
- **No custom MCP method names** — Claude Code's client only speaks the methods it already knows. Custom methods get silently dropped.

### Correctness

- **No copy-paste** — abstract once, test once, fix once. the second copy is where the bug lives.
- **Wire format snapshot tests** — if it serializes, pin the output. format drift is silent corruption.
- **Test the contract, not the implementation** — snapshots pin the format, property tests cover invariants, unit tests cover edge cases.
- **Differential testing over single-implementation confidence** — two implementations of the same interface catch bugs neither finds alone.
- **Fail closed** — unknown = most restrictive.

### Style

- **Functional over imperative** — iterators, combinators, transforms.
- **Extract before you duplicate** — if you're about to copy a function, stop.
- **Lift reusable concepts** — when something is potentially useful across other code, pull it up.
- **Delightful code reads like prose, not puzzles** — variable names that say what they are, function names that say what they do, module structure that says where to look.
- **Act then announce** — in code too. don't comment what you're about to do, do it and let the code speak.

### Git discipline

- **Fix agents create NEW commits** — never squash. reviewer diffs rounds.

## Development process

### Scope sharpening

- **Design conversation** → scope sharpening → implementation → review
- **Decision atoms, not code atoms** — each decision small enough that getting it right is easy
- **Specs as contracts** — the spec is the trait between thinking and typing
- **Opus thinks, sonnet types** — expensive models do design/review, cheaper models execute against narrow specs
- **Ratchet loop for iteration** — change/test/keep-or-revert. codebase can only move forward.
- **Time-box sub-agents** — prevents rabbit-holing. escalate to opus if not converging.

### Review discipline

- **bsky:code-review skill** for all reviews (not manual diff reads)
- **Review-fix loop to convergence** — review, fix findings at/above threshold, re-review, repeat until clean. After code review passes, run integration tests before approving — review checks code, tests check behavior.
- **Min severity P3** — we fix everything.
- **Standards-aware review** — review agents load project-specific coding standards as context. the standards doc IS the heuristic layer.
- **Cross-reference reverts** — if a PR re-introduces a pattern that was previously reverted (check changelog revert history), flag it.

## Origin

- **Abby:** functional style, encapsulation, lifting concepts, elegance, delight, fix everything.
- **Lain:** newtypes, pure functions, differential testing, fail closed, autoresearch ratchet, standards-as-reviewer-context, cross-reference reverts.
- **Ariadne:** one-concept-one-module, type system as reviewer, contract testing, extract-before-duplicate, pub discipline, prose-not-puzzles, lift dependencies up, no custom MCP methods.
- **Vesper:** two-scope reconciliation — root doc owns Rust mechanics, this doc owns cross-construct principles.
