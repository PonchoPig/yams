#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)

fail() {
  printf '%s\n' "package contract: $*" >&2
  exit 1
}

TMP=$(mktemp -d "${TMPDIR:-/tmp}/yams-package-test.XXXXXX")

cleanup() {
  status=$?
  trap - 0 HUP INT TERM
  rm -rf -- "$TMP"
  exit "$status"
}
trap cleanup 0
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

[ -x /usr/bin/python3 ] || fail '/usr/bin/python3 is required for this test'

capability_version_matches() {
  CAPABILITIES_JSON=$1 EXPECTED_VERSION=$2 /usr/bin/python3 - <<'EOF'
import json
import os
import sys

def fail(message):
    print(f"capability version validation: {message}", file=sys.stderr)
    raise SystemExit(1)

try:
    expected_version = os.environ["EXPECTED_VERSION"]
    raw_capabilities = os.environ["CAPABILITIES_JSON"]
except KeyError:
    fail("validation input is missing")
try:
    document = json.loads(raw_capabilities)
except (TypeError, ValueError):
    fail("capabilities are not valid JSON")
if not isinstance(document, dict):
    fail("top-level JSON value must be an object")
if not isinstance(document.get("yams_version"), str):
    fail("top-level yams_version must be a string")
if document["yams_version"] != expected_version:
    fail("top-level yams_version does not match the expected version")
EOF
}

capability_version_matches '{"yams_version":"1.2.3"}' 1.2.3 \
  || fail 'capability version parser must accept aligned JSON'
for BAD_CAPABILITIES in \
  '{"yams_version":"1.2.4"}' \
  '{"metadata":{"yams_version":"1.2.3"}}' \
  '{"yams_version":123}' \
  '{"yams_version":'
do
  if capability_version_matches "$BAD_CAPABILITIES" 1.2.3 2>/dev/null; then
    fail 'capability version parser accepted adversarial JSON'
  fi
done

# Give the packaging script its own scratch dist/ so this test never touches
# the repository's real dist/.
DIST="$TMP/dist"
export YAMS_PACKAGE_DIST="$DIST"

printf '%s\n' '# Fictional release notes used only by the packaging test.' \
  >"$TMP/notes.md"

STDERR="$TMP/stderr"
USAGE='package: usage: package-rust-release.sh X.Y.Z NOTES_FILE'

# Usage errors are configuration failures (exit 2) with pinned diagnostics.
status=0
"$ROOT/scripts/package-rust-release.sh" >/dev/null 2>"$STDERR" || status=$?
[ "$status" -eq 2 ] || fail 'missing arguments must exit 2'
[ "$(cat "$STDERR")" = "$USAGE" ] \
  || fail 'missing arguments must print the usage diagnostic'

status=0
"$ROOT/scripts/package-rust-release.sh" not-a-version "$TMP/notes.md" \
  >/dev/null 2>"$STDERR" || status=$?
[ "$status" -eq 2 ] || fail 'malformed version must exit 2'
[ "$(cat "$STDERR")" = "$USAGE" ] \
  || fail 'malformed version must print the usage diagnostic'

status=0
"$ROOT/scripts/package-rust-release.sh" 0.0.0 "$TMP/does-not-exist.md" \
  >/dev/null 2>"$STDERR" || status=$?
[ "$status" -eq 2 ] || fail 'missing notes file must exit 2'
[ "$(cat "$STDERR")" = "package: release notes file not found: $TMP/does-not-exist.md" ] \
  || fail 'missing notes file must print the usage diagnostic'

# Malicious/malformed version strings must be rejected as configuration
# errors, not used to build paths under dist/.
for BAD_VERSION in '0/../../tmp/evil.0.0' '1x.2y.3z'; do
  status=0
  "$ROOT/scripts/package-rust-release.sh" "$BAD_VERSION" "$TMP/notes.md" \
    >/dev/null 2>"$STDERR" || status=$?
  [ "$status" -eq 2 ] || fail "malformed version '$BAD_VERSION' must exit 2"
  [ "$(cat "$STDERR")" = "$USAGE" ] \
    || fail "malformed version '$BAD_VERSION' must print the usage diagnostic"
done

(cd "$ROOT" && cargo metadata --locked --offline --format-version 1 --no-deps) \
  >"$TMP/cargo-metadata.json"
ACTUAL_VERSION=$(
  /usr/bin/python3 - \
    "$TMP/cargo-metadata.json" \
    "$ROOT/crates/yams-wiki/Cargo.toml" <<'EOF'
import json, sys

def fail(message):
    print(f"package metadata: {message}", file=sys.stderr)
    raise SystemExit(1)

try:
    with open(sys.argv[1]) as metadata_file:
        document = json.load(metadata_file)
except (OSError, ValueError):
    fail("cargo metadata output is not valid JSON")

packages = document.get("packages") if isinstance(document, dict) else None
if not isinstance(packages, list):
    fail("cargo metadata packages must be an array")
matches = [
    package
    for package in packages
    if isinstance(package, dict) and package.get("manifest_path") == sys.argv[2]
]
if len(matches) != 1:
    fail("expected exactly one yams-wiki manifest entry")
version = matches[0].get("version")
if not isinstance(version, str):
    fail("yams-wiki manifest version must be a string")
print(version)
EOF
) || fail 'could not determine yams-wiki package version'
case $ACTUAL_VERSION in
  '' | *[!0-9.]* | .* | *. | *..* | *.*.*.*)
    fail 'yams-wiki package version must be a numeric triplet'
    ;;
esac
case $ACTUAL_VERSION in
  *.*.*) ;;
  *) fail 'yams-wiki package version must be a numeric triplet' ;;
esac

if [ "$ACTUAL_VERSION" = 0.0.0 ]; then
  MISMATCH_VERSION=0.0.1
