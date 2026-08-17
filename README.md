# Yams

Yams is a Rust implementation of semantic search over external agent-memory
corpora. It is under active development.

## Current scope

The current workspace implements deterministic Markdown parsing and chunking,
safe corpus discovery, retrieval, a versioned SQLite store, and durable wiki
validation, indexing, and writes. It builds four user-facing binaries:

- `yams`, with search plus `--all`, `--index`, `--projects`, `--stats`,
  `--gc`, and `--write` operations;
- `memory-search`, a compatibility launcher for the same CLI;
- the optional `yams-service` process; and
- `yams-wiki`, the direct repository-memory validation and maintenance
  command.

The repository still installs nothing itself: this checkout does not install
the binaries, change `PATH`, configure a background service, or bundle the
external corpora and model artifacts they use. Distribution is via the
Homebrew tap; packaging and publication are documented in
`docs/release/publishing.md`.

Yams never bundles a user's memories. Source Markdown remains in external
corpora; generated indexes, vectors, and models remain either outside this
source checkout or excluded from source control.

## Development

The repository pins Rust 1.97.1. Run the complete local gate with:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
cargo build --workspace --all-features --release --locked
RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps --locked
base=$(git merge-base HEAD origin/main) && git diff --check "$base"..HEAD
```

## Install (macOS, Apple Silicon)

Requires macOS 14 (Sonoma) or newer on Apple Silicon.

Yams supports macOS on Apple Silicon only: there are no Linux or Intel
builds. CI's Linux job is a path-regression aid, not a support commitment.

```sh
brew install ponchopig/yams/yams
```

If the project has no repository memory yet, initialize it first. Inspect,
plan, review the manifest, then apply — none of those commands git-commit:

```sh
yams-wiki init inspect --json . > inspection.json
yams-wiki init plan --from-inspect inspection.json --project-page project-page.json > manifest.json
# Review manifest_sha256, proposal, operations, and authored file contents.
yams-wiki init apply --manifest manifest.json
```

Then build this project's search store from inside the project. `yams
--index` writes the per-project search store. That is not
`yams-wiki catalog`, which regenerates `INDEX.md`. Search does not create
the search store or download the model. `yams --index` is enough when
the model is already cached. Add `YAMS_ALLOW_NET=1` only when the pinned
model snapshot is not yet cached — a first-time machine, or a Yams upgrade
that moved the pin — to download the embedding model (~550 MB). Skip
initialization when `.agents/memory` already exists:

```sh
yams --index
# YAMS_ALLOW_NET=1 yams --index   # only when the pinned snapshot is not cached
```

Everything afterwards is offline. Optional warm-query service:

```sh
brew services start yams        # opt in
brew services restart yams      # after every upgrade
```

Uninstall with `brew uninstall yams`. State under
`~/Library/Caches/yams` and `~/Library/Application Support/yams`
is never auto-deleted, and memories in your projects' `.agents/memory`
are never touched. Release provenance and publication process:
`docs/release/publishing.md`.

## Release validation

After the source gate passes, run the hermetic staged-artifact smoke:

```sh
./scripts/test-rust-release.sh
```

The script builds the four staged release binaries, exercises their public
process contracts against a fictional temporary corpus and isolated state,
and verifies the expected offline model-cache failure. It never discovers or
modifies real memories. The binaries provision their private
`rust-v1/locks` model-construction directory themselves — mode 0700, owned by
the effective user, fail-closed on anything unsafe already at that path — and
this smoke exercises that path rather than pre-creating the directory.

For a release candidate, opt into the online-to-offline direct and service
flow and supply the complete pinned Jina contract:

```sh
YAMS_RELEASE_TEST_ALLOW_NET=1 \
YAMS_TEST_JINA_MODEL_CACHE=/path/to/pinned/cache \
YAMS_TEST_JINA_EXPECTED_SIGNATURE='expected-signature' \
YAMS_TEST_JINA_EXPECTED_QUERY_SHA256='expected-digest' \
./scripts/test-rust-release.sh
```

The release-owned reference values, their provenance, and the rules for
re-establishing them are recorded in `docs/release/jina-reference.md`; a
sourceable copy lives in `scripts/release-reference.env`.

Yams pins the model itself, independently of this lane: the snapshot revision
and the artifact digest are source constants, every download is qualified by
that revision rather than a moving `main`, and a model whose bytes miss the
digest fails closed in both the online and the offline path. The
`YAMS_TEST_JINA_*` values cross-check those built-in pins against a blessed
cache on a specific host, covering the runtime and target components a single
build cannot pin everywhere; the ordinary `cargo test` gate already fails if
the recorded values and the source constants disagree.

The three `YAMS_TEST_JINA_*` values are atomic: set all of them or none.
The real-model lane downloads into temporary Yams state, reruns retrieval
with network access disabled, then proves both direct and service-backed
search. It is operator-run because model caching and frozen expected values
are not configured in hosted CI. Slow hosts may set
`YAMS_RELEASE_TEST_SERVICE_READY_TIMEOUT_SECONDS` to an integer from 1 to
600; the default is 120 seconds.

Installation, `PATH` cutover, launch-agent startup, upgrade, rollback, and
uninstall validation are defined in `docs/release/acceptance.md` and
exercised per `docs/release/publishing.md`.

All committed fixtures must be fictional. Never use a real memory page,
query log, database, model, personal path, or agent workflow document as test
data.

The installed `yams-wiki` command supports repository-memory initialization
as well as `check`, `compat`, `catalog`, `write`, and `capabilities`. The paths
below are placeholders for fictional external inputs:

```sh
yams-wiki init inspect --json /path/to/example-repository > inspection.json
yams-wiki init plan --from-inspect /path/to/example-inspection.json --project-page /path/to/example-project-page.json > manifest.json
# A human reviews the manifest digest and proposed diff here.
yams-wiki init apply --manifest manifest.json
yams-wiki check /path/to/example-wiki
yams-wiki compat /path/to/example-wiki
yams-wiki catalog --check /path/to/example-wiki
yams-wiki catalog /path/to/example-wiki
yams-wiki write /path/to/example-wiki < /path/to/example-write-request.json
yams-wiki capabilities --json
```

Initialization has an explicit inspect, plan, approve, and apply boundary.
`inspect` reports the current repository state without changing it. Its
`inspection_sha256` is an opaque, local value. `init plan --from-inspect`
copies `root` and that digest from the inspect file; `--request` still
accepts a complete JSON request. When both modes are attainable,
`recommended_mode` is `full`. `plan` also leaves the target unchanged and
prints the exact manifest to review. Omit `agents_md` or `--agents-md` to
install or keep the canonical Project memory section when that is valid;
otherwise supply the exact desired `AGENTS.md`. Before `apply`, a human must approve that
manifest's `manifest_sha256`, proposed diff (`proposal` and `operations`),
destination, and authored file contents. Bundled layout assets can be
reviewed by digest. `apply` accepts that saved manifest and refuses it if
the repository has drifted. A successful apply lists `next` commands such as
`yams --index`; apply never runs them. These commands do not stage,
commit, push, or perform other version-control operations.

Planning memory files and installing agent skills are separate steps. Yams
sets up memory files; `npx skills` installs agent skills separately.

`check` validates the corpus, and `catalog --check` reports whether the derived
catalog would change; neither rewrites corpus content. `catalog` regenerates the
derived catalog in `INDEX.md`. `compat` reports constructs outside the supported compatibility
profile. `write` consumes one JSON request from standard input, durably installs
the page, then regenerates the derived catalog. When a page becomes visible, the
JSON response reports `paths` and
`index_regenerated`, including when catalog regeneration subsequently fails.
`capabilities --json` dumps the workspace contract versions, including
`search_results: 1` even though `yams-wiki` itself does not search — that
contract belongs to `yams` / `memory-search`. `init_manifest: 3` means the inspect/plan/apply contract includes
`recommended_mode`, optional `agents_md`, and apply `next` as `yams --index`. Installed agent policy
names `yams` as the search command; `memory-search` remains the
compatibility launcher.

## License

Yams is licensed under either the MIT License or the Apache License 2.0, at
your option.
