# Beam — Electron desktop client

Native Electron shell for the Beam web client. Provides:
- `beam://` URL protocol handler
- System tray and single-instance enforcement
- OS clipboard with image support (`clipboard.readImage` / `writeImage`)
- OS keyring credential storage (`keytar`)
- Auto-update from GitHub releases (`electron-updater`)
- Fullscreen + always-on-top window controls

The web client itself lives in `../web/`. This package consumes the built
`web/dist/` output unchanged — there is no React/Vue/etc. duplication.

## Build

```bash
cd desktop-electron
npm ci
npm run dist:linux   # AppImage + .deb + .snap
npm run dist:mac     # .dmg
npm run dist:win     # NSIS installer
```

Outputs land in `desktop-electron/dist-bundle/`.

## Development

```bash
npm run dev          # builds web + ts, launches electron
```

The `__BEAM_NATIVE__` bridge it injects must match the contract declared in
[`web/src/native-bridge.ts`](../web/src/native-bridge.ts). Tauri's shell
implements the same contract, so the renderer code is shell-agnostic.

## Status

**Scaffolded but not yet wired into the release pipeline.** The release
workflow (`.github/workflows/release.yml`) only builds the `.deb` for
`beam-server`/`beam-agent` today. An Electron build matrix needs to be
added before the first artefact ships.
