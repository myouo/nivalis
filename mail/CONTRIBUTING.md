# Contributing to Nivalis Mail

## Atomic commits

Every commit must represent one independently reviewable and revertible change.

- Keep unrelated refactors, features, documentation, and generated artifacts in separate commits.
- Include the tests and directly affected documentation with the change they validate.
- Do not leave a commit in a knowingly unbuildable intermediate state.
- Use an imperative subject that describes the single outcome of the commit.

Before committing Rust or Slint changes, run:

```bash
cargo fmt --check
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
```

Release, renderer, or memory-sensitive changes must also run `cargo build --release` and the relevant procedure from `memory-report.md`.
