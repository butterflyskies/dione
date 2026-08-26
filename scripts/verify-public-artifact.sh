#!/bin/sh
set -eu

if [ "$#" -ne 7 ]; then
  echo "usage: $0 BINARY CHECKSUM RECEIPT COMMIT VERSION PACKAGE_INPUTS METADATA" >&2
  exit 2
fi

binary=$1
checksum=$2
receipt=$3
expected_commit=$4
expected_version=$5
package_inputs=$6
metadata=$7

for path in "$binary" "$checksum" "$receipt" "$package_inputs" "$metadata"; do
  if [ ! -f "$path" ] || [ -L "$path" ]; then
    echo "artifact verification input must be a regular, non-symlink file: $path" >&2
    exit 1
  fi
done

if [ ! -x "$binary" ]; then
  echo "release binary is not executable: $binary" >&2
  exit 1
fi

case "$expected_commit" in
  *[!0-9a-f]* | "")
    echo "expected commit is not lowercase hexadecimal" >&2
    exit 1
    ;;
esac
if [ "${#expected_commit}" -ne 40 ]; then
  echo "expected commit must contain exactly 40 hexadecimal characters" >&2
  exit 1
fi
case "$expected_version" in
  *[!0-9A-Za-z.+-]* | "")
    echo "expected version contains unsupported characters" >&2
    exit 1
    ;;
esac

scratch_dir=$(dirname "$metadata")
expected_receipt_file=
expected_checksum_file=
actual_checksum_file=
cleanup() {
  [ -z "$expected_receipt_file" ] || rm -f -- "$expected_receipt_file"
  [ -z "$expected_checksum_file" ] || rm -f -- "$expected_checksum_file"
  [ -z "$actual_checksum_file" ] || rm -f -- "$actual_checksum_file"
}
trap cleanup EXIT HUP INT TERM
expected_receipt_file=$(mktemp "$scratch_dir/.expected-receipt.XXXXXX")
expected_checksum_file=$(mktemp "$scratch_dir/.expected-checksum.XXXXXX")
actual_checksum_file=$(mktemp "$scratch_dir/.actual-checksum.XXXXXX")

printf 'commit=%s\nversion=%s\n' "$expected_commit" "$expected_version" \
  > "$expected_receipt_file"
if ! cmp -s "$expected_receipt_file" "$receipt"; then
  echo "release receipt is not the exact minimal commit/version receipt" >&2
  exit 1
fi

if ! sha256sum "$binary" > "$actual_checksum_file"; then
  echo "release binary hashing failed" >&2
  exit 1
fi
sed "s#  .*#  $(basename "$binary")#" \
  "$actual_checksum_file" > "$expected_checksum_file"
if ! cmp -s "$expected_checksum_file" "$checksum"; then
  echo "release checksum is not bound exactly to the uploaded binary" >&2
  exit 1
fi

# Scan all uploaded bytes case-insensitively for the two authorized bare-name
# regression canaries. Constructing them keeps this verifier compatible with
# the repository-wide source privacy boundary that the artifact job depends on.
canary_long='mir''anda'
canary_short='mi''ra'
canary="(^|[^[:alnum:]_])(${canary_long}|${canary_short})([^[:alnum:]_]|$)"
if LC_ALL=C grep -aEiq "$canary" \
  "$binary" "$checksum" "$receipt" "$package_inputs" "$metadata"; then
  echo "authorized privacy canary found in public artifact" >&2
  exit 1
else
  scan_status=$?
  if [ "$scan_status" -ne 1 ]; then
    echo "authorized privacy canary scan failed" >&2
    exit 1
  fi
fi
