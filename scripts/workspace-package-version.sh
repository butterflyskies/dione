#!/bin/sh
set -eu

if [ "$#" -ne 1 ]; then
    echo "usage: workspace-package-version.sh <package>" >&2
    exit 64
fi

package_id="$(cargo pkgid -p "$1")"
version="${package_id##*#}"
version="${version##*@}"

case "${version}" in
    ''|*[!0-9A-Za-z.+-]*)
        echo "could not extract a package version from: ${package_id}" >&2
        exit 1
        ;;
esac

printf '%s\n' "${version}"
