#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)
OUTPUT=$(mktemp "${TMPDIR:-/tmp}/yams-release-contract.XXXXXX")

cleanup() {
  rm -f -- "$OUTPUT"
}
trap cleanup 0
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

status=0
(
  unset YAMS_TEST_JINA_EXPECTED_SIGNATURE
  unset YAMS_TEST_JINA_EXPECTED_QUERY_SHA256
  unset YAMS_RELEASE_TEST_ALLOW_NET
  YAMS_TEST_JINA_MODEL_CACHE=/fictional/pinned-cache
  export YAMS_TEST_JINA_MODEL_CACHE
  exec "$ROOT/scripts/test-rust-release.sh"
) >"$OUTPUT" 2>&1 || status=$?

if [ "$status" -ne 2 ]; then
  printf '%s\n' "expected partial pinned-model configuration to exit 2, got $status" >&2
  sed -n '1,40p' "$OUTPUT" >&2
  exit 1
fi

expected='release smoke: set all three YAMS_TEST_JINA_* variables or none of them'
actual=$(sed -n '1p' "$OUTPUT")
if [ "$actual" != "$expected" ] || [ "$(wc -l <"$OUTPUT" | tr -d ' ')" -ne 1 ]; then
  printf '%s\n' "unexpected partial pinned-model diagnostic" >&2
  sed -n '1,40p' "$OUTPUT" >&2
  exit 1
fi

service_client_block=$(sed -n \
  "/expect_status 0 'service-backed release query'/,/require_output.*service-backed release query/p" \
  "$ROOT/scripts/test-rust-release.sh")
if printf '%s\n' "$service_client_block" | grep -F 'YAMS_DIRS=' >/dev/null; then
  printf '%s\n' 'service-backed query must not force direct routing with YAMS_DIRS' >&2
  exit 1
fi
if printf '%s\n' "$service_client_block" | grep -F 'YAMS_HOME=' >/dev/null; then
  printf '%s\n' 'service-backed query must not force direct routing with YAMS_HOME' >&2
  exit 1
fi

if ! grep -F -- '--ignored --exact --list' "$ROOT/scripts/test-rust-release.sh" >/dev/null; then
  printf '%s\n' 'pinned Jina contract must prove its ignored test still exists' >&2
  exit 1
fi

for build_flag in '--target-dir' '--target'; do
  if ! grep -F -- "$build_flag" "$ROOT/scripts/build-rust-release.sh" >/dev/null; then
    printf '%s\n' "release build must pin Cargo $build_flag" >&2
    exit 1
  fi
done

printf '%s\n' 'release smoke contract passed'
