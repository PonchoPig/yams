# Frozen Jina reference values

The snapshot revision and the artifact digest are enforced by the product, not
only by this lane: they ship as the `JINA_REVISION` and
`JINA_ARTIFACTS_SHA256` constants in `crates/yams-embed/src/jina.rs`. Every
download is qualified by that revision, every constructed model is checked
against that digest, and both the offline and the online paths fail closed on
any difference. The values recorded here are the same values; the
`pinned_provenance_matches_the_release_reference` unit test fails the ordinary
`cargo test` gate if this document, `scripts/release-reference.env`, and the
source constants ever disagree.

The trust boundary is those two compiled-in constants, not anything the server
says: the snapshot directory is opened by the pinned name and the loaded bytes
are checked against the pinned digest, so a hostile or misconfigured endpoint
can waste bandwidth and fail the load, but cannot cause unverified weights to
be used.

Artifact provenance established: 2026-08-11 on repository commit `5261e9d`
(macOS aarch64 release host). The Yams signature was re-derived on 2026-08-12
from the same byte-verified artifacts after the intentional hash-domain
rename, on the clean-break branch based on `309aa71`. These are the
release-owned reference values for the pinned Jina contract lane of
`scripts/test-rust-release.sh` and the ignored test
`cached_jina_v2_has_the_frozen_contract` in
`crates/yams-embed/tests/jina.rs`. They are reference fixtures, not
credentials. The populated model cache itself is operator-owned local state
(see below) and must never be committed.

## Values

A sourceable copy lives in `scripts/release-reference.env`:

- `YAMS_TEST_JINA_MODEL_CACHE` — operator-local path to the blessed
  populated cache. The sourceable file defaults to
  `$HOME/.yams-release/state/rust-v1/models`; operators may override it.
  The 2026-08-12 re-derivation reused the release host's byte-verified
  predecessor cache and its operator-local provenance record. Yams does not
  automatically migrate or copy that state.
- `YAMS_TEST_JINA_EXPECTED_SIGNATURE` —

  ```
  jinaai/jina-embeddings-v2-base-en|fastembed=5.17.4|dimensions=768|pooling=mean|quantization=none|max_length=8192|query_prefix=|passage_prefix=|intra_threads=1|ort=2.0.0-rc.13|onnxruntime=1.28.0|target=macos-aarch64|ep=cpu|snapshot=322d4d7e2f35e84137961a65af894fda0385eb7a|artifacts_sha256=3feec2cc49819ff4af53f2cc895902915a2dfef0f1130adf01667a30c38a6890
  ```

- `YAMS_TEST_JINA_EXPECTED_QUERY_SHA256` —

  ```
  87e2fa9f82d704e30bcc07abbdb2c4909a082319fa28c2659c0084447f51974c
  ```

  (SHA-256 over the little-endian f32 bytes of the 768-dimension embedding
  of the contract query `memory search`.)

## How they were established

- The cache was populated by the staged predecessor release binary's own online
  bootstrap. The equivalent current invocation is `libexec/yams --index`
  with `YAMS_ALLOW_NET=1`. It used the pinned stack: fastembed 5.17.4, ort
  2.0.0-rc.13 (ONNX Runtime 1.28.0), hf-hub 0.5.0, tokenizers 0.22.2 — all
  locked by `Cargo.lock`, with the fastembed/ort literals additionally
  lockfile-guarded by a unit test.
- Snapshot revision `322d4d7e2f35e84137961a65af894fda0385eb7a` was confirmed
  equal to the upstream `main` commit of `jinaai/jina-embeddings-v2-base-en`
  via the Hugging Face API, and every artifact's local bytes were verified
  against upstream metadata (`model.onnx` SHA-256
  `a6bccce798906f269ee6990d35b8a516390a9593cde824de2e6b9d087b07fa4d`,
  547390322 bytes; the four JSON artifacts by git-blob SHA-1 and size).
- The original signature and query digest were derived through the frozen
  contract-test codepath itself, the green contract run was repeated for
  determinism, and the complete release-candidate gate passed on commit
  `5261e9d`.
- On 2026-08-12, the unchanged artifact bytes were exercised through the Yams
  contract-test codepath. The intentional Yams artifact-digest domain changed
  `artifacts_sha256` to the value above, while the model output and frozen query
  digest remained unchanged.

## Validity and re-establishment

The signature deliberately binds the compilation target and runtime stack:
these values hold for macOS aarch64 only, and any bump of
fastembed/ort/ONNX Runtime or a model snapshot change invalidates them by
design. The revision and artifact digest are the target-independent half and
are the two values the product itself enforces.

Re-establish deliberately: repopulate a cache with the pinned stack, verify
revision and artifact bytes against upstream, re-derive both values through
the contract test, and update `JINA_REVISION` and `JINA_ARTIFACTS_SHA256` in
`crates/yams-embed/src/jina.rs`, this document, and
`scripts/release-reference.env` together in one reviewed change. Changing the
pin is a user-visible act: a build with a new pin will not load a cache that
holds only the previous snapshot, and tells its user to run
`YAMS_ALLOW_NET=1 yams --index` to download the new one. Superseded snapshots
are never deleted by Yams.
