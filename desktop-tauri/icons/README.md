# Tauri icon assets

The Tauri bundler expects specific filenames. Each one is derived from the
shared master SVG at `../../desktop-electron/build/icon.svg` so both shells
ship the same brand mark.

| File | Purpose |
|---|---|
| `32x32.png` | Smallest tray / launcher size (required by Tauri bundler) |
| `128x128.png` | Standard launcher size (required) |
| `128x128@2x.png` | HiDPI 256×256 (required) |
| `icon.png` | 512×512 generic |
| `icon.icns` | macOS bundle icon (multi-res 16→1024) |
| `icon.ico` | Windows multi-resolution (16/32/64/128/256) |
| `tray.png` | 22×22 menubar template, grayscale |

## Design

Same lockup as the Electron shell — filled blue disc with a soft offset
highlight. See [`../../desktop-electron/build/README.md`](../../desktop-electron/build/README.md)
for the design rationale.

## Regenerate

```bash
cd desktop-tauri/icons
./regenerate-icons.sh
```

Requires ImageMagick (`magick`) and macOS `iconutil`. Run after editing the
shared master SVG.
