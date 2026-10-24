#!/bin/bash
# Source + kernel snapshot.
#
# Copies the tree DIRECTLY, not via `git ls-files`: the graphics work is
# largely uncommitted, so a git-based snapshot silently omits exactly the files
# being edited. That bug made every backup earlier in this session useless for
# the DRM driver -- it reported success while capturing nothing.
set -u
D="${BACKUP_ROOT:-/tmp/dragos-backups}/graphics-$(date +%Y%m%d-%H%M%S)"
mkdir -p "$D"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
for f in $(find crates shared tools scripts esp/efi/boot -type f \( -name '*.rs' -o -name '*.asm' -o -name '*.c' -o -name '*.h' -o -name '*.sh' -o -name '*.elf' \) 2>/dev/null); do
  mkdir -p "$D/$(dirname "$f")"; cp "$f" "$D/$f" 2>/dev/null
done
N=$(find "$D" -type f | wc -l)
echo "$D" > "$D/PATH"
echo "backup[$1]: $D  ($N files)"
