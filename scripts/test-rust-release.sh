#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)

fail_configuration() {
  printf '%s\n' "release smoke: $*" >&2
  exit 2
}

fail() {
  printf '%s\n' "release smoke: $*" >&2
  exit 1
}

fixture_git() {
  /usr/bin/env -i \
    HOME="$TEST_ROOT/home" \
    LC_ALL=C \
    PATH=/usr/bin:/bin \
    GIT_CONFIG_GLOBAL=/dev/null \
    GIT_CONFIG_NOSYSTEM=1 \
    /usr/bin/git -C "$INIT_PROJECT" "$@"
}

hostile_fixture_git() (
  export GIT_DIR="$HOSTILE_GIT_DIR"
  export GIT_WORK_TREE="$HOSTILE_GIT_WORK_TREE"
  export GIT_COMMON_DIR="$HOSTILE_GIT_COMMON_DIR"
  export GIT_CONFIG="$HOSTILE_GIT_CONFIG"
  export GIT_CONFIG_GLOBAL="$HOSTILE_GIT_CONFIG"
  export GIT_CONFIG_SYSTEM="$HOSTILE_GIT_CONFIG"
  export GIT_CONFIG_NOSYSTEM=0
  export GIT_CONFIG_COUNT=1
  export GIT_CONFIG_KEY_0=core.hooksPath
  export GIT_CONFIG_VALUE_0="$TEST_ROOT/hostile-hooks"
  fixture_git "$@"
)

hostile_fixture_python() {
  PYTHONOPTIMIZE=2 PYTHONPATH="$HOSTILE_PYTHON_PATH" \
    /usr/bin/python3 -I "$@"
}

pinned_variables=0
[ -n "${YAMS_TEST_JINA_MODEL_CACHE:-}" ] && pinned_variables=$((pinned_variables + 1))
[ -n "${YAMS_TEST_JINA_EXPECTED_SIGNATURE:-}" ] && pinned_variables=$((pinned_variables + 1))
[ -n "${YAMS_TEST_JINA_EXPECTED_QUERY_SHA256:-}" ] && pinned_variables=$((pinned_variables + 1))

if [ "$pinned_variables" -ne 0 ] && [ "$pinned_variables" -ne 3 ]; then
  fail_configuration 'set all three YAMS_TEST_JINA_* variables or none of them'
fi

case ${YAMS_RELEASE_TEST_ALLOW_NET:-0} in
  0 | 1) ;;
  *) fail_configuration 'YAMS_RELEASE_TEST_ALLOW_NET must be 0 or 1' ;;
esac

SERVICE_READY_SECONDS=${YAMS_RELEASE_TEST_SERVICE_READY_TIMEOUT_SECONDS:-120}
case $SERVICE_READY_SECONDS in
  [1-9] | [1-9][0-9] | [1-5][0-9][0-9] | 600) ;;
  *)
    fail_configuration \
      'YAMS_RELEASE_TEST_SERVICE_READY_TIMEOUT_SECONDS must be an integer from 1 to 600'
    ;;
esac
SERVICE_READY_ATTEMPTS=$((SERVICE_READY_SECONDS * 10))

unset YAMS_HOME YAMS_DIRS YAMS_ALLOW_NET YAMS_NO_SERVICE YAMS_SERVICE_SOCKET

TEST_ROOT=
SERVICE_PID=

cleanup() {
  status=$?
  trap - 0 HUP INT TERM
  if [ -n "$SERVICE_PID" ] && kill -0 "$SERVICE_PID" 2>/dev/null; then
    kill "$SERVICE_PID" 2>/dev/null || true
    wait "$SERVICE_PID" 2>/dev/null || true
  fi
  if [ -n "$TEST_ROOT" ] && [ -d "$TEST_ROOT" ]; then
    rm -rf -- "$TEST_ROOT"
  fi
  exit "$status"
}
trap cleanup 0
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

expect_status() {
  expected=$1
  label=$2
  shift 2
  : >"$COMMAND_STDOUT"
  : >"$COMMAND_STDERR"
  set +e
  "$@" >"$COMMAND_STDOUT" 2>"$COMMAND_STDERR"
  actual=$?
  set -e
  if [ "$actual" -ne "$expected" ]; then
    printf '%s\n' "release smoke: $label: expected exit $expected, got $actual" >&2
    sed -n '1,80p' "$COMMAND_STDOUT" >&2
    sed -n '1,80p' "$COMMAND_STDERR" >&2
    exit 1
  fi
}

