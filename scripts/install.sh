#!/usr/bin/env bash
# Install agentgw: fetch the binary → put it in ~/.local/bin → check tmux and claude
#                  → write the service definition → start it.
#
#   scripts/install.sh [--from <binary>]
#
# **Runs on any machine.** The same script runs on the gateway (with the repo checked out)
# and on a machine that `add-machine` set up (no repo, no Rust). Only the binary's source differs:
#   1. --from <path>   a binary handed over (what add-machine copied with scp)
#   2. curl from a GitHub release (**public repo: no auth, no gh**)
#   3. gh from a GitHub release (fallback for a private repo; needs auth)
#   4. the output of `cargo dist` (only with the repo checked out)
#
# A new machine installs with this one line (**not `curl … | bash`** — stdin would no
# longer be the terminal, and `agentgw install` could not ask for the tokens):
#   bash -c "$(curl -fsSL https://raw.githubusercontent.com/takashito/agentgw/main/scripts/install.sh)"
#
# Asking for tokens, writing .env and generating the plist/unit happen in `agentgw install`
# (which has unit tests).
#
# Environment:
#   AGENTGW_STATE_DIR      state directory (default: ~/.local/state/agentgw)
#   AGENTGW_SERVICE_LABEL  launchd label (default: com.agentgw.bridge)
#   AGENTGW_REPO           repository to fetch releases from (default: takashito/agentgw)
#   AGENTGW_TAG            release tag to fetch (default: the latest release)
#   AGENTGW_LANG           ja for Japanese messages (default: English)
#   PREFIX                 where the binary goes (default: ~/.local/bin)
set -euo pipefail

state_dir="${AGENTGW_STATE_DIR:-$HOME/.local/state/agentgw}"

# Same rule as the binary: AGENTGW_LANG from the environment, then from the state .env.
lang="${AGENTGW_LANG:-}"
if [ -z "$lang" ] && [ -f "$state_dir/.env" ]; then
  lang="$(sed -n 's/^AGENTGW_LANG=//p' "$state_dir/.env" | tail -n 1 | tr -d '"'"'"' ')"
fi
case "$(printf '%s' "$lang" | tr '[:upper:]' '[:lower:]')" in
  ja|ja_*|ja-*|japanese) lang=ja ;;
  *)                     lang=en ;;
esac
# say "English" "日本語" — print the one for the current language.
say() { if [ "$lang" = ja ]; then printf '%s\n' "$2"; else printf '%s\n' "$1"; fi; }

from=""
while [ $# -gt 0 ]; do
  case "$1" in
    --from)   from="$2"; shift 2 ;;
    --from=*) from="${1#--from=}"; shift ;;
    *)        say "usage: scripts/install.sh [--from <binary>]" \
                  "使い方: scripts/install.sh [--from <バイナリ>]" >&2; exit 2 ;;
  esac
done

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)"
prefix="${PREFIX:-$HOME/.local/bin}"
target="$prefix/agentgw"
# CARGO_TARGET_DIR may be set per developer, so don't hard-code where builds land
build_dir="${CARGO_TARGET_DIR:-$repo_root/target}"

# **Don't rely on rustc** — machines have no Rust. Derive the triple from uname
# (same table as `triple_for` in src/fleet.rs)
host_triple() {
  case "$(uname -sm)" in
    "Darwin arm64")                echo "aarch64-apple-darwin" ;;
    "Darwin x86_64")               echo "x86_64-apple-darwin" ;;
    "Linux x86_64")                echo "x86_64-unknown-linux-musl" ;;
    "Linux aarch64"|"Linux arm64") echo "aarch64-unknown-linux-musl" ;;
    *)                             echo "" ;;
  esac
}

