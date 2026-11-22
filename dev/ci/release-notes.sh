#!/bin/sh
# Print a release's section of the changelog, without its heading.
#   dev/ci/release-notes.sh vX.Y.Z [CHANGELOG.md]
# The section starts at "## vX.Y.Z" and ends at the next "## " heading.
set -eu
[ "$#" -ge 1 ] && [ "$#" -le 2 ] || {
  echo "usage: dev/ci/release-notes.sh vX.Y.Z [CHANGELOG.md]" >&2
  exit 2
}
tag=$1
file=${2:-$(dirname "$0")/../../CHANGELOG.md}
notes=$(awk -v h="## $tag" '
  $0 == h || index($0, h " ") == 1 { on = 1; next }
  on && /^## / { exit }
  on { print }
' "$file" | awk 'NF { seen = 1 } seen')
if [ -z "$notes" ]; then
  echo "$file has no section for $tag" >&2
  exit 1
fi
printf '%s\n' "$notes"
