#!/bin/sh
# Reject tracked files that name a known person, agent or construct.
#
# The identity list (names, handles, Discord IDs; one per line) is supplied at run time from a CI
# secret and never committed, so this check does not itself become a roster. Matching is
# case-insensitive, against file contents and tracked paths, and bounded by any character that is
# not a letter or digit, so `quillon_scope` and `quillon-construct` match `quillon` but `quillonish`
# does not. Output names only
# `path:line` (or `path` for a path hit) and a count. The matched entry is never printed.
#
# usage: scan-known-identities.sh --markers <file> [--allow <file>]
# The allow file lists tracked paths exempt from the scan, one per line; `#` lines are ignored.
# Exit status: 0 clean, 1 identities found, 2 usage or input error.
set -eu

usage() {
    echo "usage: $0 --markers <file> [--allow <file>]" >&2
    exit 2
}

markers=
allow=scripts/known-identities-allow.txt
while [ "$#" -gt 0 ]; do
    case $1 in
        --markers) [ "$#" -ge 2 ] || usage; markers=$2; shift 2 ;;
        --allow) [ "$#" -ge 2 ] || usage; allow=$2; shift 2 ;;
        *) usage ;;
    esac
done
[ -n "${markers}" ] || usage
if [ ! -f "${markers}" ] || [ ! -r "${markers}" ]; then
    echo "identity list must be a readable file" >&2
    exit 2
fi

scratch=$(mktemp -d "${TMPDIR:-/tmp}/known-identities.XXXXXX")
trap 'rm -rf "${scratch}"' EXIT HUP INT TERM
patterns=${scratch}/patterns
hits=${scratch}/hits

# Trim each entry and drop blank lines: a pasted secret often pads entries with spaces, and an
# untrimmed `name ` would silently match nothing, while an empty line would match everything.
tr -d '\r' < "${markers}" | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//' | grep -v '^$' > "${patterns}" || true
if [ ! -s "${patterns}" ]; then
    echo "identity list is empty" >&2
    exit 2
fi
# Escape each entry for ERE and wrap it in letter-or-digit boundaries. Underscore counts as a
# boundary (unlike grep -w) because names appear inside snake_case identifiers.
regex=${scratch}/regex
sed -e 's/[][\\.*^$+?(){}|]/\\&/g' -e 's/^/(^|[^[:alnum:]])(/' -e 's/$/)([^[:alnum:]]|$)/' "${patterns}" > "${regex}"

# Pathspecs: the whole tree minus allowed paths, passed as arguments.
set -- .
if [ -f "${allow}" ]; then
    tr -d '\r' < "${allow}" | grep -v -e '^[[:space:]]*$' -e '^#' > "${scratch}/allow" || true
    while IFS= read -r path; do
        set -- "$@" ":(exclude,literal)${path}"
    done < "${scratch}/allow"
fi

# Contents. Each command writes to a file and its status is checked on its own; a pipeline would
# report only its last command, and a failed search must never read as "clean". `-z` separates
# path, line number and text with NUL, so a path containing `:` cannot shift the fields.
raw=${scratch}/raw
set +e
git grep -z -n -I -i -E -f "${regex}" -- "$@" > "${raw}"
status=$?
set -e
if [ "${status}" -ne 0 ] && [ "${status}" -ne 1 ]; then
    echo "known-identity scan failed: content search error" >&2
    exit 2
fi
LC_ALL=C tr '\000' '\001' < "${raw}" | LC_ALL=C cut -d "$(printf '\001')" -f 1,2 | LC_ALL=C tr '\001' ':' > "${hits}"

# Paths: a tracked file whose own name carries an identity.
if ! git ls-files -z -- "$@" > "${raw}"; then
    echo "known-identity scan failed: path listing error" >&2
    exit 2
fi
set +e
LC_ALL=C tr '\000' '\n' < "${raw}" | LC_ALL=C grep -i -E -f "${regex}" > "${scratch}/paths"
status=$?
set -e
if [ "${status}" -gt 1 ]; then
    echo "known-identity scan failed: path search error" >&2
    exit 2
fi
sed 's/$/ (path)/' "${scratch}/paths" >> "${hits}"

if [ -s "${hits}" ]; then
    count=$(wc -l < "${hits}" | tr -d ' ')
    LC_ALL=C sort -u "${hits}" | sed 's/^/known identity: /' >&2
    echo "known-identity scan: ${count} hit(s); replace each with an invented name, handle or ID" >&2
    exit 1
fi
echo "known-identity scan: clean"
