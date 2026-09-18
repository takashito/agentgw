#!/usr/bin/env bash
# agentgw を入れる: バイナリを用意 → ~/.local/bin へ配置 → tmux と claude を確かめる
#                  → サービス定義 → 起動。
#
#   scripts/install.sh [--from <バイナリ>]
#
# **どのマシンでも動く。** 親の手元(repo あり)でも、`add-child` が送り込んだ子の上
# (repo も rust も無い)でも同じものが走る。バイナリの出どころだけが違う:
#   1. --from <path>   外から渡された物(add-child が scp した物)
#   2. curl で release から(**公開リポなら認証も gh も要らない**)
#   3. gh で release から(非公開のときの逃げ道。認証が要る)
#   4. cargo dist の成果物(repo があるときだけ)
#
# 新しいマシンはこの1行で入れられる(**`curl … | bash` にしない** — 標準入力が端末で
# なくなり、install がトークンを訊けない):
#   bash -c "$(curl -fsSL https://raw.githubusercontent.com/takashito/agentgw/main/scripts/install.sh)"
#
# トークンの入力・.env の書き込み・plist/unit の生成は `agentgw install` の側にある
# (そちらは単体テストがある)。
#
# 環境変数で振る舞いを変えられる:
#   AGENTGW_STATE_DIR      状態ディレクトリ(既定: ~/.local/state/agentgw)
#   AGENTGW_SERVICE_LABEL  launchd のラベル(既定: com.agentgw.bridge)
#   AGENTGW_REPO           release を引くリポジトリ(既定: takashito/agentgw)
#   AGENTGW_TAG            落とす tag(既定: 最新の release)
#   PREFIX                 バイナリの置き場(既定: ~/.local/bin)
set -euo pipefail

from=""
while [ $# -gt 0 ]; do
  case "$1" in
    --from)   from="$2"; shift 2 ;;
    --from=*) from="${1#--from=}"; shift ;;
    *)        echo "usage: scripts/install.sh [--from <バイナリ>]" >&2; exit 2 ;;
  esac
done

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)"
prefix="${PREFIX:-$HOME/.local/bin}"
target="$prefix/agentgw"
# ワーカーごとに CARGO_TARGET_DIR を分けている場合があるので、成果物は決め打ちで探さない
build_dir="${CARGO_TARGET_DIR:-$repo_root/target}"

# **rustc に頼らない** — 子には rust が入っていない。uname から triple を出す
# (`src/fleet.rs` の `triple_for` と同じ表)
host_triple() {
  case "$(uname -sm)" in
    "Darwin arm64")                echo "aarch64-apple-darwin" ;;
    "Darwin x86_64")               echo "x86_64-apple-darwin" ;;
    "Linux x86_64")                echo "x86_64-unknown-linux-musl" ;;
    "Linux aarch64"|"Linux arm64") echo "aarch64-unknown-linux-musl" ;;
    *)                             echo "" ;;
  esac
}

# **`command -v` だけに頼らない。** ssh の非対話シェルは ~/.bashrc の先頭の
# 「対話でなければ return」で弾かれるので、そこで PATH に足している ~/.local/bin が
# 見えない(実測)。見つからなければ空を返す
#
# **一時ディレクトリの実体は採らない。** ラッパーやシムが /tmp や /var/folders に
# 置かれていることがあり、そこを PATH に焼き込むと、次の再起動で消えている
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

# **別実装のボットの置き場を掴まない。** この実装は UDS を作らないので、
# bridge.sock があれば相手は別のボット(Bun 版)の置き場だと分かる
state_dir="${AGENTGW_STATE_DIR:-$HOME/.local/state/agentgw}"
if [ -e "$state_dir/bridge.sock" ]; then
  cat >&2 <<EOS
中止しました。

  $state_dir は別実装のボットの置き場です(bridge.sock があります)。

ここを指したまま入れると、同じ Slack app に2本目が繋がります。Slack はイベントを
複製せず半分ずつ振り分けるので、どちらのボットも動いたり動かなかったりします。

置き場を指定してやり直してください:
  AGENTGW_STATE_DIR=\$HOME/.local/state/agentgw-dev ./scripts/install.sh
EOS
  exit 1
fi

