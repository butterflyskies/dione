# Run all checks in CI order
check: fmt-check lint test

# Format all code (with custom import grouping)
fmt:
    cargo xfmt

# Check formatting without modifying
fmt-check:
    cargo fmt --check -- --config imports_granularity=Crate --config group_imports=One --config format_code_in_doc_comments=true

# Run clippy with all warnings as errors
lint:
    cargo clippy --workspace --all-targets -- -D warnings
    cargo clippy --workspace --all-targets --features oneshot-test-seam -- -D warnings

# Run tests with nextest (parallel, fail-fast off)
test:
    cargo nextest run --workspace --no-fail-fast --features oneshot-test-seam
    # Production feature set: proves the endpoint-override seam is absent.
    cargo nextest run --workspace --no-fail-fast --test oneshot_send

# Verify documentation builds without warnings
doc:
    RUSTDOCFLAGS="-D warnings" cargo doc --no-deps

# Install the binary locally
install:
    cargo install --path .

# Full pre-push gate (format → lint → test → doc → release build)
pre-push: fmt-check lint test doc
    cargo build --release

# Update snapshot tests (after intentional changes)
snap-review:
    cargo insta review
