#!/bin/sh
# Reject tracked files that name a known person, agent or construct.
#
# The identity list (names, handles, Discord IDs; one per line) is supplied at run time from a CI
# secret and never committed, so this check does not itself become a roster. Matching is
# case-insensitive, against file contents and tracked paths, and bounded by any character that is
# not a letter or digit, so `quillon_scope` and `quillon-construct` match `quillon` but `quillonish`
# does not. Output names only
# `path:line` (or `path` for a path hit) and a count. Matched line text and roster entries are never
# printed; a path hit prints the path, which is repository text and may itself contain the entry.
#
# usage: scan-known-identities.sh --markers <file> [--allow <file>]
# Both searches run under LC_ALL=C.UTF-8, so case folding (including accented letters) is the same
# for file contents and for file paths.
# Choose roster entries that only name people: an entry that is also an ordinary identifier (a crate,
# keyword or common word) matches code that has nothing to do with the person. Prefer full handles and
# Discord IDs over short names that collide.
# The allow file lists exemptions, one per line; blank and `#` lines are ignored. A line `path`
# exempts that whole tracked file. A line `path<TAB>text` exempts only the exact, case-sensitive
# `text` inside that one file: it is removed from each matched line there before the line is
# checked again, so any other identity on the same line, the same text in another file, or the
# text in different case is still reported. Use it for public identifiers that must stay, such as a
# repository URL or organisation slug, never to excuse a person's name. Each such `text` is also
# exempt inside scripts/known-identities-allow.txt, which has to spell it out; the rest of that
# file, its comments included, is still scanned.
# Exit status: 0 clean, 1 identities found, 2 usage or input error.
set -eu

# Deliberate outcomes go through finish(). Any other exit is a failed step under `set -e`, whose own
# status (often 1) would read as "identities found", so the EXIT trap turns it into 2.
outcome=
finish() {
    outcome=$1
    exit "$1"
}

usage() {
    echo "usage: $0 --markers <file> [--allow <file>]" >&2
    finish 2
}

markers=
allow=scripts/known-identities-allow.txt
# Tracked path of the repository's allow file. Each `path<TAB>text` entry's text is also exempt
# inside this file, which must spell it out; nothing else in the file is exempt.
allow_tracked=scripts/known-identities-allow.txt
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
    finish 2
fi

if ! scratch=$(mktemp -d "${TMPDIR:-/tmp}/known-identities.XXXXXX"); then
    echo "known-identity scan failed: cannot create a scratch directory" >&2
    finish 2
fi
trap 'status=$?; rm -rf "${scratch}"; if [ -z "${outcome}" ] && [ "${status}" -ne 0 ]; then echo "known-identity scan failed: a step exited ${status}" >&2; exit 2; fi' EXIT
trap 'exit 2' HUP INT TERM
patterns=${scratch}/patterns
hits=${scratch}/hits

# Trim each entry and drop blank lines: a pasted secret often pads entries with spaces, and an
# untrimmed `name ` would silently match nothing, while an empty line would match everything.
# One command per step, checked by `set -e`: a failed step in a pipeline could keep only part of the
# roster and report clean. grep exits 1 when every line was blank, which the empty check reports.
tr -d '\r' < "${markers}" > "${scratch}/unix"
sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//' "${scratch}/unix" > "${scratch}/trimmed"
set +e
grep -a -v '^$' "${scratch}/trimmed" > "${patterns}"
status=$?
set -e
if [ "${status}" -gt 1 ]; then
    echo "known-identity scan failed: roster read error" >&2
    finish 2
fi
if [ ! -s "${patterns}" ]; then
    echo "identity list is empty" >&2
    finish 2
fi
# Escape each entry for ERE and wrap it in letter-or-digit boundaries. Underscore counts as a
# boundary (unlike grep -w) because names appear inside snake_case identifiers.
regex=${scratch}/regex
sed -e 's/[][\\.*^$+?(){}|]/\\&/g' -e 's/^/(^|[^[:alnum:]])(/' -e 's/$/)([^[:alnum:]]|$)/' "${patterns}" > "${regex}"

