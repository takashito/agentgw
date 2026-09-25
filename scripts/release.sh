#!/usr/bin/env bash
# Upload the output of `cargo dist` to a GitHub release. **Doesn't build** (that's cargo dist's job).
#
#   scripts/release.sh
#
# The tag is the version in Cargo.toml. If the tag already exists, only the assets are replaced.
# Assets are named `agentgw-<triple>` — install.sh and add-machine fetch them by that name —
# each with an `agentgw-<triple>.sha256` beside it, which `upgrade` checks the download against.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# CARGO_TARGET_DIR may be set per developer, so don't hard-code where builds land
build_dir="${CARGO_TARGET_DIR:-$repo_root/target}"
ver="$(sed -n 's/^version *= *"\(.*\)"/\1/p' "$repo_root/Cargo.toml" | head -1)"
tag="v$ver"

# Stage the builds under names with their <triple>.
# **Skip `target/release/`** — it has no triple, so there's no telling which platform it is for
staging="$(mktemp -d)"
trap 'rm -rf "$staging"' EXIT
found=0
for out in "$build_dir"/*/release/agentgw; do
  [ -f "$out" ] || continue
  triple="$(basename "$(dirname "$(dirname "$out")")")"
  cp "$out" "$staging/agentgw-$triple"
  # The checksum `upgrade` compares a download with. Just the hex digits
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$out" | cut -d' ' -f1 > "$staging/agentgw-$triple.sha256"
  else
    shasum -a 256 "$out" | cut -d' ' -f1 > "$staging/agentgw-$triple.sha256"
  fi
  echo "  agentgw-$triple ($(du -h "$out" | cut -f1))"
  found=$((found + 1))
done
if [ "$found" = 0 ]; then
  echo "Stopped. Nothing to upload — run cargo dist first." >&2
  exit 1
fi

echo "==> Uploading $found asset(s) to $tag"
if gh release view "$tag" >/dev/null 2>&1; then
  gh release upload "$tag" "$staging"/* --clobber
else
  gh release create "$tag" "$staging"/* --title "$tag" --notes "agentgw $ver"
fi
