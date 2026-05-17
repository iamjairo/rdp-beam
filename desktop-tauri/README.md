# Beam — Tauri desktop client

Lean native shell (~10MB) for the Beam web client. Same capabilities as the
Electron shell but via the system webview instead of bundled Chromium:
- `beam://` URL handler (`tauri-plugin-deep-link`)
- System tray and single-instance enforcement
- Native clipboard (`tauri-plugin-clipboard-manager`)
- OS keyring credential storage (`keyring` crate)
- Auto-update from GitHub releases (`tauri-plugin-updater`)

## Build

```bash
cd desktop-tauri
cargo install tauri-cli --version "^2"
cargo tauri build               # produces .deb, .AppImage, .dmg, .msi
```

## Development

```bash
cargo tauri dev                 # runs web dev server + tauri shell
```

## Renderer contract

The shim injected on window startup matches the `BeamNative` interface in
[`web/src/native-bridge.ts`](../web/src/native-bridge.ts). Keep both shells
in sync — the renderer is shell-agnostic by design.

## WebCodecs note

Tauri uses the system webview: WebKitGTK on Linux, WKWebView on macOS,
WebView2 on Windows. WebCodecs support in WebKitGTK is younger than in
Chromium, so verify decode performance on Linux before depending on this
shell over the Electron one for production deployments.

## Update signing

Updater bundles are signed with a minisign-format keypair; the public key
is committed in `tauri.conf.json`. Operator runbook (rotation, leak
response, CI secrets): [`SIGNING.md`](SIGNING.md).

## Status

**Scaffolded but not wired into the release pipeline.** The release
workflow needs an `os: [ubuntu-24.04, macos-14, windows-2022]` job that
runs `cargo tauri build` and uploads artefacts. Until that lands, builds
are manual.
