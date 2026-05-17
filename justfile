# Run all checks in CI order
check: fmt-check lint test

# Format all code
fmt:
    cargo fmt

# Check formatting without modifying
fmt-check:
    cargo fmt --check

# Run clippy with all warnings as errors
lint:
    cargo clippy -- -D warnings

# Run tests with nextest (parallel, fail-fast off)
test:
    cargo nextest run --workspace --no-fail-fast

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
