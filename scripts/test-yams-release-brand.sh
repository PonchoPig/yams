#!/bin/sh
set -eu

[ "$#" -gt 0 ] || {
  printf '%s\n' 'artifact brand audit: no executables supplied' >&2
  exit 2
}
[ -x /usr/bin/python3 ] || {
  printf '%s\n' 'artifact brand audit: /usr/bin/python3 is required' >&2
  exit 2
}

/usr/bin/python3 - "$@" <<'PY'
import pathlib
import sys

forbidden = bytes.fromhex("6d6f6e657461")
violations = []
for argument in sys.argv[1:]:
    path = pathlib.Path(argument)
    try:
        count = path.read_bytes().lower().count(forbidden)
    except OSError as error:
        print(f"artifact brand audit: could not read {path}: {error}", file=sys.stderr)
        raise SystemExit(2)
    if count:
        violations.append((path, count))

for path, count in violations:
    print(
        f"artifact brand audit: {path} contains {count} forbidden byte sequence(s)",
        file=sys.stderr,
    )
if violations:
    raise SystemExit(1)
PY