else
  MISMATCH_VERSION=0.0.0
fi
mkdir -p "$DIST"
SENTINEL="$DIST/version-mismatch-sentinel"
printf '%s\n' 'must survive version mismatch' >"$SENTINEL"
status=0
"$ROOT/scripts/package-rust-release.sh" "$MISMATCH_VERSION" "$TMP/notes.md" \
  >/dev/null 2>"$STDERR" || status=$?
[ "$status" -eq 2 ] || fail 'mismatched version must exit 2'
[ "$(tail -n 1 "$STDERR")" = \
  "package: requested version $MISMATCH_VERSION does not match staged yams-wiki version $ACTUAL_VERSION" ] \
  || fail 'mismatched version must print the version diagnostic'
[ -f "$SENTINEL" ] || fail 'version mismatch must not recreate dist'

"$ROOT/scripts/package-rust-release.sh" "$ACTUAL_VERSION" "$TMP/notes.md"

NAME="yams-$ACTUAL_VERSION-aarch64-apple-darwin"
[ -f "$DIST/$NAME.tar.gz" ] || fail 'tarball missing'
[ -f "$DIST/SHA256SUMS" ] || fail 'SHA256SUMS missing'
[ -f "$DIST/$NAME.cdx.json" ] || fail 'SBOM missing'

PACKAGED_WIKI="$DIST/$NAME/yams-wiki"
if ! PACKAGED_VERSION_OUTPUT=$("$PACKAGED_WIKI" --version 2>/dev/null); then
  fail 'packaged yams-wiki --version failed'
fi
[ "$PACKAGED_VERSION_OUTPUT" = "yams-wiki $ACTUAL_VERSION" ] \
  || fail 'packaged yams-wiki --version does not match the package version'
if ! PACKAGED_CAPABILITIES=$("$PACKAGED_WIKI" capabilities --json 2>/dev/null); then
  fail 'packaged yams-wiki capabilities command failed'
fi
capability_version_matches "$PACKAGED_CAPABILITIES" "$ACTUAL_VERSION" \
  || fail 'packaged yams-wiki capabilities version is invalid'

tar -tzf "$DIST/$NAME.tar.gz" | LC_ALL=C sort >"$TMP/actual.list"
LC_ALL=C sort >"$TMP/expected.list" <<EOF
$NAME/
$NAME/LICENSE-APACHE
$NAME/LICENSE-MIT
$NAME/RELEASE-NOTES.md
$NAME/memory-search
$NAME/yams
$NAME/yams-service
$NAME/yams-wiki
EOF
cmp -s "$TMP/actual.list" "$TMP/expected.list" || {
  diff "$TMP/expected.list" "$TMP/actual.list" >&2 || true
  fail 'tarball contents differ from the expected file list'
}

ARCHIVE="$TMP/archive"
mkdir -p "$ARCHIVE"
tar -xzf "$DIST/$NAME.tar.gz" -C "$ARCHIVE"
"$ROOT/scripts/test-yams-release-brand.sh" \
  "$ARCHIVE/$NAME/yams" \
  "$ARCHIVE/$NAME/memory-search" \
  "$ARCHIVE/$NAME/yams-service" \
  "$ARCHIVE/$NAME/yams-wiki" \
  || fail 'packaged executables contain retired product bytes'

# The four staged binaries must retain their executable mode in the archive.
tar -tvzf "$DIST/$NAME.tar.gz" >"$TMP/actual.modes"
for BIN in yams memory-search yams-service yams-wiki; do
  grep -E "^-rwxr-xr-x .* $NAME/$BIN\$" "$TMP/actual.modes" >/dev/null \
    || fail "$BIN is not packaged with mode -rwxr-xr-x"
done

(cd "$DIST" && shasum -a 256 -c SHA256SUMS >/dev/null) \
  || fail 'SHA256SUMS does not verify'

# SHA256SUMS must describe exactly the tarball and the four staged binaries.
awk '{ print $2 }' "$DIST/SHA256SUMS" | LC_ALL=C sort >"$TMP/actual.sums"
LC_ALL=C sort >"$TMP/expected.sums" <<EOF
$NAME.tar.gz
$NAME/memory-search
$NAME/yams
$NAME/yams-service
$NAME/yams-wiki
EOF
cmp -s "$TMP/actual.sums" "$TMP/expected.sums" || {
  diff "$TMP/expected.sums" "$TMP/actual.sums" >&2 || true
  fail 'SHA256SUMS lists a different set of files than expected'
}

/usr/bin/python3 - "$DIST/$NAME.cdx.json" "$ACTUAL_VERSION" <<'EOF' \
  || fail 'SBOM package evidence is invalid'
import json, sys

def fail(message):
    print(f"SBOM validation: {message}", file=sys.stderr)
    raise SystemExit(1)

try:
    with open(sys.argv[1]) as sbom_file:
        document = json.load(sbom_file)
except (OSError, ValueError):
    fail("document is not valid JSON")

if not isinstance(document, dict) or document.get("bomFormat") != "CycloneDX":
    fail("document is not CycloneDX")
components = document.get("components")
if not isinstance(components, list) or not components:
    fail("components must be a non-empty array")
matches = [
    component
    for component in components
    if isinstance(component, dict)
    and component.get("name") == "yams-wiki"
    and component.get("type") == "library"
    and isinstance(component.get("bom-ref"), str)
    and component["bom-ref"].startswith("CycloneDxRef-Component-yams-wiki-")
]
if len(matches) != 1:
    fail("expected exactly one first-party yams-wiki component")
if matches[0].get("version") != sys.argv[2]:
    fail("yams-wiki component version does not match the package version")
EOF

printf '%s\n' 'package contract passed'
