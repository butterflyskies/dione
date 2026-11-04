#!/bin/sh
set -eu

case "$EVENT_NAME" in
  pull_request)
    # Compare the PR's own changes, not unrelated commits that reached main
    # after the branch diverged.
    CHANGE_BASE="$(git merge-base "$PR_BASE" "$PR_HEAD")"
    VERSION_BASE="$PR_BASE"
    AFTER="$PR_HEAD"
    ;;
  push)
    CHANGE_BASE="$PUSH_BEFORE"
    AFTER="$PUSH_AFTER"
    # New-branch pushes and unreachable before-SHAs (force push, shallow
    # history): inspect the landed commit's first parent.
    case "$CHANGE_BASE" in
      ''|0000000000000000000000000000000000000000)
        CHANGE_BASE="$(git rev-parse "${AFTER}^" 2>/dev/null || echo "$AFTER")" ;;
    esac
    if ! git cat-file -e "${CHANGE_BASE}^{commit}" 2>/dev/null; then
      CHANGE_BASE="$(git rev-parse "${AFTER}^" 2>/dev/null || echo "$AFTER")"
    fi
    VERSION_BASE="$CHANGE_BASE"
    ;;
  *)
    echo "::error::Unsupported release-hygiene event: ${EVENT_NAME}"
    exit 1
    ;;
esac

git cat-file -e "${CHANGE_BASE}^{commit}"
git cat-file -e "${VERSION_BASE}^{commit}"
git cat-file -e "${AFTER}^{commit}"
changed="$(git diff --name-only "$CHANGE_BASE" "$AFTER")"
package_version() {
  git show "$1:$2" | awk '
    /^\[package\]$/ { package = 1; next }
    /^\[/ { package = 0 }
    package && /^version = "/ { sub(/^version = "/, ""); sub(/"$/, ""); print; exit }
  '
}
members="$(printf '%s\n' "$changed" | sed -n 's@^\(crates/[^/]*\)/.*@\1@p' | sort -u)"
for member in $members; do
  manifest="${member}/Cargo.toml"
  if ! git cat-file -e "${AFTER}:${manifest}" 2>/dev/null; then
    continue
  fi
  new_member_version="$(package_version "$AFTER" "$manifest")"
  if git cat-file -e "${VERSION_BASE}:${manifest}" 2>/dev/null; then
    old_member_version="$(package_version "$VERSION_BASE" "$manifest")"
    if ! sh scripts/semver-is-greater.sh "$new_member_version" "$old_member_version"; then
      echo "::error::${member} changed but its version ${new_member_version} is not greater than current base version ${old_member_version} (both must be valid SemVer)."
      exit 1
    fi
  else
    comparison_status=0
    sh scripts/semver-is-greater.sh "$new_member_version" "$new_member_version" || comparison_status="$?"
    if [ "$comparison_status" -eq 2 ]; then
      echo "::error::${member} must declare a valid initial SemVer version."
      exit 1
    fi
  fi
done
if ! printf '%s\n' "$changed" | grep -Eq '^(src/|crates/[^/]+/src/)'; then
  echo "No Rust source changes in ${CHANGE_BASE}..${AFTER} — release hygiene not required."
  exit 0
fi

old_version="$(git show "${VERSION_BASE}:Cargo.toml" | sed -n 's/^version = "\(.*\)"$/\1/p' | head -1)"
new_version="$(git show "${AFTER}:Cargo.toml" | sed -n 's/^version = "\(.*\)"$/\1/p' | head -1)"
comparison_status=0
sh scripts/semver-is-greater.sh "$new_version" "$old_version" || comparison_status="$?"
if [ "$comparison_status" -ne 0 ]; then
  if [ "$comparison_status" -eq 2 ]; then
    echo "::error::Cargo.toml versions must be valid SemVer (candidate '${new_version}', baseline '${old_version}')."
    exit 2
  fi
  echo "::error::Rust source changed in ${CHANGE_BASE}..${AFTER} but Cargo.toml version ${new_version} is not greater than current base version ${old_version}."
  exit 1
fi
if ! git show "${AFTER}:CHANGELOG.md" | awk -v heading="## [${new_version}]" '
  $0 == heading { found = 1 }
  index($0, heading " - ") == 1 {
    date = substr($0, length(heading) + 4)
    if (date ~ /^[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]$/) found = 1
  }
  END { exit found ? 0 : 1 }
'; then
  echo "::error::Version bumped to ${new_version} but CHANGELOG.md has no '## [${new_version}]' entry (bare or followed by ' - YYYY-MM-DD')."
  exit 1
fi
echo "Rust source changed; version ${old_version} -> ${new_version} with a matching changelog entry. ✅"
