#!/bin/sh
# Render the tray / menu bar icons from the logo's SVGs (needs rsvg-convert).
#
# - macOS: icon-tray.svg (the glyph in one colour, gaps cut out), 32 px with
#   2 px clear around it: macOS shows a menu bar icon 18 points high, and its
#   own glyphs (Wi-Fi, Focus) are about 16 of those. Shown as a template image:
#   macOS tints it to suit the bar, as its own icons are.
# - Windows: icon-windows.svg (white glyph, dark outline: reads on light and
#   dark taskbars) at each size the tray uses (16 px at 100% scale, 20 at 125%,
#   24 at 150%, 32 at 200%, ...): drawn at the pixel size it's shown at, so it's
#   never scaled by Windows (that made it pixelated).
set -e
cd "$(dirname "$0")/../crates/app/assets"
rsvg-convert -w 32 -h 32 icon-tray.svg | magick - -background none -gravity center -extent 36x36 tray-mac.png
mkdir -p tray
for s in 16 20 24 28 32 40 48 64; do
    rsvg-convert -w $s -h $s icon-windows.svg -o tray/windows-$s.png
done
# The empty library page: the glyph in white on transparent (tinted black or white
# by the theme at runtime) and its blurred silhouette (the drop shadow, which
# follows the horse instead of a box). Both on a 768 canvas, glyph 512 in the middle.
rsvg-convert -w 512 -h 512 icon-tray.svg | magick - -background none -gravity center -extent 768x768 glyph.png
magick glyph.png -channel A -blur 0x18 -level 0,70% +channel -fill black -colorize 100 glyph-shadow.png
echo "tray icons made"
