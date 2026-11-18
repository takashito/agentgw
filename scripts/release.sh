#!/usr/bin/env bash
# Upload the output of `cargo dist` to a GitHub release. **Doesn't build** (that's cargo dist's job).
#
#   scripts/release.sh
#
#   scripts/release.sh --replace      # re-upload over a tag someone else's commit already has
#
#   scripts/release.sh --check        # only check the release notes and print them
#
# The release notes are the `## [<version>]` section of CHANGELOG.md (Keep a Changelog). This
# stops if that section is missing, has no `### Added` / `Changed` / ... heading with `- ` lines
# under it, or has Japanese in it — every release page has to say what changed, in English.
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
check_only=0
for arg in "$@"; do
  case "$arg" in
    --replace) replace=1 ;;
    --check) check_only=1 ;;
    *) echo "Unknown argument: $arg (only --replace, --check)" >&2; exit 2 ;;
  esac
done

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# CARGO_TARGET_DIR may be set per developer, so don't hard-code where builds land
build_dir="${CARGO_TARGET_DIR:-$repo_root/target}"
ver="$(sed -n 's/^version *= *"\(.*\)"/\1/p' "$repo_root/Cargo.toml" | head -1)"
tag="v$ver"

# The release notes come first: no notes, no release (before anything is staged or uploaded)
changelog="${AGENTGW_CHANGELOG:-$repo_root/CHANGELOG.md}"
notes_file="$(mktemp)"
staging=""
trap 'rm -rf "$notes_file" ${staging:+"$staging"}' EXIT
notes_stop() {
  echo "Stopped. $1" >&2
  echo "Write the release notes for $ver in CHANGELOG.md first — the rules are in README.md (Release notes)." >&2
  exit 1
}
[ -f "$changelog" ] || notes_stop "There is no CHANGELOG.md."
# The section's body: from `## [x.y.z]` up to the next `## `, without the blank lines around it
# and without the `[x.y.z]: <url>` link lines at the end of the file
awk -v ver="$ver" '
  /^## / { if (inside) exit; if (index($0, "## [" ver "]") == 1) { inside = 1; next } }
  !inside || /^\[[^]]*\]: / { next }
  /^[[:space:]]*$/ { if (started) blanks++; next }
  { for (; blanks > 0; blanks--) print ""; print; started = 1 }
' "$changelog" > "$notes_file"
[ -s "$notes_file" ] || notes_stop "CHANGELOG.md has no section for $ver (## [$ver] - YYYY-MM-DD), or it is empty."
bad_heading="$(grep '^### ' "$notes_file" | grep -Ev '^### (Added|Changed|Deprecated|Removed|Fixed|Security)$' || true)"
[ -z "$bad_heading" ] || notes_stop "Unknown heading in the notes for $ver: $bad_heading (use Added, Changed, Deprecated, Removed, Fixed or Security)."
# Every heading needs at least one `- ` line, and every `- ` line needs a heading above it
awk '
  /^### / { if (heading && !items) { bad = 1 }; heading = 1; items = 0; next }
  /^- / { if (!heading) bad = 1; items++; seen++ }
  END { if (heading && !items) bad = 1; exit (bad || !seen) }
' "$notes_file" || notes_stop "The notes for $ver need ### Added / Changed / ... headings, each with at least one '- ' line under it."
if perl -CSD -ne 'exit 1 if /[\p{Hiragana}\p{Katakana}\p{Han}]/' "$notes_file"; then :; else
  notes_stop "The notes for $ver have Japanese in them. Release notes are written in English."
fi
echo "==> Release notes for $tag"
sed 's/^/  /' "$notes_file"
if [ "$check_only" = 1 ]; then
  exit 0
fi

# Stage the builds under names with their <triple>.
# **Skip `target/release/`** — it has no triple, so there's no telling which platform it is for
staging="$(mktemp -d)"
found=0
# A Windows build is agentgw.exe; its asset keeps the same `agentgw-<triple>` name (the triple says Windows)
for out in "$build_dir"/*/release/agentgw "$build_dir"/*/release/agentgw.exe; do
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

# Which build this release is. `update` compares it with what a machine runs, so a machine on a
# local build of the same version still takes the release. Read out of the files, since not every
# one of them runs here — and they must all be one build
labels="$(for f in "$staging"/agentgw-*; do
  case "$f" in *.sha256) ;; *) LC_ALL=C grep -a -o 'agentgw-build:[0-9.]*+[0-9a-z.]*' "$f" | head -1 ;; esac
done | sort -u)"
if [ "$(printf '%s\n' "$labels" | grep -c .)" != 1 ]; then
  echo "Stopped. The builds are not all one build:" >&2
  printf '  %s\n' $labels >&2
  exit 1
fi
printf '%s\n' "${labels#agentgw-build:}" > "$staging/agentgw.build"
echo "  agentgw.build (${labels#agentgw-build:})"

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
  gh release edit "$tag" --notes-file "$notes_file"
else
  gh release create "$tag" "$staging"/* --title "$tag" --notes-file "$notes_file"
fi
