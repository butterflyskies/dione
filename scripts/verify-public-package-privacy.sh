#!/bin/sh
set -eu

marker_file=
case $# in
    1)
        archive=$1
        ;;
    3)
        if [ "$1" != "--forbidden-markers" ]; then
            echo "usage: $0 [--forbidden-markers <file>] <crate-archive>" >&2
            exit 2
        fi
        marker_file=$2
        archive=$3
        ;;
    *)
        echo "usage: $0 [--forbidden-markers <file>] <crate-archive>" >&2
        exit 2
        ;;
esac

if [ ! -f "${archive}" ]; then
    echo "crate archive does not exist" >&2
    exit 2
fi
if [ -n "${marker_file}" ] && { [ ! -f "${marker_file}" ] || [ ! -s "${marker_file}" ] || [ ! -r "${marker_file}" ]; }; then
    echo "forbidden-marker file must exist and be non-empty" >&2
    exit 2
fi

scratch_parent=${TMPDIR:-$(dirname "${archive}")}
scratch=$(mktemp -d "${scratch_parent%/}/public-package-privacy.XXXXXX")
trap 'rm -rf "${scratch}"' EXIT HUP INT TERM
extracted=${scratch}/extracted
patterns=${scratch}/patterns
member_patterns=${scratch}/member-patterns
markers=${scratch}/markers
violations=${scratch}/violations
mkdir "${extracted}"
: > "${markers}"
: > "${violations}"

if [ -n "${marker_file}" ]; then
    carriage_return=$(printf '\r')
    while IFS= read -r marker || [ -n "${marker}" ]; do
        marker=${marker%"${carriage_return}"}
        case ${marker} in
            *[![:space:]]*) ;;
            *)
                echo "forbidden-marker file contains a blank marker" >&2
                exit 2
                ;;
        esac
        printf '%s\n' "${marker}" >> "${markers}"
    done < "${marker_file}"
fi

private_dependency=$(printf '%s%s' cingu late)
negative_canary_long=$(printf '%s%s' Mir anda)
negative_canary_short=$(printf '%s%s' Mi ra)
if ! sed \
    -e "s/{PRIVATE_DEP}/${private_dependency}/g" \
    -e "s/{CANARY_LONG}/${negative_canary_long}/g" \
    -e "s/{CANARY_SHORT}/${negative_canary_short}/g" \
    scripts/public-package-structural-patterns.txt > "${patterns}"; then
    echo "public package scan failed: rule=pattern-read-error" >&2
    exit 1
fi
if ! sed \
    -e "s/{PRIVATE_DEP}/${private_dependency}/g" \
    scripts/public-package-member-patterns.txt > "${member_patterns}"; then
    echo "public package scan failed: rule=member-pattern-read-error" >&2
    exit 1
fi

if ! tar -xzf "${archive}" -C "${extracted}" 2>/dev/null; then
    echo "public package scan failed: rule=archive-read-error" >&2
    exit 1
fi

if ! find "${extracted}" -mindepth 1 -exec sh -c '
    violations=$1
    patterns=$2
    member_patterns=$3
    markers=$4
    extracted=$5
    shift 5
    tab=$(printf "\t")

    record_violation() {
        if ! printf "%s\n" "$1" >> "${violations}"; then
            exit 10
        fi
    }

    grep_name_regex() {
        pattern=$1
        LC_ALL=C grep -Eqi -- "${pattern}" "${name_probe}" 2>/dev/null
    }

    grep_name_fixed() {
        marker=$1
        LC_ALL=C grep -Fqi -- "${marker}" "${name_probe}" 2>/dev/null
    }

    for file do
        relative=${file#"${extracted}"/}
        name_probe=${violations}.name.$$
        if ! printf "%s" "${relative}" > "${name_probe}"; then
            exit 11
        fi

        while IFS="${tab}" read -r rule pattern; do
            if grep_name_regex "${pattern}"; then
                record_violation "structural-name:${rule}"
            else
                status=$?
                if [ "${status}" -ne 1 ]; then
                    exit 12
                fi
            fi
        done < "${patterns}" || exit $?

        while IFS="${tab}" read -r rule pattern; do
            if grep_name_regex "${pattern}"; then
                record_violation "structural-member:${rule}"
            else
                status=$?
                if [ "${status}" -ne 1 ]; then
                    exit 17
                fi
            fi
        done < "${member_patterns}" || exit $?

        while IFS= read -r marker || [ -n "${marker}" ]; do
            if grep_name_fixed "${marker}"; then
                record_violation "external-name"
            else
                status=$?
                if [ "${status}" -ne 1 ]; then
                    exit 13
                fi
            fi
        done < "${markers}" || exit $?

        if [ ! -f "${file}" ] || [ -L "${file}" ]; then
            continue
        fi

        if LC_ALL=C grep -Iq . "${file}" 2>/dev/null; then
            :
        else
            status=$?
            if [ "${status}" -eq 1 ]; then
                continue
            fi
            exit 14
        fi

        while IFS="${tab}" read -r rule pattern; do
            if LC_ALL=C grep -Eqi -- "${pattern}" "${file}" 2>/dev/null; then
                record_violation "structural-content:${rule}"
            else
                status=$?
                if [ "${status}" -ne 1 ]; then
                    exit 15
                fi
            fi
        done < "${patterns}" || exit $?

        while IFS= read -r marker || [ -n "${marker}" ]; do
            if LC_ALL=C grep -Fqi -- "${marker}" "${file}" 2>/dev/null; then
                record_violation "external-content"
            else
                status=$?
                if [ "${status}" -ne 1 ]; then
                    exit 16
                fi
            fi
        done < "${markers}" || exit $?
    done
' scan "${violations}" "${patterns}" "${member_patterns}" "${markers}" "${extracted}" {} + 2>/dev/null; then
    echo "public package scan failed: rule=file-scan-error" >&2
    exit 1
fi

if [ -s "${violations}" ]; then
    report=${scratch}/report
    if ! LC_ALL=C sort -u "${violations}" -o "${report}"; then
        echo "public package scan failed: rule=report-error" >&2
        exit 1
    fi
    while IFS= read -r rule; do
        echo "public package contains forbidden private material: rule=${rule}" >&2
    done < "${report}"
    exit 1
fi