# **Don't rely on `command -v` alone.** A non-interactive ssh shell returns early from
# ~/.bashrc ("not interactive → return"), so ~/.local/bin added there is not on PATH.
# Prints nothing if not found.
#
# **Skip binaries in temporary directories.** Wrappers and shims sometimes live in /tmp or
# /var/folders; baking those into the service PATH breaks on the next reboot.
which_bin() {
  found="$(command -v "$1" 2>/dev/null || true)"
  case "$found" in
    /tmp/*|/var/folders/*|/private/var/folders/*) found="" ;;
  esac
  [ -n "$found" ] && echo "$found" && return 0
  for p in "$HOME/.local/bin/$1" "$HOME/.claude/local/$1" \
           "/usr/local/bin/$1" "/opt/homebrew/bin/$1" "/snap/bin/$1"; do
    [ -x "$p" ] && echo "$p" && return 0
  done
  return 1
}

# **Don't take over another implementation's state directory.** agentgw never creates a
# socket there, so a bridge.sock means it belongs to a different bot.
if [ -e "$state_dir/bridge.sock" ]; then
  if [ "$lang" = ja ]; then
    cat >&2 <<EOS
中止しました。

  $state_dir は別のボットが使っているディレクトリです(bridge.sock があります)。

このまま入れると、同じ Slack アプリに2つ目の接続ができます。Slack はメッセージを
2つの接続に振り分けるので、どちらのボットも返事をしたりしなかったりします。

別のディレクトリを指定してやり直してください:
  AGENTGW_STATE_DIR=\$HOME/.local/state/agentgw-dev ./scripts/install.sh
EOS
  else
    cat >&2 <<EOS
Stopped.

  $state_dir is in use by another bot (it has a bridge.sock).

Installing here would open a second connection to the same Slack app. Slack splits
messages between the two, so both bots would answer only some of the time.

Choose another directory and run again:
  AGENTGW_STATE_DIR=\$HOME/.local/state/agentgw-dev ./scripts/install.sh
EOS
  fi
  exit 1
fi

# **This script doesn't build.** Building what gets shipped is `cargo dist`'s job.
triple="$(host_triple)"
binary=""
staging=""
# **End with `return 0`.** The last command of the EXIT trap sets the script's exit code —
# with an empty staging, `[ -n … ]` returns 1 and a successful run reports failure.
cleanup() { [ -n "$staging" ] && rm -rf "$staging"; return 0; }
trap cleanup EXIT

if [ -n "$from" ]; then
  # 1. A binary handed over (by add-machine). The caller has checked it's current
  [ -e "$from" ] || { say "Stopped. $from doesn't exist." "中止しました。$from がありません。" >&2; exit 1; }
  binary="$from"
  say "==> Installing the given binary: $binary" "==> 渡されたバイナリを入れます: $binary"
elif [ -n "$triple" ]; then
  repo="${AGENTGW_REPO:-takashito/agentgw}"
  staging="$(mktemp -d)"
  # 2. **curl from a release.** A public repo needs neither auth nor gh
  #    (`latest/download/<asset>` redirects to the latest release).
  #    Pin a version with AGENTGW_TAG=v0.19.0
  if command -v curl >/dev/null 2>&1; then
    if [ -n "${AGENTGW_TAG:-}" ]; then
      url="https://github.com/$repo/releases/download/$AGENTGW_TAG/agentgw-$triple"
    else
      url="https://github.com/$repo/releases/latest/download/agentgw-$triple"
    fi
    if curl -fsSL "$url" -o "$staging/agentgw-$triple" 2>/dev/null; then
      binary="$staging/agentgw-$triple"
      chmod 755 "$binary"
      say "==> Downloaded $url" "==> ダウンロードしました: $url"
    fi
  fi
  # 3. **gh from a release.** For a private repo (needs auth)
  if [ -z "$binary" ] && command -v gh >/dev/null 2>&1; then
    if gh release download ${AGENTGW_TAG:+"$AGENTGW_TAG"} \
         --repo "$repo" \
         --pattern "agentgw-$triple" --dir "$staging" --clobber 2>/dev/null; then
      binary="$staging/agentgw-$triple"
      chmod 755 "$binary"
      say "==> Downloaded $(basename "$binary") from the GitHub release" \
          "==> GitHub の release から $(basename "$binary") をダウンロードしました"
    fi
  fi
fi

if [ -z "$binary" ]; then
  # 4. The output of `cargo dist`. **Only with the repo checked out.** Stops if it's older than the source
  binary="$build_dir/$triple/release/agentgw"
  if [ ! -e "$binary" ]; then
    say "Stopped. There's no binary to install." "中止しました。入れるバイナリがありません。" >&2
    say "  cargo dist                  # build one here" \
        "  cargo dist                  # ここでビルドする" >&2
    say "  gh auth login               # or let gh download the release" \
        "  gh auth login               # または gh で release を落とせるようにする" >&2
    exit 1
  fi
  # **Close with `|| true`.** find can exit non-zero when it finds nothing, and inside an
  # assignment `set -e` would end the script before the message
  if [ -d "$repo_root/src" ]; then
    stale="$(cd "$repo_root" && find src Cargo.toml Cargo.lock -newer "$binary" -print -quit 2>/dev/null || true)"
    if [ -n "$stale" ]; then
      say "Stopped. The binary is older than the source." "中止しました。バイナリがソースより古いままです。" >&2
      say "  binary: $(date -r "$binary" '+%m/%d %H:%M') / ${stale}: $(date -r "$stale" '+%m/%d %H:%M')" \
          "  バイナリ: $(date -r "$binary" '+%m/%d %H:%M') / ${stale}: $(date -r "$stale" '+%m/%d %H:%M')" >&2
      say "  Run cargo dist to rebuild, then try again." "  cargo dist でビルドし直してから、もう一度実行してください。" >&2
      exit 1
    fi
  fi
fi

say "==> Installing to $target" "==> $target に置きます"
mkdir -p "$prefix"
# **The source can be the target itself** (add-machine copies to ~/.local/bin/agentgw and
# points --from at it). Moving the target aside first would delete it, so copy it away first
if [ -e "$target" ] && [ "$binary" -ef "$target" ]; then
  keep="$(mktemp)"
  cp "$binary" "$keep"
  binary="$keep"
  trap 'cleanup; rm -f "$keep"' EXIT
fi
# A running binary can't be overwritten (Text file busy). Move it aside first
if [ -e "$target" ]; then
  mv -f "$target" "$target.old"
fi
cp "$binary" "$target"
chmod 755 "$target"
rm -f "$target.old"
# The macOS firewall, Gatekeeper and TCC all identify an executable by its signature; an
# unsigned one is "an unknown executable" every time. No sudo needed.
#
# **Keep the identity fixed.** An ad-hoc signature (`--sign -`) is identified by the content
# hash (cdhash), so every reinstall looks like a new app and every permission resets (the
# Desktop / other apps' data prompts come back on each start). Signing with one fixed
# self-signed certificate in the login keychain makes the requirement
# `identifier + certificate leaf`, which survives reinstalls. `--identifier` is needed too:
# the default identifier comes from the Mach-O UUID and changes with every build.
# Other systems have no codesign; skip quietly.
sign_id="${SIGN_IDENTITY:-agentgw dev}"

# Create the certificate (**once per machine**; later runs find it).
# macOS asks for the keychain password (to import the key and to trust it for code signing),
# so this **only runs at an interactive terminal**. If it fails we fall back to ad-hoc.
make_signing_identity() {
  keychain="$HOME/Library/Keychains/login.keychain-db"
  work="$(mktemp -d)"
  if security find-certificate -c "$sign_id" >/dev/null 2>&1; then
    # The certificate exists (the last run stopped before trusting it). Reuse it
    security find-certificate -c "$sign_id" -p > "$work/cert.pem"
  else
    openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
      -keyout "$work/key.pem" -out "$work/cert.pem" -subj "/CN=$sign_id" \
      -addext "basicConstraints=critical,CA:false" \
      -addext "keyUsage=critical,digitalSignature" \
      -addext "extendedKeyUsage=critical,codeSigning" 2>/dev/null || { rm -rf "$work"; return 1; }
    # **Don't leave the password empty.** security(1) rejects an empty-password p12 with
    # "MAC verification failed". -legacy is needed too (security can't read AES-256 p12s)
    openssl pkcs12 -export -legacy -passout pass:tmp \
      -inkey "$work/key.pem" -in "$work/cert.pem" -out "$work/id.p12" 2>/dev/null \
      || { rm -rf "$work"; return 1; }
    # -A: don't ask each time codesign uses the key
    security import "$work/id.p12" -k "$keychain" -P tmp -A -T /usr/bin/codesign >/dev/null \
      || { rm -rf "$work"; return 1; }
  fi
  security add-trusted-cert -r trustRoot -p codeSign -k "$keychain" "$work/cert.pem" \
    || { rm -rf "$work"; return 1; }
  rm -rf "$work"
}

if command -v codesign >/dev/null 2>&1; then
  if ! security find-identity -v -p codesigning 2>/dev/null | grep -qF "$sign_id" && [ -t 0 ]; then
    say "==> Creating a signing certificate (once per machine, so macOS keeps its permissions across reinstalls)" \
        "==> 署名用の証明書を作ります(このマシンで一度だけ。入れ直しても macOS の許可が消えないように)"
    say "    macOS will ask for your keychain password" "    macOS がキーチェーンのパスワードを訊きます"
    make_signing_identity || say "  Couldn't create it. Signing ad-hoc this time (continuing)" \
                                 "  作れませんでした。今回は ad-hoc で署名します(続けます)"
  fi
  if security find-identity -v -p codesigning 2>/dev/null | grep -qF "$sign_id"; then
    codesign --force --sign "$sign_id" --identifier agentgw "$target" 2>/dev/null \
      || say "  Couldn't sign the binary (continuing)" "  署名できませんでした(続けます)"
  else
    codesign --force --sign - "$target" 2>/dev/null \
      || say "  Couldn't sign the binary (continuing)" "  署名できませんでした(続けます)"
    say "  Signed ad-hoc: macOS will ask for permissions again after each reinstall" \
        "  ad-hoc で署名しました。入れ直すたびに macOS が許可を訊き直します"
  fi
fi

# ── Prerequisites ─────────────────────────────────────────────────────────
# Agents are TUIs inside tmux; without tmux none of them can run
say "==> Checking tmux" "==> tmux を確かめます"
if tmux_bin="$(which_bin tmux)"; then
  say "  found ($tmux_bin)" "  あります ($tmux_bin)"
else
  say "  not found — installing it" "  ありません。入れます"
  sudo_if() { if [ "$(id -u)" = 0 ]; then "$@"; else sudo "$@"; fi; }
  if   command -v apt-get >/dev/null; then sudo_if apt-get update && sudo_if apt-get install -y tmux
  elif command -v dnf     >/dev/null; then sudo_if dnf install -y tmux
  elif command -v pacman  >/dev/null; then sudo_if pacman -S --noconfirm tmux
  elif command -v brew    >/dev/null; then brew install tmux
  else
    say "No package manager found. Install tmux yourself, then run this again." \
        "パッケージマネージャが見つかりません。tmux を手で入れてから、もう一度実行してください。" >&2
    exit 1
  fi
fi

# claude is **not installed** here — it needs a sign-in and a version choice, which are
# the owner's decisions. **Put the directory it was found in at the front of PATH:**
# `install` bakes the current PATH into the service definition (Environment=PATH=… in
# src/service.rs), so otherwise agentgw would run but fail to start any agent.
say "==> Checking claude" "==> claude を確かめます"
if claude_bin="$(which_bin claude)"; then
  say "  found ($claude_bin)" "  あります ($claude_bin)"
  PATH="$(dirname "$claude_bin"):$PATH"
  export PATH
else
  say "  not found — agentgw can't start agents without it." \
      "  ありません。このままでは agentgw はエージェントを起動できません。" >&2
  say "  Install Claude Code and sign in once (it's tied to your account, so this script can't do it for you)." \
      "  Claude Code を入れて、一度サインインしてください(アカウントに結びつくので、代わりにはできません)。" >&2
fi

say "==> Writing the service definition" "==> サービスの定義を書きます"
# install prints from column 0; indent by two. We restart right after, so skip its restart hint
"$target" install --no-restart-hint | sed 's/^/  /'

say "==> Starting agentgw" "==> agentgw を起動します"
# **Check whether it's already installed first.** Starting unconditionally makes launchd
# print an unreadable `Bootstrap failed: 5: Input/output error` for an already loaded service
if "$target" status >/dev/null 2>&1; then
  "$target" restart
else
  "$target" start
fi

# Right after a restart, machines haven't reconnected yet and status shows none
# ponytail: fixed 3 seconds
sleep 3

echo
say "==> Status of this machine" "==> このマシンの状態"
# Indent status by two to sit under the heading (keep blank lines as they are)
st="$("$target" status 2>/dev/null || true)"
printf '%s\n' "$st" | sed 's/^./  &/'

# ── Next step ─────────────────────────────────────────────────────────────
# status speaks the configured language: "Role: gateway …" / "役割: ゲートウェイ …"
role="$(printf '%s\n' "$st" | sed -n -e 's/^Role: //p' -e 's/^役割: //p' | head -n 1)"
case "${role}" in
  gateway*|ゲートウェイ*)
    echo
    say "To add another machine, run this here on the gateway:" \
        "ほかのマシンを加えるには、このゲートウェイで次を実行してください:"
    say "  agentgw add-machine user@host   # installs, connects and starts it in one go" \
        "  agentgw add-machine user@host   # インストールから接続・起動まで1回で済みます"
    ;;
  machine*|マシン*)
    echo
    say "This machine works for a gateway. To hand it a Slack channel, type \`route <name>\` in that channel." \
        "このマシンはゲートウェイの下で動きます。Slack のチャンネルを任せるには、そのチャンネルで \`route <名前>\` と打ってください。"
    ;;
esac
