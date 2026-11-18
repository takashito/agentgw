#!/usr/bin/env bash
# Deploy this checkout's `cargo dist` output to every machine (`agentgw update local`), after
# checking that it doesn't drop anything already running or already on main.
#
#   scripts/deploy.sh            # check, then run `agentgw update local`
#   scripts/deploy.sh --check    # check only
#   scripts/deploy.sh --force    # deploy even if commits would be dropped (says which)
#
# The checks are in preflight.sh (shared with release.sh): the build is this commit, it has
# everything on main and in the running build, and it doesn't start agents with --strict-mcp-config.
set -euo pipefail

check_only=0 force=0
for arg in "$@"; do
  case "$arg" in
    --check) check_only=1 ;;
    --force) force=1 ;;
    *) echo "unknown argument: $arg" >&2; exit 2 ;;
  esac
done

cd "$(git rev-parse --show-toplevel)"
bin="${AGENTGW_BIN:-$HOME/.local/bin/agentgw}"

pre=""; [ $force -eq 0 ] || pre=--force
# shellcheck disable=SC2086  # $pre is one word or nothing
if ! scripts/preflight.sh $pre; then
  echo "not deploying"; exit 1
fi
[ $check_only -eq 0 ] || exit 0
exec "$bin" update local
