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
# **Inside the repository** (`worktrees/`, git-ignored), not beside it: a checkout of this project
# belongs under this project, and keeping them here means one folder to find, move or delete.
dest="${2:-$repo_root/worktrees/$name}"
base="${3:-origin/main}"
crate="$(sed -n 's/^name *= *"\(.*\)"/\1/p' "$repo_root/Cargo.toml" | head -1)"

git -C "$repo_root" worktree add -b "$name" "$dest" "$base"

src="$repo_root/target"
dst="$dest/target"
if [ ! -d "$src" ]; then
  echo "No target/ to seed from — the new worktree starts cold."
  exit 0
fi

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