require_output() {
  path=$1
  text=$2
  label=$3
  if ! grep -F -- "$text" "$path" >/dev/null; then
    printf '%s\n' "release smoke: $label: missing expected output: $text" >&2
    sed -n '1,80p' "$path" >&2
    exit 1
  fi
}

wait_for_service_ready() {
  attempts=0
  while ! grep -Fx 'READY' "$SERVICE_STDOUT" >/dev/null 2>&1; do
    if ! kill -0 "$SERVICE_PID" 2>/dev/null; then
      wait "$SERVICE_PID" 2>/dev/null || true
      SERVICE_PID=
      printf '%s\n' 'release smoke: yams-service exited before READY' >&2
      sed -n '1,80p' "$SERVICE_STDERR" >&2
      exit 1
    fi
    attempts=$((attempts + 1))
    [ "$attempts" -lt "$SERVICE_READY_ATTEMPTS" ] \
      || fail "timed out after ${SERVICE_READY_SECONDS}s waiting for yams-service READY"
    sleep 0.1
  done
}

wait_for_service_exit() {
  attempts=0
  while [ -e "$SERVICE_SOCKET" ] && kill -0 "$SERVICE_PID" 2>/dev/null; do
    attempts=$((attempts + 1))
    [ "$attempts" -lt 100 ] || fail 'timed out waiting for yams-service idle shutdown'
    sleep 0.1
  done
  set +e
  wait "$SERVICE_PID"
  service_status=$?
  set -e
  SERVICE_PID=
  [ "$service_status" -eq 0 ] || fail "yams-service exited with status $service_status"
}

TEST_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/yams-release-test.XXXXXX")
# yams-service refuses socket paths whose ancestors include symlinks (macOS
# /tmp -> private/tmp) or shared-writable directories, so pin the physical path.
TEST_ROOT=$(CDPATH= cd -- "$TEST_ROOT" && pwd -P)
PROJECT="$TEST_ROOT/project"
CORPUS="$PROJECT/.agents/memory"
INIT_PROJECT="$TEST_ROOT/fictional-init-project"
INIT_INSPECTION="$TEST_ROOT/init-inspection.json"
INIT_REQUEST="$TEST_ROOT/init-request.json"
INIT_MANIFEST="$TEST_ROOT/init-manifest.json"
INIT_POLICY="$ROOT/crates/yams-wiki/assets/repository-memory-v1/agent-policy.md"
HOSTILE_GIT_DIR="$TEST_ROOT/hostile-git-dir"
HOSTILE_GIT_WORK_TREE="$TEST_ROOT/hostile-git-work-tree"
HOSTILE_GIT_COMMON_DIR="$TEST_ROOT/hostile-git-common-dir"
HOSTILE_GIT_CONFIG="$TEST_ROOT/hostile-git-config"
HOSTILE_PYTHON_PATH="$TEST_ROOT/hostile-python"
STATE="$TEST_ROOT/state"
# Deliberately never created: the fresh-install lane below proves the binaries
# provision their whole private chain, base included.
STATE_FRESH="$TEST_ROOT/state-fresh"
TEMPORARY_DIRECTORY="$TEST_ROOT/tmp"
COMMAND_STDOUT="$TEST_ROOT/command.stdout"
COMMAND_STDERR="$TEST_ROOT/command.stderr"
SERVICE_STDOUT="$TEST_ROOT/service.stdout"
SERVICE_STDERR="$TEST_ROOT/service.stderr"
SERVICE_SOCKET="$TEST_ROOT/service.sock"
mkdir -p \
  "$CORPUS" "$INIT_PROJECT" "$STATE" \
  "$TEMPORARY_DIRECTORY" "$TEST_ROOT/home" "$HOSTILE_GIT_WORK_TREE" \
  "$HOSTILE_PYTHON_PATH"

printf '%s\n' 'raise SystemExit("hostile sitecustomize loaded")' \
  >"$HOSTILE_PYTHON_PATH/sitecustomize.py"

hostile_fixture_git init --quiet
[ ! -e "$HOSTILE_GIT_DIR" ] || fail 'fixture git escaped through hostile GIT_DIR'
[ ! -e "$HOSTILE_GIT_COMMON_DIR" ] \
  || fail 'fixture git escaped through hostile GIT_COMMON_DIR'
