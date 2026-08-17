#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)

fail_configuration() {
  printf '%s\n' "package: $*" >&2
  exit 2
}

VERSION=${1:-}
NOTES=${2:-}

# Version must be exactly three dot-separated numeric fields: no character
# outside [0-9.], no empty/leading/trailing/doubled-dot fields, and no extra
# fields. This also guards the paths built from $VERSION below.
case $VERSION in
  '' | *[!0-9.]* | .* | *. | *..* | *.*.*.*)
    fail_configuration 'usage: package-rust-release.sh X.Y.Z NOTES_FILE'
    ;;
esac
case $VERSION in
  *.*.*) ;;
  *) fail_configuration 'usage: package-rust-release.sh X.Y.Z NOTES_FILE' ;;
esac

[ -n "$NOTES" ] || fail_configuration 'usage: package-rust-release.sh X.Y.Z NOTES_FILE'
[ -f "$NOTES" ] || fail_configuration "release notes file not found: $NOTES"
[ -x /usr/bin/python3 ] || fail_configuration '/usr/bin/python3 is required'
command -v cargo-sbom >/dev/null 2>&1 \
  || fail_configuration 'cargo-sbom is required: cargo install cargo-sbom --locked'

# aarch64-apple-darwin is the only validated release target; the artifact
# name below hard-codes it.
HOST=$(rustc -vV | sed -n 's/^host: //p')
case $HOST in
  aarch64-apple-darwin) ;;
  *)
    fail_configuration \
      "unsupported host '${HOST:-unknown}': aarch64-apple-darwin is the only validated release target"
    ;;
esac

DIST=${YAMS_PACKAGE_DIST:-$ROOT/dist}
# Absolute, and the final path component must be exactly "dist": this is
# the guard in front of `rm -rf -- "$DIST"` below, so a mistyped or
# unset override must never resolve to something broader than a dist/
# directory.
case $DIST in
  /*/dist | /dist) ;;
  *)
    fail_configuration \
      'YAMS_PACKAGE_DIST must be an absolute path ending in /dist'
    ;;
esac

"$ROOT/scripts/build-rust-release.sh"

if ! WIKI_VERSION_OUTPUT=$("$ROOT/libexec/yams-wiki" --version 2>/dev/null); then
  fail_configuration 'could not read staged yams-wiki version'
fi
case $WIKI_VERSION_OUTPUT in
  'yams-wiki '*) STAGED_VERSION=${WIKI_VERSION_OUTPUT#yams-wiki } ;;
  *) fail_configuration 'staged yams-wiki returned an invalid version' ;;
esac
case $STAGED_VERSION in
  '' | *[!0-9.]* | .* | *. | *..* | *.*.*.*)
    fail_configuration 'staged yams-wiki returned an invalid version'
    ;;
esac
case $STAGED_VERSION in
  *.*.*) ;;
  *) fail_configuration 'staged yams-wiki returned an invalid version' ;;
esac
[ "$STAGED_VERSION" = "$VERSION" ] || fail_configuration \
  "requested version $VERSION does not match staged yams-wiki version $STAGED_VERSION"

if ! STAGED_CAPABILITIES=$("$ROOT/libexec/yams-wiki" capabilities --json 2>/dev/null); then
  fail_configuration 'could not read staged yams-wiki capabilities'
fi
if ! CAPABILITIES_JSON="$STAGED_CAPABILITIES" EXPECTED_VERSION="$STAGED_VERSION" \
  /usr/bin/python3 - >/dev/null 2>&1 <<'EOF'
import json
import os
import sys

try:
    expected_version = os.environ["EXPECTED_VERSION"]
    raw_capabilities = os.environ["CAPABILITIES_JSON"]
except KeyError:
    raise SystemExit(1)
try:
    document = json.loads(raw_capabilities)
except (TypeError, ValueError):
    raise SystemExit(1)
if not isinstance(document, dict):
    raise SystemExit(1)
if not isinstance(document.get("yams_version"), str):
    raise SystemExit(1)
if document["yams_version"] != expected_version:
    raise SystemExit(1)
EOF
then
  fail_configuration \
    'staged yams-wiki capabilities must contain a matching top-level yams_version'
fi

NAME="yams-$VERSION-aarch64-apple-darwin"
STAGE="$DIST/$NAME"
# Recreate dist/ fresh each run so it always describes exactly one release:
# SHA256SUMS and the tarball beside it can never describe different versions.
rm -rf -- "$DIST"
mkdir -p -- "$STAGE"

install -m 755 "$ROOT/libexec/yams" "$STAGE/yams"
install -m 755 "$ROOT/libexec/memory-search" "$STAGE/memory-search"
install -m 755 "$ROOT/libexec/yams-service" "$STAGE/yams-service"
install -m 755 "$ROOT/libexec/yams-wiki" "$STAGE/yams-wiki"
install -m 644 "$ROOT/LICENSE-APACHE" "$STAGE/LICENSE-APACHE"
install -m 644 "$ROOT/LICENSE-MIT" "$STAGE/LICENSE-MIT"
install -m 644 "$NOTES" "$STAGE/RELEASE-NOTES.md"

(cd "$ROOT" && cargo sbom --output-format cyclone_dx_json_1_4) \
  >"$DIST/$NAME.cdx.json"

# COPYFILE_DISABLE keeps macOS resource forks/xattrs out of the archive.
(cd "$DIST" && COPYFILE_DISABLE=1 tar -czf "$NAME.tar.gz" "$NAME")

(
  cd "$DIST"
  shasum -a 256 "$NAME.tar.gz"
  shasum -a 256 \
    "$NAME/yams" \
    "$NAME/memory-search" \
    "$NAME/yams-service" \
    "$NAME/yams-wiki"
) >"$DIST/SHA256SUMS"

printf '%s\n' \
  "packaged $DIST/$NAME.tar.gz" \
  "checksums $DIST/SHA256SUMS" \
  "sbom      $DIST/$NAME.cdx.json"
