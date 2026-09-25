#!/usr/bin/env bash
# Upload the output of `cargo dist` to a GitHub release. **Doesn't build** (that's cargo dist's job).
#
#   scripts/release.sh
#
#   scripts/release.sh --replace      # re-upload over a tag someone else's commit already has
#
# The tag is the version in Cargo.toml. If the tag already exists **on this same commit**, the
# assets are replaced — that is a retry. If it exists on a different commit, this stops: someone
# else has released that version, and `update` compares version numbers, not binaries, so every
# machine would say "already on it" and never take ours.
#
# Assets are named `agentgw-<triple>` — install.sh and add-machine fetch them by that name —
# each with an `agentgw-<triple>.sha256` beside it, which `update` checks the download against.
set -euo pipefail

replace=0
for arg in "$@"; do
  case "$arg" in
    --replace) replace=1 ;;
    *) echo "Unknown argument: $arg (only --replace)" >&2; exit 2 ;;
  esac
done

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
  # The checksum `update` compares a download with. Just the hex digits
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
  # **Whose commit is that tag on?** Two branches bumping to the same version is the one way
  # a build gets uploaded and then never installed: the machines already report that number,
  # so the rollout skips them (seen four times on 2026-09-25, once for half an hour).
  git -C "$repo_root" fetch --tags --quiet origin 2>/dev/null || true
  tagged="$(git -C "$repo_root" rev-parse -q --verify "refs/tags/$tag^{commit}" || true)"
  head_commit="$(git -C "$repo_root" rev-parse HEAD)"
  if [ -n "$tagged" ] && [ "$tagged" != "$head_commit" ] && [ "$replace" = 0 ]; then
    echo "Stopped. $tag is already released, from ${tagged:0:7} — this is ${head_commit:0:7}." >&2
    echo "Someone else took that version. Raise it in Cargo.toml and build again," >&2
    echo "or pass --replace if that release really should become this commit." >&2
    exit 1
  fi
  gh release upload "$tag" "$staging"/* --clobber
else
  gh release create "$tag" "$staging"/* --title "$tag" --notes "agentgw $ver"
fi
