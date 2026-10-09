#!/bin/sh
set -eu

if [ "$#" -ne 8 ]; then
    echo "expected all eight required check results, got $#" >&2
    exit 1
fi

for name in hygiene format lint test package msrv build audit; do
    check=$1
    shift
    case "$check" in
        "$name=success") ;;
        *)
            echo "required check did not succeed: $check" >&2
            exit 1
            ;;
    esac
done
