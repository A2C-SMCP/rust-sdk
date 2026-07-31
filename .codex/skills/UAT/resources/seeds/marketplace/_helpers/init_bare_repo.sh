#!/usr/bin/env bash
# usage: init_bare_repo.sh <worktree-src> <work-dst> <bare-out>
#
# Copies worktree-src into work-dst, replaces every _seeds.manifest with the
# referenced _common/<x> contents, then `git init` + commit + push to bare-out.
# Prints the bare repo path (caller can prefix with file://).
#
# 语言无关：直接复用自 python-sdk。Rust / Python 共用同一 marketplace 仓库 fixture。
set -Eeuo pipefail

WT_SRC="$1"
WT_DST="$2"
BARE="$3"

# 1. Copy worktree skeleton
cp -R "$WT_SRC" "$WT_DST"

# 2. Resolve seeds root (used to locate _common/<x>)
SEEDS_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"

# 3. For every _seeds.manifest, replace with referenced _common content
while IFS= read -r m; do
  src=$(awk '/^source:/{print $2}' "$m")
  dst=$(dirname "$m")
  rm -f "$m"
  case "$src" in
    _common/*) ;;
    *) echo "init_bare_repo: unsupported source spec: $src" >&2; exit 2 ;;
  esac
  cp -R "$SEEDS_ROOT/$src"/. "$dst/"
done < <(find "$WT_DST" -name "_seeds.manifest" -type f)

# 4. git init the dst + bare push
git init --quiet --initial-branch=main "$WT_DST"
git -C "$WT_DST" -c user.email=seed@uat -c user.name=seed add -A
git -C "$WT_DST" -c user.email=seed@uat -c user.name=seed commit -q -m "uat seed snapshot"

git init --quiet --bare --initial-branch=main "$BARE"
git -C "$WT_DST" push --quiet "$BARE" HEAD:refs/heads/main

echo "$BARE"
