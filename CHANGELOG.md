# Changelog

User-facing changes to Yams, newest first. Each released version links
to its full release notes under `docs/release/notes/`, which carry the
provenance record: the release commit, the validated toolchain, the
`Cargo.lock` checksum, and the release gate that passed.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

Nothing yet.

## [0.1.0] — 2026-08-17

First public release ([full release notes](docs/release/notes/v0.1.0.md)).

### Added

- `yams`: offline semantic search over a repository's `.agents/memory`
  pages, matching on exact words and meaning, with a confidence gate
  ("no confident match" instead of noise), JSON output, and index
  maintenance (`--index`, `--projects`, `--stats`, `--gc`, `--all`).
- `yams-wiki`: repository-memory initialization in explicit
  inspect → plan → approve → apply steps, page validation (`check`,
  `compat`), catalog maintenance (`catalog`), transactional page
  writes (`write`), and machine-readable interface versions
  (`capabilities`).
- `yams-service`: optional background service that keeps the search
  model warm between queries (`brew services start yams`).
- `memory-search`: compatibility launcher for `yams`.
- Supply-chain-pinned embedding model: downloads come from one fixed,
  known revision and are checksum-verified before use, online or
  offline.
- Homebrew distribution (`brew install ponchopig/yams/yams`) for
  macOS 14+ on Apple Silicon, with a tarball, `SHA256SUMS`, and a
  CycloneDX SBOM attached to the release.

[Unreleased]: https://github.com/PonchoPig/yams/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/PonchoPig/yams/releases/tag/v0.1.0