# **このスクリプトはビルドしない。** 配る物を作るのは `cargo dist` の仕事。
triple="$(host_triple)"
binary=""
staging=""
# **`return 0` で閉じる。** EXIT の trap の最後のコマンドの終了コードが
# スクリプトの終了コードになる — 空の staging で `[ -n … ]` が 1 を返すと、
# 成功したのに「失敗しました」になる(2026-09-18 に踏んだ)
cleanup() { [ -n "$staging" ] && rm -rf "$staging"; return 0; }
trap cleanup EXIT

if [ -n "$from" ]; then
  # 1. 外から渡された物(add-child が scp した物)。鮮度は呼び手が見ている
  [ -e "$from" ] || { echo "中止しました。$from がありません。" >&2; exit 1; }
  binary="$from"
  echo "==> 渡されたバイナリを入れます: $binary"
elif [ -n "$triple" ]; then
  repo="${AGENTGW_REPO:-takashito/agentgw}"
  staging="$(mktemp -d)"
  # 2. **curl で release から。** リポジトリが公開なら認証も gh も要らない
  #    (`latest/download/<資産名>` が最新の release にリダイレクトされる)。
  #    版を指定するなら AGENTGW_TAG=v0.19.0
  if command -v curl >/dev/null 2>&1; then
    if [ -n "${AGENTGW_TAG:-}" ]; then
      url="https://github.com/$repo/releases/download/$AGENTGW_TAG/agentgw-$triple"
    else
      url="https://github.com/$repo/releases/latest/download/agentgw-$triple"
    fi
    if curl -fsSL "$url" -o "$staging/agentgw-$triple" 2>/dev/null; then
      binary="$staging/agentgw-$triple"
      chmod 755 "$binary"
      echo "==> release から取りました ($url)"
    fi
  fi
  # 3. **gh で release から。** 非公開のときはこちら(認証が要る)
  if [ -z "$binary" ] && command -v gh >/dev/null 2>&1; then
    if gh release download ${AGENTGW_TAG:+"$AGENTGW_TAG"} \
         --repo "$repo" \
         --pattern "agentgw-$triple" --dir "$staging" --clobber 2>/dev/null; then
      binary="$staging/agentgw-$triple"
      chmod 755 "$binary"
      echo "==> GitHub release から取りました ($(basename "$binary"))"
    fi
  fi
fi

if [ -z "$binary" ]; then
  # 4. cargo dist の成果物。**repo があるときだけ**。ソースより古ければ止まる
  binary="$build_dir/$triple/release/agentgw"
  if [ ! -e "$binary" ]; then
    echo "中止しました。入れるバイナリがありません。" >&2
    echo "  cargo dist                  # 手元で焼く" >&2
    echo "  gh auth login               # または release から落とせるようにする" >&2
    echo "  (リポジトリが公開なら curl だけで落ちます — ここに来たのは網の外か、資産が無いとき)" >&2
    exit 1
  fi
  # **`|| true` で閉じる。** find が何も見つけないと非ゼロで返ることがあり、
  # 代入の中で落ちると `set -e` が echo の手前でスクリプトごと終わらせる(実際に踏んだ)
  if [ -d "$repo_root/src" ]; then
    stale="$(cd "$repo_root" && find src Cargo.toml Cargo.lock -newer "$binary" -print -quit 2>/dev/null || true)"
    if [ -n "$stale" ]; then
      echo "中止しました。バイナリがソースより古いままです。" >&2
      echo "  バイナリ: $(date -r "$binary" '+%m/%d %H:%M') / ${stale}: $(date -r "$stale" '+%m/%d %H:%M')" >&2
      echo "  cargo dist   # で作り直してから、もう一度" >&2
      exit 1
    fi
  fi
fi

echo "==> このマシンに置く: $target"
mkdir -p "$prefix"
# **置き先そのものを渡されることがある**(add-child は ~/.local/bin/agentgw に scp して
# から、そこを --from で指す)。退けてから cp すると元が消えるので、先に控えを取る
if [ -e "$target" ] && [ "$binary" -ef "$target" ]; then
  keep="$(mktemp)"
  cp "$binary" "$keep"
  binary="$keep"
  trap 'cleanup; rm -f "$keep"' EXIT
fi
# 走っているバイナリは上書きできない(Text file busy)。先に退けてから置く
if [ -e "$target" ]; then
  mv -f "$target" "$target.old"
