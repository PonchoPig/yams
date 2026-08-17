# Install-surface acceptance checklist

Run after `brew install ponchopig/yams/yams` on a clean account, and
after any formula change. Use only fictional, run-owned test data.

1. Install printed the caveats (layout, first-run download, service
   opt-in, uninstall paths).
2. The installed formula prefix contains all four executable commands, and the
   wiki command reports its version and capability contract:

   ```sh
   set -eu
   expected_version=X.Y.Z  # replace with the candidate's numeric triplet
   case $expected_version in
     '' | *[!0-9.]* | .* | *. | *..* | *.*.*.*)
       printf '%s\n' 'set expected_version to the candidate X.Y.Z' >&2
       exit 1
       ;;
   esac
   case $expected_version in
     *.*.*) ;;
     *) printf '%s\n' 'set expected_version to the candidate X.Y.Z' >&2; exit 1 ;;
   esac
   prefix=$(brew --prefix yams) || exit 1
   for binary in yams memory-search yams-service yams-wiki; do
     [ -x "$prefix/bin/$binary" ] || {
       printf '%s\n' "missing executable: $prefix/bin/$binary" >&2
       exit 1
     }
   done
   wiki_version=$("$prefix/bin/yams-wiki" --version) || exit 1
   [ "$wiki_version" = "yams-wiki $expected_version" ] || {
     printf '%s\n' \
       "expected yams-wiki $expected_version, got $wiki_version" >&2
     exit 1
   }
   capabilities=$("$prefix/bin/yams-wiki" capabilities --json) || exit 1
   EXPECTED_VERSION="$expected_version" CAPABILITIES_JSON="$capabilities" \
     /usr/bin/python3 - <<'PY'
   import json
   import os
   import sys

   def fail(message):
       print(f"capability validation failed: {message}", file=sys.stderr)
       raise SystemExit(1)

   try:
       expected_version = os.environ["EXPECTED_VERSION"]
       raw_capabilities = os.environ["CAPABILITIES_JSON"]
   except KeyError:
       fail("validation input is missing")
   try:
       document = json.loads(raw_capabilities)
   except (TypeError, ValueError):
       fail("yams-wiki capabilities are not valid JSON")
   if not isinstance(document, dict):
       fail("top-level JSON value must be an object")
   if document.get("yams_version") != expected_version:
       fail(f"top-level yams_version must equal {expected_version}")
   contracts = document.get("contracts")
   if not isinstance(contracts, dict):
       fail("contracts must be an object")
   for key, expected_value in (
       ("repository_layout", 1),
       ("init_manifest", 3),
       ("wiki_maintenance", 2),
   ):
       value = contracts.get(key)
       if type(value) is not int or value != expected_value:
           fail(f"contracts.{key} must be integer {expected_value}")
   PY
   ```

   These checks bind both version surfaces to the candidate and retain the
   `"repository_layout":1`, `"init_manifest":3`, and `"wiki_maintenance":2`
   contract checks.
3. Locks directory is not installed: the binaries provision
   `~/Library/Caches/yams/rust-v1/locks` themselves on first model
   construction, so verify after step 5 that
   `/usr/bin/stat -f %Lp ~/Library/Caches/yams/rust-v1/locks` prints
   `700` (a GNU `stat` earlier on `PATH` breaks `-f`).
4. Model-free management works: `yams --projects --json` exits 0 and
   prints `"ok":true`.
5. First-run download: in a scratch project containing
   `.agents/memory/alpha.md` with fictional content,
   `YAMS_ALLOW_NET=1 yams --index` exits 0.
6. Offline retrieval: `yams --json --no-gate 'fictional query text'`
   returns the fictional page with `YAMS_ALLOW_NET` unset (offline is
   the default).
7. Warm service:
   - `brew services start yams`, then poll up to 120 s for the socket
     (launchd startup is asynchronous). Note there is no `/` between
     the `getconf` output and `yams-...`: `DARWIN_USER_TEMP_DIR`
     already ends in a trailing slash.

     ```sh
     RUNTIME_SOCKET="$(getconf DARWIN_USER_TEMP_DIR)yams-$(id -u)/service.sock"
     i=0
     while [ ! -S "$RUNTIME_SOCKET" ]; do
       i=$((i + 1))
       [ "$i" -lt 240 ] || { echo "timed out waiting for $RUNTIME_SOCKET"; exit 1; }
       sleep 0.5
     done
     ```
   - Prove the service answered: `YAMS_SERVICE_SOCKET="$RUNTIME_SOCKET" yams --json --no-gate 'fictional query text'`
     returns the fictional page. A `YAMS_HOME` override forces direct
     execution even when a socket is set, so do not combine them here.
8. Stop: `brew services stop yams`; allow a few seconds, then confirm
   `brew services list` shows yams stopped and the service process is
   gone. A leftover socket file is expected (the service unlinks it on
   idle shutdown, not on SIGTERM); clients fall back to direct cleanly
   and the next start reclaims it.
9. Upgrade drill (when a previous release exists): install the previous
   formula revision (the exact recipe is in the Rollback section of
   `docs/release/publishing.md`); `brew list --versions yams` to
   record the installed version; `brew upgrade` to the candidate; `brew
   list --versions yams` again to confirm the running version changed;
   confirm the caveat about `brew services restart` and that queries
   still answer throughout.
10. Uninstall: `brew uninstall yams`; binaries gone (`brew list
   --versions yams` prints nothing); state under
   `~/Library/Caches/yams` and `~/Library/Application Support/yams`
   still present; the scratch project's `.agents/memory` untouched.
