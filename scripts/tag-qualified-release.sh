#!/bin/sh
set -eu

# Dione 0.42.0 was already on main when automated release tagging was added.
# It is deliberately untagged and must never be backfilled by this automation.
bootstrap_untagged_version=0.42.0

if [ -z "${EXPECTED_COMMIT:-}" ]; then
    echo "EXPECTED_COMMIT is required" >&2
    exit 2
fi
if [ -z "${PUSH_BEFORE:-}" ]; then
    echo "PUSH_BEFORE is required" >&2
    exit 2
fi

actual_commit="$(git rev-parse HEAD)"
if [ "${actual_commit}" != "${EXPECTED_COMMIT}" ]; then
    echo "checked-out commit ${actual_commit} does not match event commit ${EXPECTED_COMMIT}" >&2
    exit 1
fi

case "${PUSH_BEFORE}" in
    0000000000000000000000000000000000000000)
        echo "cannot qualify a release without the previous main commit" >&2
        exit 1
        ;;
esac
if ! git cat-file -e "${PUSH_BEFORE}^{commit}" 2>/dev/null; then
    echo "previous main commit ${PUSH_BEFORE} is unavailable" >&2
    exit 1
fi
if ! git merge-base --is-ancestor "${PUSH_BEFORE}" "${actual_commit}"; then
    echo "previous main commit ${PUSH_BEFORE} is not an ancestor of ${actual_commit}" >&2
    exit 1
fi

version="$(scripts/workspace-package-version.sh dione)"
base_manifest="$(git show "${PUSH_BEFORE}:Cargo.toml")"
base_version="$(printf '%s\n' "${base_manifest}" | sed -n 's/^version = "\([^"]*\)"$/\1/p' | head -n 1)"
if [ -z "${base_version}" ]; then
    echo "could not read the previous Dione version" >&2
    exit 1
fi
unchanged_version=false
if [ "${version}" = "${base_version}" ]; then
    unchanged_version=true
else
    comparison_status=0
    sh scripts/semver-is-greater.sh "${version}" "${base_version}" || comparison_status="$?"
    if [ "${comparison_status}" -ne 0 ]; then
        if [ "${comparison_status}" -eq 2 ]; then
            echo "Dione versions must be valid SemVer (candidate '${version}', baseline '${base_version}')" >&2
            exit 2
        fi
        echo "Dione version ${version} is not greater than previous main version ${base_version}" >&2
        exit 1
    fi

    if git diff --quiet "${PUSH_BEFORE}" "${actual_commit}" -- Cargo.toml; then
        echo "Dione version changed without a Cargo.toml change" >&2
        exit 1
    fi
    if git diff --quiet "${PUSH_BEFORE}" "${actual_commit}" -- CHANGELOG.md; then
        echo "Dione version changed without a changelog change" >&2
        exit 1
    fi
fi
if ! awk -v heading="## [${version}]" '
    $0 == heading { found = 1 }
    index($0, heading " - ") == 1 {
        date = substr($0, length(heading) + 4)
        if (date ~ /^[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]$/) found = 1
    }
    END { exit found ? 0 : 1 }
' CHANGELOG.md; then
    echo "CHANGELOG.md has no exact heading for Dione ${version}" >&2
    exit 1
fi

tag="v${version}"
git fetch origin 'refs/tags/*:refs/tags/*'
if git rev-parse --verify --quiet "refs/tags/${tag}" >/dev/null; then
    if [ "$(git cat-file -t "refs/tags/${tag}")" != tag ]; then
        echo "${tag} exists but is not annotated" >&2
        exit 1
    fi
    tagged_commit="$(git rev-parse "refs/tags/${tag}^{commit}")"
    if [ "${unchanged_version}" = true ]; then
        if ! git merge-base --is-ancestor "${tagged_commit}" "${actual_commit}"; then
            echo "${tag} does not point to an ancestor of ${actual_commit}" >&2
            exit 1
        fi
        tagged_manifest="$(git show "${tagged_commit}:Cargo.toml")"
        tagged_version="$(printf '%s\n' "${tagged_manifest}" | sed -n 's/^version = "\([^"]*\)"$/\1/p' | head -n 1)"
        if [ "${tagged_version}" != "${version}" ]; then
            echo "${tag} points to a commit with Dione version ${tagged_version:-unknown}, not ${version}" >&2
            exit 1
        fi
        echo "${tag} already annotates a released ancestor; leaving it unchanged"
        exit 0
    else
        if [ "${tagged_commit}" != "${actual_commit}" ]; then
            echo "${tag} points to ${tagged_commit}, not ${actual_commit}" >&2
            exit 1
        fi
        echo "${tag} already annotates the exact qualified commit"
        exit 0
    fi
fi

if [ "${unchanged_version}" = true ] && [ "${version}" = "${bootstrap_untagged_version}" ]; then
    echo "Dione ${version} is the pre-automation untagged bootstrap version; not backfilling"
    exit 0
fi
if [ "${unchanged_version}" = true ]; then
    echo "Dione version is unchanged at ${version}; reconciling its missing release tag"
fi

latest_release=""
for historical_tag in $(git tag --list 'v*'); do
    historical_version=${historical_tag#v}
    if [ -z "${latest_release}" ]; then
        if sh scripts/semver-is-greater.sh "${historical_version}" "${historical_version}"; then
            :
        elif [ "$?" -eq 2 ]; then
            continue
        fi
        latest_release=${historical_version}
    elif sh scripts/semver-is-greater.sh "${historical_version}" "${latest_release}"; then
        latest_release=${historical_version}
    elif [ "$?" -eq 2 ]; then
        continue
    fi
done
if [ -n "${latest_release}" ] && ! sh scripts/semver-is-greater.sh "${version}" "${latest_release}"; then
    echo "Dione version ${version} is not greater than historical release ${latest_release}" >&2
    exit 1
fi

git config user.name "Dione Release Bot"
git config user.email "dione-release-bot@noreply.local"
git tag --annotate "${tag}" --message "Dione ${tag}"
test "$(git cat-file -t "refs/tags/${tag}")" = tag
test "$(git rev-parse "refs/tags/${tag}^{commit}")" = "${actual_commit}"
git push origin "refs/tags/${tag}"