fi
cp "$binary" "$target"
chmod 755 "$target"
rm -f "$target.old"
# macOS のファイアウォールもゲートキーパーも TCC も実行ファイルを署名で見るので、署名が
# 無いものは毎回「素性の分からない実行ファイル」として扱われる。sudo は要らない。
#
# **身元を固定する。** ad-hoc(`--sign -`)の身元は中身のハッシュ(cdhash)なので、ビルド
# し直すたびに別物になり、出した許可が毎回リセットされる(デスクトップ/他アプリのデータの
# ダイアログが起動のたびに並ぶ)。`scripts/signing-cert.sh` で作った固定の証明書があれば
# それで署名する — 要求が `identifier + 証明書のリーフ` で一致するので許可が生き残る。
# `--identifier` も明示する。既定の識別子は Mach-O の UUID 由来でビルドごとに変わるため、
# 証明書だけ固定しても一致しない。
# macOS 以外には codesign が無いので黙って飛ばす
sign_id="${SIGN_IDENTITY:-agentgw dev}"
if command -v codesign >/dev/null 2>&1; then
  if security find-identity -v -p codesigning 2>/dev/null | grep -qF "$sign_id"; then
    codesign --force --sign "$sign_id" --identifier agentgw "$target" 2>/dev/null \
      || echo "  署名できませんでした(続けます)"
  else
    codesign --force --sign - "$target" 2>/dev/null \
      || echo "  署名できませんでした(続けます)"
    echo "  ad-hoc 署名です。許可のダイアログが毎回出るなら ./scripts/signing-cert.sh"
  fi
fi

# ── 前提を確かめる(deploy.sh から引っ越し)─────────────────────────────────
# ワーカーは tmux の中の TUI なので、無ければ1本も動かない
echo "==> tmux を確かめる"
if tmux_bin="$(which_bin tmux)"; then
  echo "  あります ($tmux_bin)"
else
  echo "  ありません — 入れます"
  sudo_if() { if [ "$(id -u)" = 0 ]; then "$@"; else sudo "$@"; fi; }
  if   command -v apt-get >/dev/null; then sudo_if apt-get update && sudo_if apt-get install -y tmux
  elif command -v dnf     >/dev/null; then sudo_if dnf install -y tmux
  elif command -v pacman  >/dev/null; then sudo_if pacman -S --noconfirm tmux
  elif command -v brew    >/dev/null; then brew install tmux
  else
    echo "パッケージマネージャが見つかりません。tmux を手で入れてください" >&2
    exit 1
  fi
fi

# claude は**入れない** — 導入にサインインと版の選択が伴い、それは持ち主が決めること。
# **見つけた場所を PATH に前置きする。** `install` はそのときの PATH をサービス定義に
# 焼き込むので(`src/service.rs` の Environment=PATH=…)、素で通すと
# 「claude はあるのにワーカーを起動できない Bridge」が黙って出来上がる
echo "==> claude を確かめる"
if claude_bin="$(which_bin claude)"; then
  echo "  あります ($claude_bin)"
  PATH="$(dirname "$claude_bin"):$PATH"
  export PATH
else
  cat >&2 <<'EOS'
  ありません — Bridge はワーカーを起動できません。
  一度だけ Claude Code を入れてサインインしてください(アカウントに紐づくので代行できません)。
EOS
fi

echo "==> サービスの定義を書く"
# install は行頭から書くので2字下げに揃える。直後に自分で起こし直すので反映の案内は要らない
"$target" install --no-restart-hint | sed 's/^/  /'

echo "==> 起動する"
# **先に「もう入っているか」を見る。** 無条件に start すると、bootstrap 済みの相手には
# `Bootstrap failed: 5: Input/output error` という読めない失敗が出て、それが一番目立つ
if "$target" status >/dev/null 2>&1; then
  "$target" restart
else
  "$target" start
fi

# 起こし直した直後は子がまだ繋ぎ直しておらず、status に「子 — なし」と出る(deploy.sh と同じ)
# ponytail: 固定 3 秒
sleep 3

echo
echo "==> このマシンの状態"
# status は畳んだ形で出るので、見出しの下に置くための2字下げだけ足す(空行はそのまま)
st="$("$target" status 2>/dev/null || true)"
printf '%s\n' "$st" | sed 's/^./  &/'

# ── 締め ────────────────────────────────────────────────────────────────
role="$(printf '%s\n' "$st" | sed -n 's/^役割: //p')"
case "${role}" in
  親*|単独*)
    cat <<'EOS'

子のマシンを足すには(親のマシンで):
  agentgw add-child user@host     # 届けて・繋いで・起こすまで1本でやります
EOS
    ;;
  *)
    echo
    echo "このマシンは子です。Slack で \`route <名前>\` と言えば、担当になります。"
    ;;
esac
