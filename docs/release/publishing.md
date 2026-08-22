# Publishing a Yams release

Operator-only. Every step is gated on the one before it. Steps marked
**AUTHORIZATION** are release acts: do not perform them ahead of the
release decision, and never let automation perform them.

Prerequisites: validated Apple Silicon host, `gh` authenticated,
`cargo-sbom` installed, a configured git signing key (`git tag -s` must
work), the frozen Jina reference (`scripts/release-reference.env`) valid
for the release commit and in agreement with the `JINA_REVISION` and
`JINA_ARTIFACTS_SHA256` constants the release binaries enforce (step 1's
`cargo test` fails if they are not), and the full git history (not only
the tip) has been scanned for secrets and personal paths — going public
publishes every commit.

Steps 1-5 run from the repository root; steps 6-7 run in the tap
checkout, and steps 8-9 on the target machine.

1. **Gate the release commit.** On a clean default-branch checkout (`main`)
   at the release commit, run the complete source gate:

       cargo fmt --all --check
       cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
       cargo test --workspace --all-features --locked
       cargo build --workspace --all-features --release --locked
       RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps --locked
       base=$(git merge-base HEAD origin/main) && git diff --check "$base"..HEAD

   then the release-only lanes:

       ./scripts/test-rust-release-contract.sh
       ./scripts/test-package-contract.sh
       TMPDIR=$(getconf DARWIN_USER_TEMP_DIR) \
         YAMS_RELEASE_TEST_ALLOW_NET=1 \
         sh -c '. scripts/release-reference.env && ./scripts/test-rust-release.sh'

   All lanes must pass, including `pinned Jina contract passed`.

2. **Write the release notes** from `docs/release/notes-template.md` to
   `docs/release/notes/vX.Y.Z.md` (`mkdir -p docs/release/notes` on first
   use), filling every field (commit sha, macOS version, rustc version,
   `Cargo.lock` sha). Then promote the `[Unreleased]` section of
   `CHANGELOG.md` to `[X.Y.Z] — <date>`: link the entry heading to the
   release tag and its first line to the new notes file, add the
   reference link at the bottom, and start a fresh empty `[Unreleased]`
   section. Commit the notes and the changelog together.

3. **AUTHORIZATION — go public.** First release only: make
   `github.com/PonchoPig/yams` public and enable GitHub private
   vulnerability reporting (repository Settings → Advanced Security).
   The history was squashed to a fresh root on 2026-08-17. Before
   flipping visibility, confirm `git ls-remote origin` lists no ref
   carrying pre-squash history, and treat GitHub's retention of
   unreachable objects as a residual-disclosure risk: ask GitHub
   support to run a garbage collection, or publish from a freshly
   created repository, if that residue matters.
   Every release: push `main`, then create and push the git-signed
   annotated tag:

       git tag -s vX.Y.Z -m "Yams vX.Y.Z"
       git push origin main vX.Y.Z

4. **Package.**

       ./scripts/package-rust-release.sh X.Y.Z docs/release/notes/vX.Y.Z.md

   The packaging script rejects a requested version that differs from the
   staged `yams-wiki` binary before recreating `dist`.

   Record the tarball sha256:

       grep '\.tar\.gz$' dist/SHA256SUMS

   Inspect the archive inventory and verify every checksum before upload:

       tar -tzf dist/yams-X.Y.Z-aarch64-apple-darwin.tar.gz
       grep -E '/(yams|memory-search|yams-service|yams-wiki)$' dist/SHA256SUMS
       (cd dist && shasum -a 256 -c SHA256SUMS)

   The archive must contain the four executable commands `yams`,
   `memory-search`, `yams-service`, and `yams-wiki`, plus the two license
   files and release notes. `SHA256SUMS` must cover the tarball and each of the
   four commands.

   The build is not byte-reproducible (gzip timestamps), so do not re-run
   packaging after this point: the formula's sha256 in step 6 must describe
   the exact artifact uploaded in step 5.

5. **AUTHORIZATION — publish the release.**

       gh release create vX.Y.Z \
         --title "Yams vX.Y.Z" \
         --notes-file docs/release/notes/vX.Y.Z.md \
         --verify-tag \
         dist/yams-X.Y.Z-aarch64-apple-darwin.tar.gz \
         dist/SHA256SUMS \
         dist/yams-X.Y.Z-aarch64-apple-darwin.cdx.json

   `--verify-tag` is required: without it, `gh` silently creates a
   lightweight unsigned tag if the signed one was never pushed.

6. **Update the tap.** In the tap checkout — the brew-managed one at
   `$(brew --repository)/Library/Taps/ponchopig/homebrew-yams` (the
   Rollback section below uses this same path): set the formula `url`
   to the published tarball URL and `sha256` to the recorded value; run
   `brew style ponchopig/yams` and
   `brew audit --strict ponchopig/yams/yams`; commit. Until this step
   runs, the tap's `sha256` is always a pre-release placeholder — audit
   cannot detect that; a wrong value surfaces as a checksum mismatch at
   step 8. The formula must install `yams`, `memory-search`,
   `yams-service`, and `yams-wiki` from the artifact; its service stanza
   runs `yams-service`.

