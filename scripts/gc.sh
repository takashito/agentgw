#!/usr/bin/env bash
# Throw away what earlier builds left in target/. **Keeps the dependency cache** — those
# artifacts are the expensive ones (a cold build of the Linux target is ~9 minutes) and they
# are never stale: cargo rebuilds a dependency only when its version or features change.
#
#   scripts/gc.sh [--dry-run]
#
# What piles up is *our own* crate. Every build writes a new `libagentgw-<hash>.rlib` (plus
# the test binaries) and a new incremental session, and cargo never deletes the old ones —
# measured: 10GB of incremental in 93 generations, 2.5GB of stale deps.
#
# The rule: our own artifacts are rewritten by **every** build, so any of them that has not
# been touched for a while belongs to a build that is over. 30 minutes is longer than a cold
# build of both targets, so a build in flight is never touched. Deleting one of these costs a
# recompile of this crate alone, never of a dependency.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)"
root="${CARGO_TARGET_DIR:-$repo_root/target}"
[ -d "$root" ] || { echo "No target directory at $root — nothing to do."; exit 0; }

crate="$(sed -n 's/^name *= *"\(.*\)"/\1/p' "$repo_root/Cargo.toml" | head -1)"
age=30   # minutes

# **Never sweep under a running build.** A build in flight is writing exactly these files;
# 30 minutes is longer than a cold build, but a stalled or waiting one would look old.
if pgrep -qf '[c]argo|[r]ustc'; then
  echo "A build is running — nothing swept."
  exit 0
fi

before="$(du -sk "$root" | cut -f1)"

# Old incremental sessions: one directory per crate fingerprint, the live one is refreshed
# by each build. They live at <profile>/incremental/<crate>-<hash> (one level deeper per target triple).
stale_dirs=$(find "$root" -mindepth 3 -maxdepth 4 -type d -path '*/incremental/*' -name "${crate}-*" -mmin +$age 2>/dev/null || true)
# Old copies of our own crate: rlib/rmeta/.o/.d and the test binaries, all named after it.
stale_files=$(find "$root" -type f \( -name "*${crate}*" -o -name "*lib${crate}*" \) -mmin +$age ! -path '*/incremental/*' 2>/dev/null || true)

if [ "${1:-}" = "--dry-run" ]; then
  printf '%s\n' $stale_dirs $stale_files | sed '/^$/d'
  echo "(dry run) $(printf '%s\n' $stale_dirs $stale_files | sed '/^$/d' | wc -l | tr -d ' ') entries, target is $((before / 1024))MB"
  exit 0
fi

[ -n "$stale_dirs" ] && printf '%s\n' $stale_dirs | xargs -I{} rm -rf {}
[ -n "$stale_files" ] && printf '%s\n' $stale_files | xargs -I{} rm -f {}

after="$(du -sk "$root" | cut -f1)"
echo "target: $((before / 1024))MB → $((after / 1024))MB (freed $(( (before - after) / 1024 ))MB)"
