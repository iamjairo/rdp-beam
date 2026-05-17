#!/usr/bin/env bash
# Regenerate the Tauri icon pack from the shared master SVG.
#
# Inputs:  ../../desktop-electron/build/icon.svg
# Outputs: 32x32.png, 128x128.png, 128x128@2x.png, icon.png, icon.icns, icon.ico, tray.png
#
# Requires: ImageMagick (`magick`) and macOS `iconutil`. Run from this directory.

set -euo pipefail
cd "$(dirname "$0")"

SVG="../../desktop-electron/build/icon.svg"
[ -f "$SVG" ] || { echo "missing master SVG at $SVG"; exit 1; }

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

for s in 16 32 64 128 256 512 1024; do
    magick -background none -density 384 "$SVG" -resize "${s}x${s}" "$tmp/icon-$s.png"
done

# Tauri's bundler expects these specific filenames:
cp "$tmp/icon-32.png"  32x32.png
cp "$tmp/icon-128.png" 128x128.png
cp "$tmp/icon-256.png" 128x128@2x.png
cp "$tmp/icon-512.png" icon.png

# .icns
iconset="$tmp/beam.iconset"
mkdir -p "$iconset"
cp "$tmp/icon-16.png"   "$iconset/icon_16x16.png"
cp "$tmp/icon-32.png"   "$iconset/icon_16x16@2x.png"
cp "$tmp/icon-32.png"   "$iconset/icon_32x32.png"
cp "$tmp/icon-64.png"   "$iconset/icon_32x32@2x.png"
cp "$tmp/icon-128.png"  "$iconset/icon_128x128.png"
cp "$tmp/icon-256.png"  "$iconset/icon_128x128@2x.png"
cp "$tmp/icon-256.png"  "$iconset/icon_256x256.png"
cp "$tmp/icon-512.png"  "$iconset/icon_256x256@2x.png"
cp "$tmp/icon-512.png"  "$iconset/icon_512x512.png"
cp "$tmp/icon-1024.png" "$iconset/icon_512x512@2x.png"
iconutil -c icns "$iconset" -o icon.icns

# .ico
magick "$tmp/icon-16.png" "$tmp/icon-32.png" "$tmp/icon-64.png" "$tmp/icon-128.png" "$tmp/icon-256.png" icon.ico

# Tray (22x22 monochrome template).
magick -background none -density 64 "$SVG" -resize 22x22 -colorspace gray tray.png

echo "regenerated:"
ls -la *.png *.icns *.ico