[ ! -e "$HOSTILE_GIT_CONFIG" ] \
  || fail 'fixture git used a hostile Git configuration path'

{
  printf '%s\n' '---'
  printf '%s\n' 'title: Alpha beacon'
  printf '%s\n' 'status: current'
  printf '%s\n' '---'
  printf '\n%s\n' 'A fictional violet beacon is used only for release testing.'
} >"$CORPUS/alpha.md"

(
  unset CARGO_TARGET_DIR CARGO_BUILD_TARGET
  exec "$ROOT/scripts/build-rust-release.sh"
)

YAMS="$ROOT/libexec/yams"
MEMORY_SEARCH="$ROOT/libexec/memory-search"
YAMS_SERVICE="$ROOT/libexec/yams-service"
YAMS_WIKI="$ROOT/libexec/yams-wiki"
for binary in "$YAMS" "$MEMORY_SEARCH" "$YAMS_SERVICE" "$YAMS_WIKI"; do
  [ -x "$binary" ] || fail "staged binary is not executable: $binary"
done
"$ROOT/scripts/test-yams-release-brand.sh" \
  "$YAMS" "$MEMORY_SEARCH" "$YAMS_SERVICE" "$YAMS_WIKI" \
  || fail 'staged executables contain retired product bytes'

"$YAMS" --help >"$TEST_ROOT/yams.help"
"$MEMORY_SEARCH" --help >"$TEST_ROOT/memory-search.help"
cmp -s "$TEST_ROOT/yams.help" "$TEST_ROOT/memory-search.help" \
  || fail 'memory-search --help differs from yams --help'

expect_status 0 'yams-wiki capabilities' "$YAMS_WIKI" capabilities --json
require_output "$COMMAND_STDOUT" '"repository_layout":1' 'yams-wiki capabilities'
require_output "$COMMAND_STDOUT" '"init_manifest":3' 'yams-wiki capabilities'
require_output "$COMMAND_STDOUT" '"wiki_maintenance":2' 'yams-wiki capabilities'

[ -f "$INIT_POLICY" ] || fail "canonical initialization policy is missing: $INIT_POLICY"

expect_status 0 'repository memory init inspect' \
  "$YAMS_WIKI" init inspect --json "$INIT_PROJECT"
cp "$COMMAND_STDOUT" "$INIT_INSPECTION"

hostile_fixture_python - "$INIT_INSPECTION" "$INIT_POLICY" "$INIT_REQUEST" <<'PY'
import json
import pathlib
import sys

inspection_path, policy_path, request_path = map(pathlib.Path, sys.argv[1:])
with inspection_path.open(encoding="utf-8") as source:
    inspection = json.load(source)
policy = policy_path.read_text(encoding="utf-8")
request = {
    "root": inspection["root"],
    "inspection_sha256": inspection["inspection_sha256"],
    "mode": "minimal",
    "date": "2026-08-12",
    "agents_md": policy,
    "project_page": {
        "title": "Project context",
        "page_type": "project-state",
        "fact": "The fictional release project stores shared memory in Markdown.",
        "why": "The staged initialization contract needs a hermetic release test.",
        "how_to_apply": "Review the manifest digest and diff before applying it.",
        "falsified_by": "The staged binary cannot create the approved files.",
        "summary": "fictional release initialization uses an approved manifest",
    },
}
request_path.write_text(
    json.dumps(request, ensure_ascii=False, separators=(",", ":")) + "\n",
    encoding="utf-8",
)
PY

expect_status 0 'repository memory init plan' \
  "$YAMS_WIKI" init plan --request "$INIT_REQUEST"
# Preserve the staged binary's exact stdout as the artifact a human approves.
cp "$COMMAND_STDOUT" "$INIT_MANIFEST"

hostile_fixture_python - "$INIT_INSPECTION" "$INIT_MANIFEST" <<'PY'
import hashlib
import json
import pathlib
import sys

inspection_path, manifest_path = map(pathlib.Path, sys.argv[1:])
inspection = json.loads(inspection_path.read_text(encoding="utf-8"))
envelope = json.loads(manifest_path.read_text(encoding="utf-8"))
if not isinstance(inspection, dict):
    raise SystemExit("release smoke: init inspection must be a JSON object")
if not isinstance(envelope, dict):
    raise SystemExit("release smoke: init manifest envelope must be a JSON object")
manifest = envelope.get("manifest")
if not isinstance(manifest, dict):
    raise SystemExit("release smoke: init manifest must be a JSON object")
