#!/bin/bash
# Search semantics through the CLI: one-shot mode for the matching rules and
# a short serve/search/status round trip. Usage: tests/cli_test.sh [LINCE_BIN]
set -euo pipefail

bin=$(realpath "${1:-target/release/lince}")
work=$(mktemp -d)
daemon=""
cleanup() {
  [[ -z $daemon ]] || kill "$daemon" 2>/dev/null || true
  rm -rf -- "$work"
}
trap cleanup EXIT

passed=0
check() {
  if ! "$@"; then
    echo "FAILED (line ${BASH_LINENO[0]}): $*" >&2
    exit 1
  fi
  passed=$((passed + 1))
}
contains() { [[ $1 == *"$2"* ]]; }
lacks() { [[ $1 != *"$2"* ]]; }

home="$work/home"
other="$work/other"
mkdir -p "$home/Documents" "$home/.secret" "$home/.config/tool/fresh-dir/deeper/deepest/bottom" \
  "$home/node_modules" "$home/orderdir" "$home/Música" "$other/disk"
touch "$home/Documents/fresh-note.md" "$home/Documents/fresh-image.png" "$home/Documents/fresh-[literal].md" \
  "$home/.secret/fresh-hidden.md" "$home/node_modules/fresh-vendor.md" "$home/Documents/canción.md" \
  "$home/Documents/espana.md" "$home/Música/tema.mp3" "$home/Documents/omarchy-find.md" \
  "$home/order-file.md" "$home/orderdir/child.md" "$other/disk/fresh-remote.md" \
  "$home/.config/tool/fresh-dir/deeper/deepest/bottom/fresh-deep.md"
ln -s "$home/Documents/fresh-note.md" "$home/fresh-link.md"

one() { "$bin" oneshot --root "$home" --exclude node_modules "$@"; }

# Kinds, extensions, hidden entries, excludes, symlinks.
docs=$(one --query fresh --kind f --extensions md)
check contains "$docs" "fresh-note.md"
check lacks "$docs" "fresh-image.png"
check lacks "$docs" "fresh-hidden.md"
check lacks "$docs" "fresh-vendor.md"
check lacks "$docs" "fresh-link.md"
check contains "$(one --query fresh --kind f --hidden)" "fresh-hidden.md"
check contains "$(one --query fresh --kind d --hidden)" "fresh-dir"

# Scopes and depth (like fd with search paths and --max-depth).
scoped=$(one --query fresh --kind d --hidden --scope .config --max-depth 2)
check contains "$scoped" ".config/tool/fresh-dir"
check test -z "$(one --query fresh --kind f --hidden --scope .config --max-depth 3)"
check contains "$(one --query fresh --kind f --hidden --scope .config)" "fresh-deep.md"
check test -z "$(one --query fresh --kind f --max-depth 1)"

# Literal metacharacters, accents both ways, path terms, the root anchor.
check contains "$(one --query '[literal]' --kind f)" "fresh-[literal].md"
check contains "$(one --query cancion --kind f)" "canción.md"
check contains "$(one --query españa --kind f)" "espana.md"
check contains "$(one --query 'musica tema' --kind f)" "Música/tema.mp3"
check contains "$(one --query documents/fresh-note --kind f)" "fresh-note.md"
check test -z "$(one --query "$(basename "$work")" --kind f)"

# Name matches before path matches; fuzzy only as a fallback.
mapfile -t ordered < <(one --query order --kind f)
check test "${ordered[0]}" == "$home/order-file.md"
check test "${ordered[1]}" == "$home/orderdir/child.md"
check contains "$(one --query omfind --kind f)" "omarchy-find.md"

# Several roots, each with its own anchor.
check contains "$("$bin" oneshot --root "$home" --root "$other" --query fresh-remote --kind f)" "other/disk/fresh-remote.md"

# Daemon round trip.
sock="$work/lince.sock"
"$bin" serve --socket "$sock" --root "$home" --root "$other" --exclude node_modules --snapshot "$work/s.lnc" 2>/dev/null &
daemon=$!
for _ in {1..200}; do [[ -S $sock ]] && break; sleep 0.02; done
check contains "$("$bin" search --socket "$sock" fresh note)" "fresh-note.md"
touch "$home/Documents/brand-new.txt"
check contains "$("$bin" search --socket "$sock" brand-new)" "brand-new.txt"
status=$("$bin" status --socket "$sock")
check contains "$status" '"fresh": true'
check contains "$status" '"v": 1'
check contains "$("$bin" search --socket "$sock" --json fresh)" '"v":1'
check contains "$(printf '%s\n' '{"op":"search","query":"x","roots":["/nowhere"]}' | "$bin" query --socket "$sock")" "root not indexed"

echo "$passed CLI checks passed"
