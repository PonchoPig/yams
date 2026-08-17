# Yams vX.Y.Z

<one-paragraph summary of what changed for users>

## Provenance

- Release commit: `<40-hex sha>` on `main`
- Validated on: macOS <version> (Apple Silicon, aarch64-apple-darwin)
- rustc: `<rustc --version output>`
- `Cargo.lock` SHA-256: `<shasum -a 256 Cargo.lock>`
- fastembed 5.17.4, ort 2.0.0-rc.13 (ONNX Runtime 1.28.0), hf-hub 0.5.0,
  tokenizers 0.22.2 (update if the lockfile changed)
- Frozen Jina signature the release gate validated against: see
  `docs/release/jina-reference.md` at the release commit
- Release gate: full source gate + complete release-candidate gate
  (`YAMS_RELEASE_TEST_ALLOW_NET=1` + `scripts/release-reference.env`)
  passed on the release commit

## Install

Supported channel: `brew install ponchopig/yams/yams`.
Direct tarball downloads are quarantined by Gatekeeper (binaries are
ad-hoc signed only) and are not a supported install path.

## Known limitations

- The first-run model download (~550 MB) is not yet bounded or resumable;
  interrupt with Ctrl-C and rerun if it stalls.
- Apple Silicon only; macOS floor declared :sonoma, validated on the
  version listed above.
- `yams`, `memory-search`, `yams-service`, and `yams-wiki` all report
  their package version with `--version`.
