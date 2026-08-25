#!/bin/sh
set -eu

if [ "$#" -lt 2 ]; then
    echo "usage: verify-crates-io-owners.sh <crate> <expected-owner>..." >&2
    exit 64
fi

crate_name="$1"
shift
scratch_parent="${CARGO_TARGET_DIR:-target}"
mkdir -p "${scratch_parent}"
response="$(mktemp "${scratch_parent}/crate-owners.XXXXXX")"
trap 'rm -f "${response}"' EXIT HUP INT TERM

status="$(curl --silent --show-error --connect-timeout 5 --max-time 20 \
    --user-agent 'dione-release-workflow/1.0 (https://github.com/butterflyskies/dione)' \
    --output "${response}" \
    --write-out '%{http_code}' \
    "https://crates.io/api/v1/crates/${crate_name}/owners")"

case "${status}" in
    200) ;;
    404)
        echo "crates.io namespace does not exist: ${crate_name}" >&2
        exit 3
        ;;
    *)
        echo "crates.io returned HTTP ${status} while checking owners for ${crate_name}" >&2
        exit 1
        ;;
esac

actual="$(python3 -c \
    'import json, sys; print("\n".join(sorted(owner["login"] for owner in json.load(sys.stdin)["users"])))' \
    < "${response}")"
expected="$(printf '%s\n' "$@" | sort)"

if [ "${actual}" != "${expected}" ]; then
    echo "crates.io ownership mismatch for ${crate_name}" >&2
    echo "expected:" >&2
    printf '%s\n' "${expected}" >&2
    echo "actual:" >&2
    printf '%s\n' "${actual}" >&2
    exit 1
fi
