#!/usr/bin/env bash
# Checks that the `cargo dist` output of this checkout can go out without taking anything back.
# deploy.sh and release.sh both run it before they ship anything.
#
#   scripts/preflight.sh            # exit 0 = fine to ship, 1 = stop
#   scripts/preflight.sh --force    # don't stop for dropped commits (still says which)
#
# Why: a build made from a branch that lacked a fix shipped earlier silently takes that fix back
# out — first through `update local` (deploy), then through a release cut from main while a fix
# was running from another branch. Nothing compared the build with what was running. Checks:
#   1. the built binaries are this commit (`--build` of each artifact names HEAD)
#   2. every commit on main is in HEAD
#   3. every commit in the running build is in HEAD (rebased or cherry-picked copies count)
#   4. the built binaries never start agents with `--strict-mcp-config` (it hides the user's MCP
#      servers and claude.ai connectors from every agent)
# 2 and 3 can be overridden with --force; 1 and 4 can't.
#
# AGENTGW_BIN names the running binary to compare with (default ~/.local/bin/agentgw).
set -euo pipefail

force=0
for arg in "$@"; do
  case "$arg" in
    --force) force=1 ;;
    *) echo "unknown argument: $arg" >&2; exit 2 ;;
  esac
done

cd "$(git rev-parse --show-toplevel)"
bin="${AGENTGW_BIN:-$HOME/.local/bin/agentgw}"
build_dir="${CARGO_TARGET_DIR:-target}"
head=$(git rev-parse --short HEAD)
fail=0 drop=0

say() { printf '%s\n' "$*"; }

# 1. artifacts are this commit
shopt -s nullglob
artifacts=("$build_dir"/*/release/agentgw "$build_dir"/*/release/agentgw.exe)
shopt -u nullglob
if [ "${#artifacts[@]}" -eq 0 ]; then
  say "✗ no cargo dist output ($build_dir/<triple>/release/agentgw) — run cargo dist first"; exit 1
fi
native=""
for a in "${artifacts[@]}"; do
  if build=$("$a" --build 2>/dev/null); then
    native="$a"
    commit=${build##*+}; commit=${commit%.dirty}
    if [ "$commit" != "$head" ]; then
      say "✗ $a is $build, but HEAD is $head — run cargo dist again"; fail=1
    else
      say "✓ $a is $build"
    fi
  fi
done
[ -n "$native" ] || say "· none of the artifacts runs here; their commit is not checked"
if ! git diff --quiet HEAD; then say "· the work tree has uncommitted changes (the build is .dirty)"; fi

# 2. main is in HEAD
for ref in main origin/main; do
  git rev-parse -q --verify "$ref" >/dev/null || continue
  missing=$(git log --no-merges --cherry-pick --right-only --format='  %h %s' "HEAD...$ref")
  if [ -n "$missing" ]; then
    say "✗ HEAD lacks these commits from $ref:"; say "$missing"; drop=1
  else
    say "✓ HEAD has everything on $ref"
  fi
done

# 3. the running build is in HEAD
running=$("$bin" --build 2>/dev/null || true)
rc=${running##*+}; rc=${rc%.dirty}
if [ -z "$running" ] || [ "$rc" = "$running" ]; then
  say "· could not read the running build from $bin --build"
elif ! git cat-file -e "$rc^{commit}" 2>/dev/null; then
  say "· the running build $running names a commit this repo doesn't have — fetch, or check by hand"
  drop=1
else
  missing=$(git log --no-merges --cherry-pick --right-only --format='  %h %s' "HEAD...$rc")
  if [ -n "$missing" ]; then
    say "✗ this would drop these commits from the running build ($running):"; say "$missing"; drop=1
  else
    say "✓ HEAD has everything in the running build ($running)"
  fi
fi

# 4. no --strict-mcp-config
for a in "${artifacts[@]}"; do
  if grep -aq -- '--strict-mcp-config' "$a"; then
    say "✗ $a starts agents with --strict-mcp-config"; fail=1
  fi
done
[ $fail -eq 1 ] || say "✓ no artifact contains --strict-mcp-config"

if [ $fail -eq 1 ]; then exit 1; fi
if [ $drop -eq 1 ]; then
  if [ $force -eq 0 ]; then
    say "stopped — merge the missing commits, or pass --force if dropping them is intended"; exit 1
  fi
  say "· --force: going ahead without the commits listed above"
fi
exit 0