# Pathspecs: the whole tree minus whole-file exemptions, passed as arguments. Occurrence exemptions
# (`path<TAB>text`) go to their own file and are applied to the content hits below.
tab=$(printf '\t')
set -- .
: > "${scratch}/allow-occurrences"
if [ -f "${allow}" ]; then
    tr -d '\r' < "${allow}" > "${scratch}/allow-unix"
    set +e
    grep -a -v -e '^[[:space:]]*$' -e '^#' "${scratch}/allow-unix" > "${scratch}/allow"
    status=$?
    set -e
    if [ "${status}" -gt 1 ]; then
        echo "known-identity scan failed: allow list read error" >&2
        finish 2
    fi
    set +e
    grep -a -v -e "${tab}" "${scratch}/allow" > "${scratch}/allow-paths"
    paths_status=$?
    grep -a -e "${tab}" "${scratch}/allow" > "${scratch}/allow-occurrences"
    occurrences_status=$?
    set -e
    if [ "${paths_status}" -gt 1 ] || [ "${occurrences_status}" -gt 1 ]; then
        echo "known-identity scan failed: allow list read error" >&2
        finish 2
    fi
    # An occurrence entry is exactly a non-empty path, one tab and text that is not only whitespace.
    # Anything else (an empty or blank side, a second tab) is refused rather than guessed at.
    if ! LC_ALL=C awk -F "${tab}" 'NF != 2 || $1 == "" || $2 ~ /^[[:space:]]*$/ { bad = 1 } END { exit bad }' \
        "${scratch}/allow-occurrences"; then
        echo "allow list has a malformed path<TAB>text entry" >&2
        finish 2
    fi
    # The tracked allow file necessarily holds each exempt text too. Exempt exactly those texts
    # inside it, never the whole file, so a name in one of its comments is still reported.
    LC_ALL=C awk -F "${tab}" -v tab="${tab}" -v self="${allow_tracked}" '{ print self tab $2 }' \
        "${scratch}/allow-occurrences" > "${scratch}/allow-self"
    cat "${scratch}/allow-self" >> "${scratch}/allow-occurrences"
    while IFS= read -r path; do
        set -- "$@" ":(exclude,literal)${path}"
    done < "${scratch}/allow-paths"
fi

# Tracked paths are framed below by NUL from Git and then by SOH, tab and newline; a path holding
# any of those bytes could borrow another path's exemption, split a record or lose a trailing tab
# to `read`. Such names are refused, not guessed.
if ! git ls-files -z > "${scratch}/tracked"; then
    echo "known-identity scan failed: path listing error" >&2
    finish 2
fi
LC_ALL=C tr -cd '\001\n\t' < "${scratch}/tracked" > "${scratch}/framing"
if [ -s "${scratch}/framing" ]; then
    echo "a tracked path contains a newline, tab or SOH byte; rename it before scanning" >&2
    finish 2
fi

# Contents. Each command writes to a file and its status is checked on its own; a pipeline would
# report only its last command, and a failed search must never read as "clean". `-z` separates
# path, line number and text with NUL, so a path containing `:` cannot shift the fields.
raw=${scratch}/raw
set +e
# --text: a file Git treats as binary (`-diff` in .gitattributes, or a NUL byte) is still searched;
# without it such a file would silently scan clean. --no-textconv searches the stored bytes.
LC_ALL=C.UTF-8 git grep -z -n --text --no-textconv -i -E -f "${regex}" -- "$@" > "${raw}"
status=$?
set -e
if [ "${status}" -ne 0 ] && [ "${status}" -ne 1 ]; then
    echo "known-identity scan failed: content search error" >&2
    finish 2
