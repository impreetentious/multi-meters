# Contributing to MultiMeters

Thank you for improving MultiMeters.

## Before opening a change

- Search existing issues and pull requests for related work; the bug and feature templates cover most cases.
- Keep changes focused and explain the user-facing reason for them.
- Never include API keys, access tokens, credential files, or private usage logs.
- Add regression coverage for behavior changes where practical.

## Local checks

Run the checks relevant to your change before opening a pull request:

```sh
node scripts/check-version-coherence.mjs
npm --prefix ui run build
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
```

Windows shell and installer changes should also be exercised on Windows.

## Pull requests

Describe what changed, why it changed, how it was verified, and any remaining platform limitations. Keep unrelated refactors in separate pull requests.