7. **AUTHORIZATION — publish the tap.** Push the tap (first release:
   create `github.com/PonchoPig/homebrew-yams` public and push).

8. **Verify as a user.** On a clean macOS account:
   `brew install ponchopig/yams/yams`, then run the acceptance
   checklist in `docs/release/acceptance.md`. A failure here means fix
   forward: yank nothing, publish a corrected release.

9. **Cut over this machine** (operator's Mac only):

       [ ! -e ~/.local/bin/memory-search-py ] || {
         echo 'memory-search-py already exists; resolve manually'
         exit 1
       }
       mv ~/.local/bin/memory-search ~/.local/bin/memory-search-py

   The Python venv and store under `~/.local/share/memory-search` stay
   in place; rollback is renaming the launcher back.

## Rollback (users)

Tarballs are never deleted and the tap keeps every formula revision:

    set -eu
    expected_previous_version=X.Y.Z  # replace with the rollback target
    case $expected_previous_version in
      '' | *[!0-9.]* | .* | *. | *..* | *.*.*.*)
        printf '%s\n' 'set expected_previous_version to the rollback X.Y.Z' >&2
        exit 1
        ;;
    esac
    case $expected_previous_version in
      *.*.*) ;;
      *) printf '%s\n' 'set expected_previous_version to the rollback X.Y.Z' >&2; exit 1 ;;
    esac
    brew services stop yams   # only if the service is running
    brew uninstall yams
    git -C "$(brew --repository)/Library/Taps/ponchopig/homebrew-yams" \
      checkout <previous-release-commit> -- Formula/yams.rb
    brew install --formula \
      "$(brew --repository)/Library/Taps/ponchopig/homebrew-yams/Formula/yams.rb"
    prefix=$(brew --prefix yams) || exit 1
    for binary in yams memory-search yams-service yams-wiki; do
      [ -x "$prefix/bin/$binary" ] || {
        printf '%s\n' "missing executable: $prefix/bin/$binary" >&2
        exit 1
      }
    done
    wiki_version=$("$prefix/bin/yams-wiki" --version) || exit 1
    [ "$wiki_version" = "yams-wiki $expected_previous_version" ] || {
      printf '%s\n' \
        "expected yams-wiki $expected_previous_version, got $wiki_version" >&2
      exit 1
    }
    capabilities=$("$prefix/bin/yams-wiki" capabilities --json) || exit 1
    EXPECTED_VERSION="$expected_previous_version" CAPABILITIES_JSON="$capabilities" \
      /usr/bin/python3 - <<'PY'
    import json
    import os
    import sys

    def fail(message):
        print(f"rollback capability validation failed: {message}", file=sys.stderr)
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
    PY
    brew services start yams   # only if the service was running

Afterwards:

    git -C "$(brew --repository)/Library/Taps/ponchopig/homebrew-yams" \
      checkout HEAD -- Formula/yams.rb

restores the tap. This does not undo the release: the tap's `HEAD`
still points at the newest formula revision, so a later `brew upgrade`
returns to it. Rollback is a stopgap pending a fixed-forward release,
not a substitute for one.

## Upgrade note

After `brew upgrade`, verify the complete installed command set and the wiki
contract:

    set -eu
    expected_candidate_version=X.Y.Z  # replace with the candidate version
    case $expected_candidate_version in
      '' | *[!0-9.]* | .* | *. | *..* | *.*.*.*)
        printf '%s\n' 'set expected_candidate_version to the candidate X.Y.Z' >&2
        exit 1
        ;;
    esac
    case $expected_candidate_version in
      *.*.*) ;;
      *) printf '%s\n' 'set expected_candidate_version to the candidate X.Y.Z' >&2; exit 1 ;;
    esac
    prefix=$(brew --prefix yams) || exit 1
    for binary in yams memory-search yams-service yams-wiki; do
      [ -x "$prefix/bin/$binary" ] || {
        printf '%s\n' "missing executable: $prefix/bin/$binary" >&2
        exit 1
      }
    done
    wiki_version=$("$prefix/bin/yams-wiki" --version) || exit 1
    [ "$wiki_version" = "yams-wiki $expected_candidate_version" ] || {
      printf '%s\n' \
        "expected yams-wiki $expected_candidate_version, got $wiki_version" >&2
      exit 1
    }
    capabilities=$("$prefix/bin/yams-wiki" capabilities --json) || exit 1
    EXPECTED_VERSION="$expected_candidate_version" CAPABILITIES_JSON="$capabilities" \
      /usr/bin/python3 - <<'PY'
    import json
    import os
    import sys

    def fail(message):
        print(f"upgrade capability validation failed: {message}", file=sys.stderr)
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
    PY

Service users must then `brew services restart yams`; until then the
previous `yams-service` binary keeps serving (staleness, not breakage).