fi
# Post-processing runs one command per step, each into its own file, so `set -e` sees every
# failure; a failed step in a pipeline would leave an empty hit list that reads as "clean".
sep=$(printf '\001')
LC_ALL=C tr '\000' '\001' < "${raw}" > "${scratch}/fields"
if [ -s "${scratch}/allow-occurrences" ] && [ -s "${scratch}/fields" ]; then
    # Remove every exempt text from the hit lines of its own file (byte-exact, so case-sensitive),
    # replacing it with a space so neighbouring text cannot fuse into a new word. A line nothing was
    # removed from stays a hit as the first search found it. Only changed lines are searched again,
    # and they stay hits when an identity remains, so the re-check can never drop an untouched hit.
    LC_ALL=C awk -v sep="${sep}" -v tab="${tab}" -v stripped="${scratch}/stripped" \
        -v changed="${scratch}/changed" -v kept="${scratch}/kept-records" '
        NR == FNR {
            i = index($0, tab); p = substr($0, 1, i - 1)
            n[p]++; text[p, n[p]] = substr($0, i + 1); next
        }
        {
            i = index($0, sep); p = substr($0, 1, i - 1); rest = substr($0, i + 1)
            j = index(rest, sep); line = substr(rest, j + 1); original = line
            for (k = 1; k <= n[p]; k++) {
                # Copy the line forward, replacing each match and resuming after it, so a
                # replacement can never recreate the text it replaced and loop.
                t = text[p, k]; done_part = ""
                while ((x = index(line, t)) > 0) {
                    done_part = done_part substr(line, 1, x - 1) " "
                    line = substr(line, x + length(t))
                }
                line = done_part line
            }
            if (line == original) { print FNR > kept }
            else { print line > stripped; print FNR > changed }
        }
        END { printf "" > kept; printf "" > stripped; printf "" > changed }
    ' "${scratch}/allow-occurrences" "${scratch}/fields"
    set +e
    # -a: a hit line with a byte that is not valid UTF-8 must stay a line to search, not be dropped
    # as "binary file matches" (GNU grep).
    LC_ALL=C.UTF-8 grep -a -n -i -E -f "${regex}" "${scratch}/stripped" > "${scratch}/remaining"
    status=$?
    set -e
    if [ "${status}" -gt 1 ]; then
        echo "known-identity scan failed: occurrence re-check error" >&2
        finish 2
    fi
    LC_ALL=C cut -d : -f 1 "${scratch}/remaining" > "${scratch}/remaining-lines"
    # Map each still-matching changed line back to its record number, and add it to the kept ones.
    LC_ALL=C awk 'NR == FNR { still[$1]; next } FNR in still' "${scratch}/remaining-lines" \
        "${scratch}/changed" >> "${scratch}/kept-records"
    LC_ALL=C awk 'NR == FNR { keep[$1]; next } FNR in keep' "${scratch}/kept-records" \
        "${scratch}/fields" > "${scratch}/kept"
    mv "${scratch}/kept" "${scratch}/fields"
fi
LC_ALL=C cut -d "${sep}" -f 1,2 "${scratch}/fields" > "${scratch}/located"
LC_ALL=C tr '\001' ':' < "${scratch}/located" > "${hits}"
if [ -s "${scratch}/fields" ] && [ ! -s "${hits}" ]; then
    echo "known-identity scan failed: matches found but not reported" >&2
    finish 2
fi

# Paths: a tracked file whose own name carries an identity.
if ! git ls-files -z -- "$@" > "${raw}"; then
    echo "known-identity scan failed: path listing error" >&2
    finish 2
fi
LC_ALL=C tr '\000' '\n' < "${raw}" > "${scratch}/listed"
set +e
LC_ALL=C.UTF-8 grep -a -i -E -f "${regex}" "${scratch}/listed" > "${scratch}/paths"
status=$?
set -e
if [ "${status}" -gt 1 ]; then
    echo "known-identity scan failed: path search error" >&2
    finish 2
fi
sed 's/$/ (path)/' "${scratch}/paths" >> "${hits}"

