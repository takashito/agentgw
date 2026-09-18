#!/usr/bin/env bash
# cargo dist の成果物を GitHub release に上げる。**ビルドはしない**(それは cargo dist の仕事)。
#
#   scripts/release.sh
#
# tag は Cargo.toml の version。同じ tag が既にあれば、資産だけ差し替える。
# 資産名は `agentgw-<triple>` — 入れる側(install.sh / add-child)はこの名前で引く。
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# ワーカーごとに CARGO_TARGET_DIR を分けている場合があるので、成果物は決め打ちで探さない
build_dir="${CARGO_TARGET_DIR:-$repo_root/target}"
ver="$(sed -n 's/^version *= *"\(.*\)"/\1/p' "$repo_root/Cargo.toml" | head -1)"
tag="v$ver"

# 成果物を <triple> 付きの名前で一時ディレクトリに並べる。
# **`target/release/` は拾わない** — triple が付かないので、どのマシン用か分からない
staging="$(mktemp -d)"
trap 'rm -rf "$staging"' EXIT
found=0
for out in "$build_dir"/*/release/agentgw; do
  [ -f "$out" ] || continue
  triple="$(basename "$(dirname "$(dirname "$out")")")"
  cp "$out" "$staging/agentgw-$triple"
  echo "  agentgw-$triple ($(du -h "$out" | cut -f1))"
  found=$((found + 1))
done
if [ "$found" = 0 ]; then
  echo "中止しました。成果物がありません。先に cargo dist を回してください。" >&2
  exit 1
fi

echo "==> $tag に $found 個の資産を上げます"
if gh release view "$tag" >/dev/null 2>&1; then
  gh release upload "$tag" "$staging"/* --clobber
else
  gh release create "$tag" "$staging"/* --title "$tag" --notes "agentgw $ver"
fi
