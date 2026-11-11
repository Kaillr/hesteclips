#!/bin/sh
# Render the glyph's icons from the logo's SVGs (needs rsvg-convert and
# ImageMagick's magick).
#
# The glyph comes as two SVGs:
# - glyph.svg: the glyph alone in one colour, the gaps that keep the horse,
#   the sticks and the board apart cut out (transparent).
# - glyph-keyline.svg: the glyph in white, a dark keyline round it and in
#   those gaps, so it reads on a light bar as well as a dark one. Only ever
#   drawn small: at 32 px and below the keyline is a pixel or less; bigger,
#   it's a heavy black outline.
#
# What's made from them:
# - The app icon (glyph-icon/N.png): the window's, and Linux's icon theme's
#   at each size it has. From 48 px up, glyph.svg in white with a soft
#   shadow, which keeps it clear of a light background without an outline;
#   at 32 px and below glyph-keyline.svg, which reads better there (a shadow
#   that small is a faint smudge). PNG, not SVG: KDE draws SVG icons with Qt,
#   which ignores paint-order (the keyline's strokes covered the glyph) and,
#   in older versions, masks and filters.
# - The tray on Windows and Linux (tray/glyph-N.png): glyph-keyline.svg at
#   each size trays use (16 px at 100% scale, 20 at 125%, 24 at 150%, 32 at
#   200%, ...), drawn at the pixel size it's shown at, so it's never scaled
#   by Windows (that made it pixelated).
# - macOS's menu bar (tray-mac.png): glyph.svg, 32 px with 2 px clear around
#   it: macOS shows a menu bar icon 18 points high, and its own glyphs
#   (Wi-Fi, Focus) are about 16 of those. Shown as a template image: macOS
#   tints it to suit the bar, as its own icons are.
# - The empty library page (glyph.png): glyph.svg in white on transparent
#   (tinted black or white by the theme at runtime), 512 px in the middle of
#   a 768 canvas.
set -e
cd "$(dirname "$0")/../crates/app/assets"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

mkdir -p glyph-icon tray
for s in 16 22 24 32; do
    rsvg-convert -w $s -h $s glyph-keyline.svg -o glyph-icon/$s.png
done
for s in 48 64 96 128 192 256; do
    # The glyph a little smaller than the icon, for room round it for the
    # shadow: a wide one cast down, and a close one all round that keeps its
    # edges on a light background.
    g=$((s * 560 / 604))
    rsvg-convert -w $g -h $g glyph.svg | magick - -background none -gravity center -extent ${s}x${s} "$tmp/glyph.png"
    wide=$(awk "BEGIN { print $s * 0.025 }")
    drop=$(awk "BEGIN { d = int($s * 0.016 + 0.5); print (d < 1 ? 1 : d) }")
    near=$(awk "BEGIN { n = $s * 0.011; print (n < 0.7 ? 0.7 : n) }")
    magick -size ${s}x${s} xc:none \
        \( "$tmp/glyph.png" -alpha extract -blur 0x$wide -background black -alpha shape -channel A -evaluate multiply 0.55 +channel \) -geometry +0+$drop -compose over -composite \
        \( "$tmp/glyph.png" -alpha extract -blur 0x$near -background black -alpha shape -channel A -evaluate multiply 0.95 +channel \) -geometry +0+0 -compose over -composite \
        "$tmp/glyph.png" -geometry +0+0 -compose over -composite \
        glyph-icon/$s.png
done
for s in 16 20 24 28 32 40 48 64; do
    rsvg-convert -w $s -h $s glyph-keyline.svg -o tray/glyph-$s.png
done
rsvg-convert -w 32 -h 32 glyph.svg | magick - -background none -gravity center -extent 36x36 tray-mac.png
rsvg-convert -w 512 -h 512 glyph.svg | magick - -background none -gravity center -extent 768x768 glyph.png
echo "icons made"