# Symlinks: Git stores a link's target as its blob, and neither search above reads it. Read each
# tracked link's stored target without following it, and report the link when the target names
# someone. Paths cannot hold newlines or tabs (refused above), so one record per line is unambiguous.
# Occurrence exemptions for the link's own path apply to its target text, as to a file's lines.
if ! git ls-files -s -z -- "$@" > "${raw}"; then
    echo "known-identity scan failed: link listing error" >&2
    finish 2
fi
LC_ALL=C tr '\000' '\n' < "${raw}" > "${scratch}/staged"
LC_ALL=C awk -v tab="${tab}" '$1 == "120000" { i = index($0, tab); print $2 tab substr($0, i + 1) }' \
    "${scratch}/staged" > "${scratch}/links"
: > "${scratch}/targets"
: > "${scratch}/link-paths"
while IFS="${tab}" read -r oid path; do
    if ! git cat-file blob "${oid}" > "${scratch}/target"; then
        echo "known-identity scan failed: link target read error" >&2
        finish 2
    fi
    # A stored target is one record of its exact bytes. A newline would have to be rewritten to fit
    # one line, and an exemption could then match bytes the target does not hold, so it is refused.
    LC_ALL=C tr -cd '\n' < "${scratch}/target" > "${scratch}/target-newlines"
    if [ -s "${scratch}/target-newlines" ]; then
        echo "a tracked symlink target contains a newline; rewrite the link before scanning" >&2
        finish 2
    fi
    cat "${scratch}/target" >> "${scratch}/targets"
    echo >> "${scratch}/targets"
    printf '%s\n' "${path}" >> "${scratch}/link-paths"
done < "${scratch}/links"
if ! LC_ALL=C awk -v tab="${tab}" -v occurrences="${scratch}/allow-occurrences" \
    -v targets="${scratch}/targets" '
    BEGIN {
        while ((r = (getline entry < occurrences)) > 0) {
            i = index(entry, tab); p = substr(entry, 1, i - 1)
            n[p]++; text[p, n[p]] = substr(entry, i + 1)
        }
        if (r < 0) exit 3
    }
    {
        if ((getline line < targets) <= 0) exit 3
        for (k = 1; k <= n[$0]; k++) {
            t = text[$0, k]; done_part = ""
            while ((x = index(line, t)) > 0) {
                done_part = done_part substr(line, 1, x - 1) " "
                line = substr(line, x + length(t))
            }
            line = done_part line
        }
        print line
    }
' "${scratch}/link-paths" > "${scratch}/targets-exempted"; then
    echo "known-identity scan failed: link target exemption error" >&2
    finish 2
fi
set +e
LC_ALL=C.UTF-8 grep -a -n -i -E -f "${regex}" "${scratch}/targets-exempted" > "${scratch}/target-hits"
status=$?
set -e
if [ "${status}" -gt 1 ]; then
    echo "known-identity scan failed: link target search error" >&2
    finish 2
fi
LC_ALL=C cut -d : -f 1 "${scratch}/target-hits" > "${scratch}/target-lines"
LC_ALL=C awk 'NR == FNR { hit[$1]; next } FNR in hit { print $0 " (symlink target)" }' \
    "${scratch}/target-lines" "${scratch}/link-paths" >> "${hits}"
if [ -s "${scratch}/target-hits" ] && ! grep -a -q ' (symlink target)$' "${hits}"; then
    echo "known-identity scan failed: link target matches found but not reported" >&2
    finish 2
fi

if [ -s "${hits}" ]; then
    LC_ALL=C sort -u "${hits}" > "${scratch}/sorted"
    sed 's/^/known identity: /' "${scratch}/sorted" > "${scratch}/report"
    count=$(wc -l < "${hits}")
    count=$((count + 0))
    cat "${scratch}/report" >&2
    echo "known-identity scan: ${count} hit(s); replace each with an invented name, handle or ID" >&2
    finish 1
fi
echo "known-identity scan: clean"
finish 0
