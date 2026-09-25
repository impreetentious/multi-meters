# MultiMeters

MultiMeters is a native Windows tray app that puts AI coding subscription usage in one compact dashboard.

It supports Claude, Codex, Cursor, GitHub Copilot, Devin, Grok, OpenCode, OpenRouter, Z.ai and Antigravity.

## Features

- Live quota meters, reset countdowns, pacing and stale-data warnings
- Daily and 30-day cost and token totals from supported local usage logs
- Configurable providers, metrics, ordering, pins, theme, density and refresh cadence
- Windows tray controls, launch at login, global shortcut and quota notifications
- Secure API-key storage through Windows Credential Manager
- A read-only loopback API and script-friendly CLI

## Install

Download an MSI or NSIS installer from the latest GitHub release and launch MultiMeters from the Windows tray.

MultiMeters discovers supported local app and CLI credentials automatically. OpenRouter and Z.ai keys can be added in Settings. Credentials never appear in the dashboard API.

## Use

Left-click the tray icon to toggle the dashboard. Right-click it for Open, Settings and Quit. Press `Esc` to hide the flyout and `Ctrl+R` to refresh.

The local API listens on `http://127.0.0.1:6736` and exposes normalized usage at `/v1/limits`. The companion CLI prints the same data:

```text
MultiMeters [provider] [--force]
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

Build Windows installers with `cargo tauri build`.

## Contributing

Issues and pull requests are welcome on [GitHub](https://github.com/ItsMonarch04/multi-meters). Bug and feature templates are in place; see [CONTRIBUTING.md](CONTRIBUTING.md) for the checks a change is expected to leave green.

## License

Apache-2.0 © 2024-2026 Sidakpreet Singh — see [LICENSE](LICENSE).

---

**Version:** v0.2.5
