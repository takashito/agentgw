#!/usr/bin/env bash
# Throw away what earlier builds left in target/. **Keeps the dependency cache** — those
# artifacts are the expensive ones (a cold build of the Linux target is ~9 minutes) and they
# are never stale: cargo rebuilds a dependency only when its version or features change.
#
#   scripts/prune-old-builds.sh [--dry-run]
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

# **Every checkout, not just this one.** Each worktree builds into its own target/, and whoever
# runs this (a hook, a person) is in one of them — the others would then never be swept. Measured:
# a day's work in one worktree left 3.6GB of incremental while the main checkout sat at 243MB.
# CARGO_TARGET_DIR, when set, is the one place everything lands, so it is the only one to sweep.
roots=()
if [ -n "${CARGO_TARGET_DIR:-}" ]; then
  roots+=("$CARGO_TARGET_DIR")
else
  while read -r dir; do
    [ -d "$dir/target" ] && roots+=("$dir/target")
  done < <(git -C "$repo_root" worktree list --porcelain 2>/dev/null | awk '/^worktree /{print $2}')
  # Not a git checkout (a tarball, a CI copy): fall back to this one
  [ ${#roots[@]} -gt 0 ] || roots+=("$repo_root/target")
fi

crate="$(sed -n 's/^name *= *"\(.*\)"/\1/p' "$repo_root/Cargo.toml" | head -1)"
age=30   # minutes

# **Never sweep under a running build.** A build in flight is writing exactly these files;
# 30 minutes is longer than a cold build, but a stalled or waiting one would look old.
if pgrep -qf '[c]argo|[r]ustc'; then
  echo "A build is running — nothing swept."
  exit 0
fi

for root in "${roots[@]}"; do
  [ -d "$root" ] || continue
  before="$(du -sk "$root" | cut -f1)"

  # Old incremental sessions: one directory per crate fingerprint, the live one is refreshed
  # by each build. They live at <profile>/incremental/<crate>-<hash> (one level deeper per target triple).
  stale_dirs=$(find "$root" -mindepth 3 -maxdepth 4 -type d -path '*/incremental/*' -name "${crate}-*" -mmin +$age 2>/dev/null || true)
  # Old copies of our own crate: rlib/rmeta/.o/.d and the test binaries, all named after it.
  stale_files=$(find "$root" -type f \( -name "*${crate}*" -o -name "*lib${crate}*" \) -mmin +$age ! -path '*/incremental/*' 2>/dev/null || true)

  if [ "${1:-}" = "--dry-run" ]; then
    printf '%s\n' $stale_dirs $stale_files | sed '/^$/d'
    echo "(dry run) $root: $(printf '%s\n' $stale_dirs $stale_files | sed '/^$/d' | wc -l | tr -d ' ') entries, $((before / 1024))MB"
    continue
  fi

  [ -n "$stale_dirs" ] && printf '%s\n' $stale_dirs | xargs -I{} rm -rf {}
  [ -n "$stale_files" ] && printf '%s\n' $stale_files | xargs -I{} rm -f {}

  after="$(du -sk "$root" | cut -f1)"
  echo "$root: $((before / 1024))MB → $((after / 1024))MB (freed $(( (before - after) / 1024 ))MB)"
done
