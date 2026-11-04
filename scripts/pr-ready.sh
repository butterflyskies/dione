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

if [ "$#" -gt 2 ]; then
  echo "usage: pr-ready.sh [base-ref] [review|release]" >&2
  exit 2
fi
base_ref=${1:-origin/main}
build_profile=${2:-review}
case "$build_profile" in
  review|release) ;;
  *)
    echo "unknown verification profile: ${build_profile}; choose review or release" >&2
    exit 2
    ;;
esac
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
if ! python3 -c 'import sys; print(sys.version); sys.exit(0 if sys.version_info >= (3, 9) else "Python 3.9 or newer is required for review receipt checks")'; then
  echo "Python 3.9 or newer is required for the review receipt checks." >&2
  exit 1
fi

EVENT_NAME=pull_request PR_BASE=$base_commit PR_HEAD=$head_commit \
  sh .forgejo/scripts/release-hygiene.sh

cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy --workspace --all-targets --locked --features oneshot-test-seam -- -D warnings
cargo nextest run --workspace --locked --no-fail-fast --features oneshot-test-seam
cargo nextest run --workspace --locked --no-fail-fast --test oneshot_send
cargo test --workspace --doc --locked
python3 -B -m unittest discover -s .forgejo/scripts -p 'test_*.py'
cargo +1.98.0 check --workspace --all-targets --locked
cargo package --workspace --locked
dione_version=$(sh scripts/workspace-package-version.sh dione)
target_dir=${CARGO_TARGET_DIR:-target}
sh scripts/verify-public-package-privacy.sh "${target_dir}/package/dione-${dione_version}.crate"
rendering_version=$(sh scripts/workspace-package-version.sh dione-rendering)
sh scripts/verify-public-package-privacy.sh "${target_dir}/package/dione-rendering-${rendering_version}.crate"
durable_version=$(sh scripts/workspace-package-version.sh dione-durable)
sh scripts/verify-public-package-privacy.sh "${target_dir}/package/dione-durable-${durable_version}.crate"
contracts_version=$(sh scripts/workspace-package-version.sh dione-contracts)
sh scripts/verify-public-package-privacy.sh "${target_dir}/package/dione-contracts-${contracts_version}.crate"
rook_version=$(sh scripts/workspace-package-version.sh dione-rook)
sh scripts/verify-public-package-privacy.sh "${target_dir}/package/dione-rook-${rook_version}.crate"
attention_version=$(sh scripts/workspace-package-version.sh dione-attention)
sh scripts/verify-public-package-privacy.sh "${target_dir}/package/dione-attention-${attention_version}.crate"
cargo deny check
cargo build --profile "$build_profile" --workspace --bins --locked
"${target_dir}/${build_profile}/dione" --version

echo "PR head $head_commit passed the local checks (${build_profile} build). CI and final review are still required."
