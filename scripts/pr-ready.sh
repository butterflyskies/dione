#!/bin/sh
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repo_root"

case "$repo_root" in
  /mnt/[A-Za-z]/*)
    echo "Run this gate from a native Linux checkout (for WSL, clone under the Linux home directory)." >&2
    exit 1
    ;;
esac
if ! git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  echo "Run this gate from a native Linux checkout (for WSL, clone under the Linux home directory)." >&2
  exit 1
fi
if [ -n "$(git status --porcelain)" ]; then
  echo "Commit or remove working-tree changes before checking a PR head." >&2
  exit 1
fi

base_ref=${1:-origin/main}
base_commit=$(git rev-parse --verify "${base_ref}^{commit}")
head_commit=$(git rev-parse --verify HEAD)
echo "Checking PR head $head_commit against $base_ref ($base_commit)"

if ! cargo nextest --version >/dev/null 2>&1; then
  echo "cargo-nextest is required. Install it before running this gate." >&2
  exit 1
fi
if ! cargo deny --version >/dev/null 2>&1; then
  echo "cargo-deny is required. Install it before running this gate." >&2
  exit 1
fi
if ! python3 --version >/dev/null 2>&1; then
  echo "python3 is required for the review receipt checks." >&2
  exit 1
fi

EVENT_NAME=pull_request PR_BASE=$base_commit PR_HEAD=$head_commit \
  sh .forgejo/scripts/release-hygiene.sh

cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy --workspace --all-targets --locked --features oneshot-test-seam -- -D warnings
cargo nextest run --workspace --locked --no-fail-fast --features oneshot-test-seam
cargo nextest run --workspace --locked --no-fail-fast --test oneshot_send
python3 -B -m unittest discover -s .forgejo/scripts -p 'test_*.py'
cargo +1.98.0 check --workspace --all-targets --locked
cargo package -p dione --locked
dione_version=$(sh scripts/workspace-package-version.sh dione)
target_dir=${CARGO_TARGET_DIR:-target}
sh scripts/verify-public-package-privacy.sh "${target_dir}/package/dione-${dione_version}.crate"
cargo deny check
cargo build --release --workspace --bins --locked
"${target_dir}/release/dione" --version

echo "PR head $head_commit passed the local checks. CI and final review are still required."
