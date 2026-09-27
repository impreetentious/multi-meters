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

Windows shell and installer changes should also be exercised on Windows. When only the front end
changed, `npm --prefix ui run dev` serves the interface against a local fixture and needs no Rust
toolchain.

Provider icons live in `ui/public/icons` and are served from there; `resources/icons` holds only
the 1024px app-icon master that `cargo tauri icon` generates the bundle icons from.

The three files in `resources/pricing` are compiled into the binary and price local usage
logs. Each one's `$comment` records where it came from and how to refresh it; bump its
`retrieved_at`/`updated_at` in the same change. Models missing from all three are excluded
from cost totals and named in the interface rather than guessed at.

## Pull requests

Describe what changed, why it changed, how it was verified, and any remaining platform limitations. Keep unrelated refactors in separate pull requests.
