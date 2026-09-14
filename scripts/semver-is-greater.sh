#!/bin/sh
set -eu

if [ "$#" -ne 2 ]; then
    echo "usage: semver-is-greater.sh <candidate> <baseline>" >&2
    exit 2
fi

awk -v candidate="$1" -v baseline="$2" '
function valid_identifiers(value, reject_numeric_leading_zero, parts, count, idx) {
    if (value == "") return 0
    count = split(value, parts, ".")
    for (idx = 1; idx <= count; idx++) {
        if (parts[idx] !~ /^[0-9A-Za-z-]+$/) return 0
        if (reject_numeric_leading_zero && parts[idx] ~ /^[0-9]+$/ && length(parts[idx]) > 1 && substr(parts[idx], 1, 1) == "0") {
            return 0
        }
    }
    return 1
}

function valid_semver(value, core_and_pre, build, prerelease, plus, dash, core_parts, count, idx) {
    plus = index(value, "+")
    if (plus) {
        build = substr(value, plus + 1)
        core_and_pre = substr(value, 1, plus - 1)
        if (index(build, "+") || !valid_identifiers(build, 0)) return 0
    } else {
        core_and_pre = value
    }

    dash = index(core_and_pre, "-")
    if (dash) {
        prerelease = substr(core_and_pre, dash + 1)
        core_and_pre = substr(core_and_pre, 1, dash - 1)
        if (!valid_identifiers(prerelease, 1)) return 0
    }

    count = split(core_and_pre, core_parts, ".")
    if (count != 3) return 0
    for (idx = 1; idx <= count; idx++) {
        if (core_parts[idx] !~ /^[0-9]+$/) return 0
        if (length(core_parts[idx]) > 1 && substr(core_parts[idx], 1, 1) == "0") return 0
    }
    return 1
}

function trim_zeroes(value) {
    sub(/^0+/, "", value)
    return value == "" ? "0" : value
}

function compare_digits(left, right, idx, left_digit, right_digit) {
    left = trim_zeroes(left)
    right = trim_zeroes(right)
    if (length(left) != length(right)) {
        return length(left) < length(right) ? -1 : 1
    }
    for (idx = 1; idx <= length(left); idx++) {
        left_digit = substr(left, idx, 1)
        right_digit = substr(right, idx, 1)
        if (left_digit != right_digit) {
            return left_digit < right_digit ? -1 : 1
        }
    }
    return 0
}

function compare_prerelease(left, right, left_parts, right_parts, left_count, right_count, count, idx, result, left_numeric, right_numeric) {
    if (left == right) return 0
    if (left == "") return 1
    if (right == "") return -1

    left_count = split(left, left_parts, ".")
    right_count = split(right, right_parts, ".")
    count = left_count < right_count ? left_count : right_count
    for (idx = 1; idx <= count; idx++) {
        if (("x" left_parts[idx]) == ("x" right_parts[idx])) continue
        left_numeric = left_parts[idx] ~ /^[0-9]+$/
        right_numeric = right_parts[idx] ~ /^[0-9]+$/
        if (left_numeric && right_numeric) {
            result = compare_digits(left_parts[idx], right_parts[idx])
            if (result != 0) return result
        } else if (left_numeric) {
            return -1
        } else if (right_numeric) {
            return 1
        } else {
            return ("x" left_parts[idx]) < ("x" right_parts[idx]) ? -1 : 1
        }
    }
    if (left_count == right_count) return 0
    return left_count < right_count ? -1 : 1
}

function compare(left, right, left_core, right_core, left_pre, right_pre, left_dash, right_dash, left_parts, right_parts, idx, result) {
    if (!valid_semver(left) || !valid_semver(right)) exit 2
    sub(/\+.*/, "", left)
    sub(/\+.*/, "", right)
    left_dash = index(left, "-")
    right_dash = index(right, "-")
    left_pre = left_dash ? substr(left, left_dash + 1) : ""
    right_pre = right_dash ? substr(right, right_dash + 1) : ""
    left_core = left_dash ? substr(left, 1, left_dash - 1) : left
    right_core = right_dash ? substr(right, 1, right_dash - 1) : right

    split(left_core, left_parts, ".")
    split(right_core, right_parts, ".")
    for (idx = 1; idx <= 3; idx++) {
        result = compare_digits(left_parts[idx], right_parts[idx])
        if (result != 0) return result
    }
    return compare_prerelease(left_pre, right_pre)
}

BEGIN {
    exit compare(candidate, baseline) > 0 ? 0 : 1
}
' </dev/null
