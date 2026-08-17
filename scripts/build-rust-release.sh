#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)
cd "$ROOT"

HOST=$(rustc -vV | sed -n 's/^host: //p')
case $HOST in
  '' | *[!A-Za-z0-9_.-]*)
    printf '%s\n' 'could not determine a safe Rust host target' >&2
    exit 1
    ;;
esac
TARGET_DIR="$ROOT/target/release-stage"
ARTIFACT_DIR="$TARGET_DIR/$HOST/release"

# This is deliberately a non-installing artifact step. It only stages release
# binaries below the repository's ignored libexec directory.
cargo build --locked --release --target-dir "$TARGET_DIR" --target "$HOST" \
  -p yams-cli --bin yams \
  -p yams-cli --bin memory-search \
  -p yams-service --bin yams-service \
  -p yams-wiki --bin yams-wiki
mkdir -p "$ROOT/libexec"
install -m 755 "$ARTIFACT_DIR/yams" "$ROOT/libexec/yams"
install -m 755 "$ARTIFACT_DIR/memory-search" "$ROOT/libexec/memory-search"
install -m 755 "$ARTIFACT_DIR/yams-service" "$ROOT/libexec/yams-service"
install -m 755 "$ARTIFACT_DIR/yams-wiki" "$ROOT/libexec/yams-wiki"
# Smoke: the launcher is silently product/help compatible and adds no surface.
[ "$("$ROOT/libexec/memory-search" --help)" = "$("$ROOT/libexec/yams" --help)" ] \
  || { echo "memory-search --help diverged from yams --help" >&2; exit 1; }
printf '%s\n' "staged $ROOT/libexec/yams" "staged $ROOT/libexec/memory-search" "staged $ROOT/libexec/yams-service" "staged $ROOT/libexec/yams-wiki"