canonical = json.dumps(
    manifest, ensure_ascii=False, separators=(",", ":")
).encode("utf-8")
if envelope.get("ok") is not True:
    raise SystemExit("release smoke: init plan did not report ok=true")
if manifest.get("mode") != "minimal":
    raise SystemExit("release smoke: init plan did not produce a minimal manifest")
if manifest.get("inspection_sha256") != inspection.get("inspection_sha256"):
    raise SystemExit("release smoke: init plan changed the opaque inspection digest")
if envelope.get("manifest_sha256") != hashlib.sha256(canonical).hexdigest():
    raise SystemExit("release smoke: init plan reported the wrong manifest digest")
PY

expect_status 0 'repository memory init apply' \
  "$YAMS_WIKI" init apply --manifest "$INIT_MANIFEST"

hostile_fixture_python - "$INIT_PROJECT" <<'PY'
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
page = root / ".agents" / "memory" / "project-context.md"
if not page.is_file():
    raise SystemExit("release smoke: minimal initialization did not create the flat page")
if (root / ".agents" / "memory" / "pages").exists():
    raise SystemExit("release smoke: minimal initialization created a structured pages directory")

source = (root / "AGENTS.md").read_text(encoding="utf-8")
heading_count = 0
fence = None
for line in source.splitlines():
    stripped = line.lstrip(" \t")
    marker = stripped[:1]
    width = len(stripped) - len(stripped.lstrip(marker)) if marker else 0
    if marker in ("`", "~") and width >= 3:
        if fence is None:
            fence = (marker, width)
        elif marker == fence[0] and width >= fence[1] and not stripped[width:].strip():
            fence = None
        continue
    if fence is None and line == "## Project memory":
        heading_count += 1
if heading_count != 1:
    raise SystemExit(
        f"release smoke: found {heading_count} logical Project memory headings"
    )
PY

if hostile_fixture_git rev-parse --verify HEAD >/dev/null 2>&1; then
  fail 'repository memory initialization unexpectedly created a commit'
fi

expect_status 0 'projects management' \
  env HOME="$TEST_ROOT/home" TMPDIR="$TEMPORARY_DIRECTORY" \
  YAMS_HOME="$STATE" YAMS_DIRS="$CORPUS" YAMS_NO_SERVICE=1 \
  "$YAMS" --projects --json
require_output "$COMMAND_STDOUT" '"ok":true' 'projects management'
require_output "$COMMAND_STDOUT" '"projects":[]' 'projects management'

expect_status 0 'stats management' \
  env HOME="$TEST_ROOT/home" TMPDIR="$TEMPORARY_DIRECTORY" \
  YAMS_HOME="$STATE" YAMS_DIRS="$CORPUS" YAMS_NO_SERVICE=1 \
  "$YAMS" --stats --json --project "$PROJECT"
require_output "$COMMAND_STDOUT" '"ok":true' 'stats management'
require_output "$COMMAND_STDOUT" '"operation":"stats"' 'stats management'

expect_status 2 'unknown option contract' "$YAMS" --definitely-not-a-yams-option

# Nothing provisions the model-construction lock directory here: the binaries
# create it themselves, mode 0700 and fail-closed, so this index exercises
# that self-provisioning before it reaches the expected offline cache failure.
[ ! -e "$STATE/rust-v1/locks" ] || fail 'the release smoke must not pre-create the lock directory'

expect_status 4 'offline empty-cache index' \
  env HOME="$TEST_ROOT/home" TMPDIR="$TEMPORARY_DIRECTORY" \
  YAMS_HOME="$STATE" YAMS_DIRS="$CORPUS" YAMS_NO_SERVICE=1 \
  "$YAMS" --index --project "$PROJECT"
require_output "$COMMAND_STDERR" 'network is off; retry with YAMS_ALLOW_NET=1' \
  'offline empty-cache index'

# macOS-only artifact smoke; -f is the BSD stat format flag, as in
# docs/release/acceptance.md.
require_private_directory() {
  [ -d "$1" ] || fail "model construction did not provision $1"
  mode=$(/usr/bin/stat -f %Lp "$1")
  [ "$mode" = 700 ] || fail "self-provisioned $1 mode is $mode, expected 700"
}

require_private_directory "$STATE/rust-v1/locks"

