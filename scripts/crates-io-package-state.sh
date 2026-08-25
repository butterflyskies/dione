#!/bin/sh
set -eu

if [ "$#" -lt 3 ] || [ "$#" -gt 4 ]; then
    echo "usage: crates-io-package-state.sh <crate> <version> <crate-file> [--ignore-vcs-info]" >&2
    exit 64
fi

crate_name="$1"
crate_version="$2"
crate_file="$3"
comparison="${4:-exact}"

if [ "${comparison}" != exact ] && [ "${comparison}" != --ignore-vcs-info ]; then
    echo "unknown comparison mode: ${comparison}" >&2
    exit 64
fi

if [ ! -f "${crate_file}" ]; then
    echo "local crate archive does not exist: ${crate_file}" >&2
    exit 1
fi

scratch_parent="${CARGO_TARGET_DIR:-target}"
mkdir -p "${scratch_parent}"
response="$(mktemp "${scratch_parent}/crate-state.XXXXXX")"
trap 'rm -f "${response}"' EXIT HUP INT TERM

url="https://crates.io/api/v1/crates/${crate_name}/${crate_version}"
status="$(curl --silent --show-error --connect-timeout 5 --max-time 20 \
    --user-agent 'dione-release-workflow/1.0 (https://github.com/butterflyskies/dione)' \
    --output "${response}" --write-out '%{http_code}' "${url}")"

case "${status}" in
    200)
        remote_checksum="$(python3 -c \
            'import json, sys; print(json.load(sys.stdin)["version"]["checksum"])' \
            < "${response}")"
        local_checksum="$(sha256sum "${crate_file}")"
        local_checksum="${local_checksum%% *}"
        if [ "${remote_checksum}" != "${local_checksum}" ]; then
            if [ "${comparison}" != --ignore-vcs-info ]; then
                echo "published ${crate_name} ${crate_version} does not match local package bytes" >&2
                echo "registry: ${remote_checksum}" >&2
                echo "local:    ${local_checksum}" >&2
                exit 1
            fi

            remote_crate="$(mktemp "${scratch_parent}/remote-crate.XXXXXX")"
            trap 'rm -f "${response}" "${remote_crate}"' EXIT HUP INT TERM
            curl --fail --silent --show-error --location \
                --connect-timeout 5 --max-time 60 \
                --user-agent 'dione-release-workflow/1.0 (https://github.com/butterflyskies/dione)' \
                --output "${remote_crate}" \
                "https://crates.io/api/v1/crates/${crate_name}/${crate_version}/download"
            if ! scripts/compare-crate-contents.sh "${crate_file}" "${remote_crate}"; then
                echo "published ${crate_name} ${crate_version} does not match local package contents" >&2
                exit 1
            fi
        fi
        echo published
        ;;
    404)
        echo missing
        ;;
    *)
        echo "crates.io returned HTTP ${status} for ${crate_name} ${crate_version}" >&2
        exit 1
        ;;
esac
