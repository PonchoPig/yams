#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)

exec /usr/bin/env python3 - "$ROOT" <<'PY'
import os
import subprocess
import sys

root = os.fsencode(sys.argv[1])
forbidden = bytes.fromhex("6d6f6e657461")

tracked = subprocess.run(
    [b"git", b"-C", root, b"ls-files", b"-z"],
    check=True,
    stdout=subprocess.PIPE,
).stdout.split(b"\0")

violations = 0
for relative in tracked:
    if not relative:
        continue

    folded_path = relative.lower()
    offset = folded_path.find(forbidden)
    while offset != -1:
        print(
            f"tracked path violation at byte {offset}: {os.fsdecode(relative)}",
            file=sys.stderr,
        )
        violations += 1
        offset = folded_path.find(forbidden, offset + 1)

    path = os.path.join(root, relative)
    if os.path.islink(path):
        content = os.fsencode(os.readlink(path))
    else:
        with open(path, "rb") as tracked_file:
            content = tracked_file.read()

    folded_content = content.lower()
    offset = folded_content.find(forbidden)
    while offset != -1:
        print(
            f"tracked bytes violation at byte {offset}: {os.fsdecode(relative)}",
            file=sys.stderr,
        )
        violations += 1
        offset = folded_content.find(forbidden, offset + 1)

if violations:
    print(f"brand audit found {violations} violation(s)", file=sys.stderr)
    raise SystemExit(1)

print("Yams brand audit passed")
PY
