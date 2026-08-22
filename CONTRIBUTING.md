# Contributing

This repository requires Rust **1.97.1** and edition **2024**, pinned in
`rust-toolchain.toml`. Hosted CI installs the same toolchain.

The default branch is `main`. Hosted CI uses
`github.event.repository.default_branch` (currently `main`). The local
whitespace gate below uses `origin/main`; keep that remote ref current so
it matches the default branch.

Hosted CI is the source, documentation, and Linux path-regression gate.
Packaging, SBOM, the online Jina lane, and any soak or stress evidence stay
operator-only.

Before submitting a change, run:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
cargo build --workspace --all-features --release --locked
RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps --locked
base=$(git merge-base HEAD origin/main) && git diff --check "$base"..HEAD
```

Run the gate from the repository root. Keep the command flags aligned with
CI when changing workspace features or targets.

Keep fixtures synthetic and self-contained. Do not commit real memories,
queries, databases, model artifacts, absolute personal paths, transcripts,
or local workflow plans. The tracked `AGENTS.md` carries only the canonical
Project memory policy that `yams-wiki init` installs; do not add other agent
instructions to it or commit agent instructions elsewhere. Add focused tests
beside the crate that owns the behavior and preserve stable output and exit
contracts.

Record user-facing changes under the `[Unreleased]` heading in
`CHANGELOG.md` as part of the change that introduces them. The release
operator promotes that section to a version entry when cutting a release.
