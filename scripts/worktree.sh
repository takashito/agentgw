#!/usr/bin/env bash
# Add a git worktree whose target/ starts out with this checkout's dependency cache, for free.
#
#   scripts/worktree.sh <name> [path] [base-ref]   # default path: worktrees/<name>
#
# **Why not share one target/ between checkouts.** Cargo takes one lock on the whole build
# directory, so a second build anywhere in the repo waits for the first to finish (seen here:
# `Blocking waiting for file lock on build directory`, a 2m35s wall for 73s of work). Separate
# target/ directories remove the wait, but each one then rebuilds the same ~150 dependencies and
# costs another 10GB+ on a disk that is 79% full.
#
# So the new worktree gets its own target/ that **shares the blocks** of this one. On APFS this is
# `cp -c` (a clone: no bytes copied, and a later write to either copy allocates only what changed);
# elsewhere it falls back to hardlinks. Either way the dependency artifacts are the same data on
# disk, and nothing is rebuilt.
#
# Two things are left out, on purpose:
#   - incremental/  — the largest part (measured: 10GB of 15GB) and worthless to a different
#     checkout, whose sources hash differently
#   - our own crate's artifacts — they belong to the other checkout's path, and cargo rebuilds
#     them there anyway; copying them only invites a stale one to be picked up
set -euo pipefail

name="${1:-}"
if [ -z "$name" ]; then
  echo "usage: scripts/worktree.sh <name> [path] [base-ref]" >&2
  exit 2
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)"
# **Where Claude Code puts them anyway** (`.claude/worktrees/`, git-ignored): still inside this
# project, so a checkout of it never lands beside it, and a worktree made by this script and one made
# by Claude Code itself end up in the same folder to find, move or delete.
dest="${2:-$repo_root/.claude/worktrees/$name}"
base="${3:-origin/main}"
crate="$(sed -n 's/^name *= *"\(.*\)"/\1/p' "$repo_root/Cargo.toml" | head -1)"

# **stdout is the path, nothing else.** A `WorktreeCreate` hook hands the path it prints back to
# Claude Code as the session's working directory, so every word meant for a person goes to stderr.
exec 3>&1 1>&2

git -C "$repo_root" worktree add -b "$name" "$dest" "$base"

# Claude Code marks every worktree it creates with git by writing the base commit to CLAUDE_BASE in
# the worktree's git metadata. Its cleanup only touches worktrees carrying that mark, and ExitWorktree
# refuses to remove one without it ("Could not verify worktree state"). This hook replaces that
# creation, so it has to leave the same mark, or every worktree made here becomes ours to sweep by hand.
# ponytail: CLAUDE_BASE is an undocumented file name, found by diffing a native worktree against one
# of ours. If a Claude Code upgrade stops removing these, diff the two again and update the name.
git -C "$repo_root" rev-parse "$base" | tr -d '\n' \
  > "$(git -C "$dest" rev-parse --absolute-git-dir)/CLAUDE_BASE"

src="$repo_root/target"
dst="$dest/target"
if [ ! -d "$src" ]; then
  echo "No target/ to seed from — the new worktree starts cold."
  echo "$dest" >&3
  exit 0
fi

# Seeding is an optimisation, never a reason to fail: a non-zero exit here would abort the whole
# worktree creation when this runs as a WorktreeCreate hook. A cold target/ still builds.
seed() {
  echo "==> Seeding $dst from $src"
  if cp -Rc "$src" "$dst" 2>/dev/null; then
    how="clone (APFS, no bytes copied)"
  else
    # BSD cp has no -l, and neither does macOS's. cpio is everywhere and links as it walks.
    mkdir -p "$dst"
    (cd "$src" && find . -type f -print | cpio -pdlm "$dst" 2>/dev/null)
    how="hardlinks"
  fi

  # Drop what must not be shared (see the header)
  find "$dst" -type d -name incremental -prune -exec rm -rf {} + 2>/dev/null || true
  find "$dst" -type f -name "*${crate}*" -delete 2>/dev/null || true

  echo "    $how, $(du -sh "$dst" | cut -f1) apparent, $(df -h "$dest" | awk 'NR==2 {print $4}') free on disk"
  echo
  echo "The worktree builds into its own target/, so builds there no longer wait on this one."
  echo "  cd $dest && cargo test"
}
seed || echo "Seeding failed — the worktree is fine, its target/ just starts cold."

echo "$dest" >&3
