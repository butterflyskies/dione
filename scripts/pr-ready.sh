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
python3 scripts/run-tests.py --workspace --locked --no-fail-fast --features oneshot-test-seam
python3 scripts/run-tests.py --workspace --locked --no-fail-fast --test oneshot_send
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
evidence_version=$(sh scripts/workspace-package-version.sh dione-evidence)
sh scripts/verify-public-package-privacy.sh "${target_dir}/package/dione-evidence-${evidence_version}.crate"
contradictionary_version=$(sh scripts/workspace-package-version.sh dione-contradictionary)
sh scripts/verify-public-package-privacy.sh "${target_dir}/package/dione-contradictionary-${contradictionary_version}.crate"
bell_rings_version=$(sh scripts/workspace-package-version.sh dione-bell-rings)
sh scripts/verify-public-package-privacy.sh "${target_dir}/package/dione-bell-rings-${bell_rings_version}.crate"
delivery_scheduler_version=$(sh scripts/workspace-package-version.sh dione-delivery-scheduler)
sh scripts/verify-public-package-privacy.sh "${target_dir}/package/dione-delivery-scheduler-${delivery_scheduler_version}.crate"
access_requests_version=$(sh scripts/workspace-package-version.sh dione-access-requests)
sh scripts/verify-public-package-privacy.sh "${target_dir}/package/dione-access-requests-${access_requests_version}.crate"
mutes_version=$(sh scripts/workspace-package-version.sh dione-mutes)
sh scripts/verify-public-package-privacy.sh "${target_dir}/package/dione-mutes-${mutes_version}.crate"
coordination_version=$(sh scripts/workspace-package-version.sh dione-coordination)
teams_identities_version=$(sh scripts/workspace-package-version.sh dione-teams-identities)
discord_identities_version=$(sh scripts/workspace-package-version.sh dione-discord-identities)
pluralkit_identities_version=$(sh scripts/workspace-package-version.sh dione-pluralkit-identities)
mcp_tools_version=$(sh scripts/workspace-package-version.sh dione-mcp-tools)
provider_delivery_version=$(sh scripts/workspace-package-version.sh dione-provider-delivery)
teams_version=$(sh scripts/workspace-package-version.sh dione-teams)
omp_version=$(sh scripts/workspace-package-version.sh dione-omp)
codex_version=$(sh scripts/workspace-package-version.sh dione-codex)
claude_version=$(sh scripts/workspace-package-version.sh dione-claude)
discord_presence_version=$(sh scripts/workspace-package-version.sh dione-discord-presence)
discord_state_version=$(sh scripts/workspace-package-version.sh dione-discord-state)
discord_events_version=$(sh scripts/workspace-package-version.sh dione-discord-events)
tracing_channel_version=$(sh scripts/workspace-package-version.sh dione-tracing-channel)
config_watcher_version=$(sh scripts/workspace-package-version.sh dione-config-watcher)
drop_ledger_version=$(sh scripts/workspace-package-version.sh dione-drop-ledger)
permissions_version=$(sh scripts/workspace-package-version.sh dione-permissions)
discord_bells_version=$(sh scripts/workspace-package-version.sh dione-discord-bells)
discord_gate_version=$(sh scripts/workspace-package-version.sh dione-discord-gate)
oneshot_version=$(sh scripts/workspace-package-version.sh dione-oneshot)
sh scripts/verify-public-package-privacy.sh "${target_dir}/package/dione-coordination-${coordination_version}.crate"
sh scripts/verify-public-package-privacy.sh "${target_dir}/package/dione-teams-identities-${teams_identities_version}.crate"
sh scripts/verify-public-package-privacy.sh "${target_dir}/package/dione-discord-identities-${discord_identities_version}.crate"
sh scripts/verify-public-package-privacy.sh "${target_dir}/package/dione-pluralkit-identities-${pluralkit_identities_version}.crate"
sh scripts/verify-public-package-privacy.sh "${target_dir}/package/dione-mcp-tools-${mcp_tools_version}.crate"
sh scripts/verify-public-package-privacy.sh "${target_dir}/package/dione-provider-delivery-${provider_delivery_version}.crate"
sh scripts/verify-public-package-privacy.sh "${target_dir}/package/dione-teams-${teams_version}.crate"
sh scripts/verify-public-package-privacy.sh "${target_dir}/package/dione-omp-${omp_version}.crate"
sh scripts/verify-public-package-privacy.sh "${target_dir}/package/dione-codex-${codex_version}.crate"
sh scripts/verify-public-package-privacy.sh "${target_dir}/package/dione-claude-${claude_version}.crate"
sh scripts/verify-public-package-privacy.sh "${target_dir}/package/dione-discord-presence-${discord_presence_version}.crate"
sh scripts/verify-public-package-privacy.sh "${target_dir}/package/dione-discord-state-${discord_state_version}.crate"
sh scripts/verify-public-package-privacy.sh "${target_dir}/package/dione-discord-events-${discord_events_version}.crate"
sh scripts/verify-public-package-privacy.sh "${target_dir}/package/dione-tracing-channel-${tracing_channel_version}.crate"
sh scripts/verify-public-package-privacy.sh "${target_dir}/package/dione-config-watcher-${config_watcher_version}.crate"
sh scripts/verify-public-package-privacy.sh "${target_dir}/package/dione-drop-ledger-${drop_ledger_version}.crate"
sh scripts/verify-public-package-privacy.sh "${target_dir}/package/dione-permissions-${permissions_version}.crate"
sh scripts/verify-public-package-privacy.sh "${target_dir}/package/dione-discord-bells-${discord_bells_version}.crate"
sh scripts/verify-public-package-privacy.sh "${target_dir}/package/dione-discord-gate-${discord_gate_version}.crate"
sh scripts/verify-public-package-privacy.sh "${target_dir}/package/dione-oneshot-${oneshot_version}.crate"
pre_send_version=$(sh scripts/workspace-package-version.sh dione-pre-send)
outbound_consent_version=$(sh scripts/workspace-package-version.sh dione-outbound-consent)
config_version=$(sh scripts/workspace-package-version.sh dione-config)
sh scripts/verify-public-package-privacy.sh "${target_dir}/package/dione-pre-send-${pre_send_version}.crate"
sh scripts/verify-public-package-privacy.sh "${target_dir}/package/dione-outbound-consent-${outbound_consent_version}.crate"
sh scripts/verify-public-package-privacy.sh "${target_dir}/package/dione-config-${config_version}.crate"
presentation_version=$(sh scripts/workspace-package-version.sh dione-presentation)
sh scripts/verify-public-package-privacy.sh "${target_dir}/package/dione-presentation-${presentation_version}.crate"
harness_version=$(sh scripts/workspace-package-version.sh dione-harness)
sh scripts/verify-public-package-privacy.sh "${target_dir}/package/dione-harness-${harness_version}.crate"
identity_display_version=$(sh scripts/workspace-package-version.sh dione-identity-display)
sh scripts/verify-public-package-privacy.sh "${target_dir}/package/dione-identity-display-${identity_display_version}.crate"
gaie_version=$(sh scripts/workspace-package-version.sh dione-gaie)
sh scripts/verify-public-package-privacy.sh "${target_dir}/package/dione-gaie-${gaie_version}.crate"
vaelii_version=$(sh scripts/workspace-package-version.sh dione-vaelii)
sh scripts/verify-public-package-privacy.sh "${target_dir}/package/dione-vaelii-${vaelii_version}.crate"
messages_version=$(sh scripts/workspace-package-version.sh dione-messages)
sh scripts/verify-public-package-privacy.sh "${target_dir}/package/dione-messages-${messages_version}.crate"
rate_limiter_version=$(sh scripts/workspace-package-version.sh dione-rate-limiter)
sh scripts/verify-public-package-privacy.sh "${target_dir}/package/dione-rate-limiter-${rate_limiter_version}.crate"
cargo deny check
cargo build --profile "$build_profile" --workspace --bins --locked
"${target_dir}/${build_profile}/dione" --version

echo "PR head $head_commit passed the local checks (${build_profile} build). CI and final review are still required."
