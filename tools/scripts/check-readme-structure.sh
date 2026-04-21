#!/usr/bin/env bash
# Enforce bidirectional sync between README.md's ## Project Structure block and
# the tracked filesystem:
#   (a) every path listed in the block exists on disk;
#   (b) every tracked path at a documented depth is listed in the block.
#
# A directory is "documented" at its depth if it has any children listed under
# it (including the repository root, which is always documented). Directories
# mentioned only as a prefix of a file path (e.g. `.config/containers.conf`)
# do NOT make their interior documented — only the leading component counts at
# the parent's level.
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

readme="README.md"
section="Project Structure"

block="$(awk -v h="$section" '
  $0 ~ "^## "h"$" { in_sec = 1; next }
  in_sec && /^## / { exit }
  in_sec && /^```/ { in_block = !in_block; next }
  in_sec && in_block { print }
' "$readme")"

if [[ -z "$block" ]]; then
  echo "could not find fenced block under '## $section' in $readme" >&2
  exit 2
fi

# Parse into TAB-separated records: parent<TAB>child<TAB>full_path
records="$(awk -v unit=2 '
  function clear_deeper(d,    k) {
    for (k in stack) if (k+0 > d) delete stack[k]
  }
  NF == 0 { next }
  {
    i = 0
    while (i < length($0) && substr($0, i+1, 1) == " ") i++
    depth = int(i / unit)
    name = $1
    bare = name; sub("/$", "", bare)
    first = bare; sub("/.*$", "", first)
    parent = (depth == 0) ? "." : stack[depth-1]
    full = (parent == ".") ? bare : parent "/" bare
    child = (bare ~ /\//) ? first : bare
    print parent "\t" child "\t" full
    if (name ~ /\/$/) {
      stack[depth] = full
      clear_deeper(depth)
    }
  }
' <<< "$block")"

declare -A listed=()
declare -a full_paths=()
while IFS=$'\t' read -r parent child full; do
  [[ -z "$parent" ]] && continue
  listed["$parent"]+="$child"$'\n'
  full_paths+=("$full")
done <<< "$records"

missing=()
for p in "${full_paths[@]}"; do
  [[ -e "$p" ]] || missing+=("$p")
done

undocumented=()
for parent in "${!listed[@]}"; do
  if [[ "$parent" == "." ]]; then
    mapfile -t tracked < <(git ls-tree --name-only HEAD)
  else
    mapfile -t tracked < <(git ls-tree --name-only "HEAD:$parent")
  fi
  for t in "${tracked[@]}"; do
    if ! grep -qxF "$t" <<<"${listed[$parent]}"; then
      [[ "$parent" == "." ]] && undocumented+=("$t") || undocumented+=("$parent/$t")
    fi
  done
done

status=0
if (( ${#missing[@]} )); then
  echo "README $section lists paths that don't exist:" >&2
  printf '  %s\n' "${missing[@]}" >&2
  status=1
fi
if (( ${#undocumented[@]} )); then
  echo "Tracked paths missing from README $section (at documented depth):" >&2
  printf '  %s\n' "${undocumented[@]}" >&2
  status=1
fi

exit "$status"
