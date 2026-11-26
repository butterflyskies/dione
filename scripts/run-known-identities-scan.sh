#!/bin/sh
# Admit the run-time identity roster from the environment and run scan-known-identities.sh on it.
#
# usage: run-known-identities-scan.sh pull-request|publish
#   KNOWN_IDENTITIES  the roster, one entry per line (a CI secret)
#   FROM_FORK         "true" when the pull request comes from a fork (pull-request mode only)
#
# pull-request: a fork PR never receives secrets, so a missing roster there skips with exit 0.
#               Anywhere else a missing roster is a configuration fault: exit 2.
# publish:      a missing roster is always exit 2; publication must not proceed unscanned.
# Otherwise the scan's own status is returned (0 clean, 1 identities found, 2 scan failed). The
# roster is written to a private temporary file that is removed on every exit, error or signal.
set -eu

mode=${1:-}
case ${mode} in
    pull-request|publish) ;;
    *) echo "usage: $0 pull-request|publish" >&2; exit 2 ;;
esac

if [ -z "${KNOWN_IDENTITIES:-}" ]; then
    if [ "${mode}" = pull-request ] && [ "${FROM_FORK:-}" = true ]; then
        echo "KNOWN_IDENTITIES is not available to fork pull requests; identity scan skipped"
        exit 0
    fi
    echo "KNOWN_IDENTITIES is not configured; identity scan cannot run" >&2
    exit 2
fi

umask 077
if ! list=$(mktemp); then
    echo "identity scan failed: cannot create a private roster file" >&2
    exit 2
fi
trap 'rm -f "${list}"' EXIT
trap 'exit 2' HUP INT TERM
if ! printf '%s\n' "${KNOWN_IDENTITIES}" > "${list}"; then
    echo "identity scan failed: cannot write the roster file" >&2
    exit 2
fi
set +e
sh "$(dirname "$0")/scan-known-identities.sh" --markers "${list}"
status=$?
set -e
exit "${status}"
