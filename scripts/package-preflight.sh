#!/bin/sh
# Shared privacy, ownership, registry comparison and output contract.
set -eu
if [ "$#" -lt 7 ]; then
    echo "usage: package-preflight.sh package version archive comparison mode output-file output-key [owners...]" >&2
    exit 2
fi
package=$1 version=$2 archive=$3 comparison=$4 mode=$5 output=$6 key=$7
shift 7
case "$comparison" in exact|--ignore-vcs-info) ;; *) exit 2 ;; esac
case "$mode" in verify|upload|reconcile) ;; *) exit 2 ;; esac
sh scripts/verify-public-package-privacy.sh "$archive"
if [ "$mode" != reconcile ]; then
    sh scripts/verify-crates-io-owners.sh "$package" "$@"
fi
if [ "$comparison" = exact ]; then
    state=$(sh scripts/crates-io-package-state.sh "$package" "$version" "$archive")
else
    state=$(sh scripts/crates-io-package-state.sh "$package" "$version" "$archive" --ignore-vcs-info)
fi
if [ "$mode" = reconcile ]; then
    test "$state" = published
    sh scripts/verify-crates-io-owners.sh "$package" "$@"
else
    case "$state" in
        published) printf '%s=false\n' "$key" >> "$output" ;;
        missing) printf '%s=true\n' "$key" >> "$output" ;;
        *) echo "unexpected registry package state" >&2; exit 2 ;;
    esac
fi
