#!/bin/sh
# Render the registry icons of desktop/assets/registry-icons.svg to the PNGs in
# desktop/assets/registry, 64 x 64 each. Needs rsvg-convert (librsvg).
#   desktop/assets/make-registry-icons.sh
set -eu
cd "$(dirname "$0")/../.."
sprite=desktop/assets/registry-icons.svg
out=desktop/assets/registry
size=64
symbols=$(mktemp)
trap 'rm -f "$symbols"' EXIT
# The sprite without its root element: the symbols and their definitions.
sed '1d;$d' "$sprite" > "$symbols"
for id in $(sed -n 's/.*<symbol id="\([^"]*\)".*/\1/p' "$sprite"); do
  # Symbols that only reuse another one share its PNG.
  if grep -q "<symbol id=\"$id\"[^>]*><use href=" "$sprite"; then
    continue
  fi
  {
    echo "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"$size\" height=\"$size\">"
    cat "$symbols"
    echo "<use href=\"#$id\" width=\"$size\" height=\"$size\"/>"
    echo "</svg>"
  } | rsvg-convert -w "$size" -h "$size" -o "$out/$id.png"
done
