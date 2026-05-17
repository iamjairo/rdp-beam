# Electron build assets

| File | Purpose | Source |
|---|---|---|
| `icon.svg` | Master vector | hand-authored |
| `icon.png` | 512×512 generic | derived from `icon.svg` |
| `icon.icns` | macOS bundle icon | derived (multi-res 16→1024) |
| `icon.ico` | Windows multi-resolution | derived (16/32/64/128/256) |
| `tray.png` | 22×22 menubar template | derived, grayscale |
| `entitlements.mac.plist` | Hardened-runtime entitlements | hand-authored (see [`../NOTARIZATION.md`](../NOTARIZATION.md)) |
| `entitlements.mac.inherit.plist` | Child-process entitlements | hand-authored |

## Design

Filled disc with a soft offset highlight evoking a beam of light on a curved
surface. Matches the existing `web/public/favicon.svg` motif (filled circle)
but with depth so it stays legible at 16×16. No text inside — small icon
slots can't render readable text. Two-stop blue gradient (`#5a9bff → #1a3a78`)
distinguishes it from generic grey UI chrome at every size.

## Regenerate

```bash
cd desktop-electron/build
./regenerate-icons.sh
```

Requires ImageMagick (`magick`) and macOS `iconutil`. The script reads
`icon.svg` and rewrites the four bitmap outputs and the tray asset.
