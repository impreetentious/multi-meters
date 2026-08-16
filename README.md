# MultiMeters

MultiMeters is a native Windows tray app that puts AI coding subscription usage in one compact dashboard.

It supports Claude, Codex, Cursor, GitHub Copilot, Devin, Grok, OpenCode, OpenRouter, Z.ai, and Antigravity.

## Features

- Live quota meters, reset countdowns, pacing, and stale-data warnings
- Daily and 30-day cost and token totals from supported local usage logs
- Configurable providers, metrics, ordering, pins, theme, density, and refresh cadence
- Windows tray controls, launch at login, global shortcut, and quota notifications
- Secure API-key storage through Windows Credential Manager
- A read-only loopback API and script-friendly CLI

## Install

Download an MSI or NSIS installer from the latest GitHub release and launch MultiMeters from the Windows tray.

MultiMeters discovers supported local app and CLI credentials automatically. OpenRouter and Z.ai keys can be added in Settings. Credentials never appear in the dashboard API.

## Use

Left-click the tray icon to toggle the dashboard. Right-click it for Open, Settings, and Quit. Press `Esc` to hide the flyout and `Ctrl+R` to refresh.

The local API listens on `http://127.0.0.1:6736` and exposes normalized usage at `/v1/limits`. The companion CLI prints the same data:

```text
multimeters [provider] [--force]
```

## Privacy

MultiMeters runs locally. It reads supported credentials and usage data only to contact provider usage endpoints or calculate local totals; it does not proxy prompts or responses. Settings, cache data, and logs remain under `%LOCALAPPDATA%\MultiMeters`.

## Develop

Development requires Rust stable, Node.js 20 or later, and the standard [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/) for Windows.

```sh
npm --prefix ui ci
npm --prefix ui run build
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
cargo tauri dev
```

Build Windows installers with `cargo tauri build`.

## License

MultiMeters is available under the [MIT License](LICENSE).

**Version:** v0.1.5
