# MultiMeters

MultiMeters is a native Windows tray app that puts AI coding subscription usage in one compact dashboard.

It supports Claude, Codex, Cursor, GitHub Copilot, Devin, Grok, OpenCode, OpenRouter, Z.ai and Antigravity.

## Features

- Live quota meters, reset countdowns, pacing and stale-data warnings
- Daily and 30-day cost and token totals from supported local usage logs
- Configurable providers, metrics, ordering, pins, theme, density, refresh cadence and flyout behaviour
- Windows tray controls with a live tooltip, launch at login, global shortcut and quota notifications
- Secure API-key storage through Windows Credential Manager
- A read-only loopback API and script-friendly CLI

## Install

No binary release is published yet. Build the MSI and NSIS installers from source with `cargo tauri build` — see [Develop](#develop) — then launch MultiMeters from the Windows tray.

MultiMeters discovers supported local app and CLI credentials automatically. OpenRouter and Z.ai keys can be added in Settings. Credentials never appear in the dashboard API.

## Use

Left-click the tray icon to toggle the dashboard. Right-click it for Open, Settings and Quit. Hovering it shows your pinned meters without opening the flyout. Press `Esc` to hide the flyout — once to leave a text field, again to close — and `Ctrl+R` to refresh.

The flyout closes as soon as it loses focus. Turn off **Close When Unfocused** in Settings to keep it open beside your editor.

The local API listens on `http://127.0.0.1:6736`. It exposes normalized usage at `/v1/limits` and the flyout's own line-by-line shape at `/v1/usage`; both accept a `/{provider}` suffix.

A companion CLI prints the same normalized data. The installer does not bundle it — build it with `cargo build -p multimeters-cli --release` and run it from `target/release`:

```text
multimeters [provider] [--force]
```

## Privacy

MultiMeters runs locally. It reads supported credentials and usage data only to contact provider usage endpoints or calculate local totals; it does not proxy prompts or responses. Settings, cache data and logs remain under `%LOCALAPPDATA%\MultiMeters`.

The loopback API is unauthenticated: it binds `127.0.0.1`, so any program running under your account can read it. Browser requests are answered only for loopback origins, which stops pages you visit from reading it.

## Limitations

- Windows only. Installers and the tray shell target Windows; the core library and CLI also build on macOS and Linux.
- Live meters need each provider's own app or CLI signed in on the same machine. MultiMeters has no account of its own and cannot add one.
- Provider usage endpoints are private and undocumented, so a provider-side change can stall a meter until MultiMeters catches up.
- Cost and token totals for Claude, Codex, Cursor and Grok are estimated from local logs priced against a bundled catalog snapshot; OpenCode reports measured cost. Models missing from the snapshot are excluded from totals and named in the UI.

## Develop

Development requires Rust stable, Node.js 20 or later and the standard [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/) for Windows.

```sh
npm --prefix ui ci
npm --prefix ui run build
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
cargo tauri dev
```

`npm --prefix ui run dev` serves the interface on its own at `http://localhost:1420`, backed by a local fixture instead of the Tauri IPC. It needs no Rust toolchain and runs on any OS, which makes it the fast loop for front-end work; the fixture is excluded from production bundles.

Build Windows installers with `cargo tauri build`.

## Contributing

Issues and pull requests are welcome on [GitHub](https://github.com/impreetentious/multi-meters). Bug and feature templates are in place; see [CONTRIBUTING.md](CONTRIBUTING.md) for the checks a change is expected to leave green.

## License

Apache-2.0 © 2024-2026 Sidakpreet Singh — see [LICENSE](LICENSE).

---

**Version:** v0.2.9
