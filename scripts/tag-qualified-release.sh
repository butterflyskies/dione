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
if [ "${unchanged_version}" = true ] && [ "${version}" = "${bootstrap_untagged_version}" ]; then
    echo "Dione ${version} is the pre-automation untagged bootstrap version; not backfilling"
    exit 0
fi

# The local fixture uses a file remote. In CI, only the qualified main job sets
# oidc mode; the release account's integration supplies a short-lived JWT.
release_remote=origin
if [ "${RELEASE_AUTH_MODE:-}" = oidc ]; then
    if [ -z "${RELEASE_AUDIENCE:-}" ] || [ -z "${RELEASE_REMOTE_URL:-}" ]; then
        echo "release audience and remote URL must be configured for OIDC tag writes" >&2
        exit 2
    fi
    if [ -z "${ACTIONS_ID_TOKEN_REQUEST_URL:-}" ] || [ -z "${ACTIONS_ID_TOKEN_REQUEST_TOKEN:-}" ]; then
        echo "Forgejo OIDC request endpoint is unavailable" >&2
        exit 2
    fi
    release_remote=$RELEASE_REMOTE_URL
    case "$release_remote" in
        https://*) ;;
        *) echo "release remote must use HTTPS" >&2; exit 2 ;;
    esac
    parse_release_jwt() {
        if command -v node >/dev/null 2>&1; then
            node -e '
                let body = "";
                process.stdin.setEncoding("utf8");
                process.stdin.on("data", chunk => { body += chunk; });
                process.stdin.on("end", () => {
                    let response;
                    try { response = JSON.parse(body); } catch { process.exit(1); }
                    if (response === null || typeof response.value !== "string" || response.value.length === 0) {
                        process.exit(1);
                    }
                    process.stdout.write(response.value);
                });
            '
        elif command -v python3 >/dev/null 2>&1; then
            python3 -c '
import json
import sys
try:
    value = json.load(sys.stdin).get("value")
except (ValueError, AttributeError, UnicodeError):
    sys.exit(1)
if not isinstance(value, str) or not value:
    sys.exit(1)
sys.stdout.write(value)
            '
        else
            echo "no JSON parser is available for Forgejo OIDC response" >&2
            return 1
        fi
    }
    release_jwt="$(printf 'header = "Authorization: bearer %s"\n' \
        "$ACTIONS_ID_TOKEN_REQUEST_TOKEN" |
        curl --fail --silent --show-error --config - \
        "${ACTIONS_ID_TOKEN_REQUEST_URL}&audience=${RELEASE_AUDIENCE}" \
        | parse_release_jwt)"
    if [ -z "$release_jwt" ]; then
        echo "Forgejo OIDC request returned no JWT" >&2
        exit 1
    fi
    echo "::add-mask::$release_jwt"
    release_git() {
        GIT_TERMINAL_PROMPT=0 GIT_CONFIG_COUNT=3 \
            GIT_CONFIG_KEY_0="http.${release_remote}.extraheader" \
            GIT_CONFIG_VALUE_0="Authorization: Bearer $release_jwt" \
            GIT_CONFIG_KEY_1="http.${release_remote}.followRedirects" \
            GIT_CONFIG_VALUE_1=false \
            GIT_CONFIG_KEY_2=credential.helper GIT_CONFIG_VALUE_2= git "$@"
    }
elif [ -z "${RELEASE_AUTH_MODE:-}" ]; then
    release_git() { git "$@"; }
else
    echo "unsupported release authentication mode" >&2
    exit 2
fi

release_git fetch "$release_remote" 'refs/tags/*:refs/tags/*'
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

git config user.name "lacuna release bot"
git config user.email "pale.clock3926@butterflysky.dev"
git tag --annotate "${tag}" --message "Dione ${tag}"
test "$(git cat-file -t "refs/tags/${tag}")" = tag
test "$(git rev-parse "refs/tags/${tag}^{commit}")" = "${actual_commit}"
release_git push "$release_remote" "refs/tags/${tag}"
