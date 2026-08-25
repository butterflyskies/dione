#!/bin/sh
set -eu

if [ "$#" -ne 2 ]; then
    echo "usage: compare-crate-contents.sh <local.crate> <remote.crate>" >&2
    exit 64
fi

local_crate="$1"
remote_crate="$2"
scratch_parent="${CARGO_TARGET_DIR:-target}"
mkdir -p "${scratch_parent}"
comparison_root="$(mktemp -d "${scratch_parent}/crate-compare.XXXXXX")"
trap 'rm -rf "${comparison_root}"' EXIT HUP INT TERM

local_tree="${comparison_root}/local"
remote_tree="${comparison_root}/remote"
mkdir "${local_tree}" "${remote_tree}"
tar xzf "${local_crate}" --no-same-owner --no-same-permissions -C "${local_tree}"
tar xzf "${remote_crate}" --no-same-owner --no-same-permissions -C "${remote_tree}"
if [ "$(find "${local_tree}" -mindepth 1 -maxdepth 1 -type d | wc -l)" -ne 1 ] || \
    [ "$(find "${remote_tree}" -mindepth 1 -maxdepth 1 -type d | wc -l)" -ne 1 ]; then
    echo "crate archives must each contain exactly one root directory" >&2
    exit 1
fi
local_root="$(find "${local_tree}" -mindepth 1 -maxdepth 1 -type d)"
remote_root="$(find "${remote_tree}" -mindepth 1 -maxdepth 1 -type d)"
rm -f "${local_root}/.cargo_vcs_info.json" "${remote_root}/.cargo_vcs_info.json"
diff -qr "${local_tree}" "${remote_tree}" > /dev/null