# The management commands above already created $STATE/rust-v1 through the
# store, so that lane only proves leaf creation. Repeat the expectation against
# an untouched YAMS_HOME, where the base and rust-v1 are missing too, which is
# what a fresh install looks like before anything has ever run.
[ ! -e "$STATE_FRESH" ] || fail 'the fresh-install lane must start from nothing'

expect_status 4 'offline empty-cache index from a fresh home' \
  env HOME="$TEST_ROOT/home" TMPDIR="$TEMPORARY_DIRECTORY" \
  YAMS_HOME="$STATE_FRESH" YAMS_DIRS="$CORPUS" YAMS_NO_SERVICE=1 \
  "$YAMS" --index --project "$PROJECT"
require_output "$COMMAND_STDERR" 'network is off; retry with YAMS_ALLOW_NET=1' \
  'offline empty-cache index from a fresh home'

require_private_directory "$STATE_FRESH"
require_private_directory "$STATE_FRESH/rust-v1"
require_private_directory "$STATE_FRESH/rust-v1/locks"

if [ "${YAMS_RELEASE_TEST_ALLOW_NET:-0}" = 1 ]; then
  expect_status 0 'online release index' \
    env HOME="$TEST_ROOT/home" TMPDIR="$TEMPORARY_DIRECTORY" \
    YAMS_HOME="$STATE" YAMS_DIRS="$CORPUS" YAMS_NO_SERVICE=1 \
    YAMS_ALLOW_NET=1 "$YAMS" --index --project "$PROJECT"

  expect_status 0 'offline direct release query' \
    env HOME="$TEST_ROOT/home" TMPDIR="$TEMPORARY_DIRECTORY" \
    YAMS_HOME="$STATE" YAMS_DIRS="$CORPUS" YAMS_NO_SERVICE=1 \
    "$YAMS" --json --no-gate --project "$PROJECT" 'violet beacon'
  require_output "$COMMAND_STDOUT" '"name": "Alpha beacon"' 'offline direct release query'

  env HOME="$TEST_ROOT/home" TMPDIR="$TEMPORARY_DIRECTORY" \
    YAMS_HOME="$STATE" YAMS_DIRS="$CORPUS" \
    "$YAMS_SERVICE" --socket "$SERVICE_SOCKET" --idle-timeout 2 \
    >"$SERVICE_STDOUT" 2>"$SERVICE_STDERR" &
  SERVICE_PID=$!
  wait_for_service_ready

  expect_status 0 'service-backed release query' \
    env HOME="$TEST_ROOT/home" TMPDIR="$TEMPORARY_DIRECTORY" \
    YAMS_SERVICE_SOCKET="$SERVICE_SOCKET" \
    "$YAMS" --json --no-gate --project "$PROJECT" 'violet beacon'
  require_output "$COMMAND_STDOUT" '"name": "Alpha beacon"' 'service-backed release query'

  wait_for_service_exit
  [ ! -e "$SERVICE_SOCKET" ] || fail 'yams-service left its socket behind after shutdown'
  printf '%s\n' 'real-model direct and service smoke passed'
else
  printf '%s\n' 'real-model direct and service smoke skipped (set YAMS_RELEASE_TEST_ALLOW_NET=1)'
fi

if [ "$pinned_variables" -eq 3 ]; then
  PINNED_TEST_LIST="$TEST_ROOT/pinned-jina.list"
  (
    cd "$ROOT"
    cargo test -p yams-embed --test jina \
      cached_jina_v2_has_the_frozen_contract \
      --all-features --locked -- --ignored --exact --list
  ) >"$PINNED_TEST_LIST"
  pinned_test_matches=$(grep -Fxc \
    'cached_jina_v2_has_the_frozen_contract: test' "$PINNED_TEST_LIST" || true)
  if [ "$pinned_test_matches" -ne 1 ]; then
    printf '%s\n' \
      'release smoke: pinned Jina contract test is missing, renamed, or no longer ignored' >&2
    sed -n '1,40p' "$PINNED_TEST_LIST" >&2
    exit 1
  fi
  (
    cd "$ROOT"
    cargo test -p yams-embed --test jina \
      cached_jina_v2_has_the_frozen_contract \
      --all-features --locked -- --ignored --exact
  )
  printf '%s\n' 'pinned Jina contract passed'
else
  printf '%s\n' 'pinned Jina contract skipped (set all three YAMS_TEST_JINA_* variables)'
fi

printf '%s\n' 'release artifact smoke passed'
