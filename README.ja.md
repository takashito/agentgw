<div align="center">

# agentgw

### Claude Code を自分のマシンで動かし、Slack から操る。

**Slack のスレッドが、そのまま Claude Code のセッションになる。チャンネルごとに、動かすマシンを選べる。**

[![Release](https://img.shields.io/github/v/release/takashito/agentgw?style=flat-square&labelColor=black)](https://github.com/takashito/agentgw/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-green?style=flat-square&labelColor=black)](LICENSE)
[![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Linux-lightgrey?style=flat-square&labelColor=black)](#必要なもの)
[![Built with Rust](https://img.shields.io/badge/built%20with-Rust-dea584?style=flat-square&labelColor=black&logo=rust)](https://www.rust-lang.org/)

[English](README.md) · **日本語**

</div>

---

## なぜ agentgw か

Claude Code は実務に強い。ただ、1台のマシンの端末の中でしか動きません。

- 🖥️ **机から離れられない。** キーボードを離れると、何をしているかも見えず、舵も切れない。
- 🧵 **一度に1つの会話。** 仕事を並べると、端末も並べることになる。
- 🗄️ **コードがあちこちにある。** 触りたいリポジトリは、自宅のサーバーだったり、ビルド用のマシンだったり、ノートだったりする。

**agentgw は Slack を入口にします。** スレッドに書けば、そのスレッド専属の Claude Code のワーカーが、担当のマシンで、本物の作業ディレクトリを使って仕事をし、スレッドに返事を書きます。途中経過はその場で見え、許可が要るときは Slack のボタンで答えられます。

動かすのは**公式の Claude Code CLI** で、場所は自分のマシン、使うのは自分のサブスクリプションです。第三者のサービスは経由しません。

## できること

### 🧵 スレッド1つにセッション1つ
Slack のスレッドごとにワーカー(`tmux` の中で動く本物の Claude Code のセッション)が付きます。続きはスレッドに書くだけ。agentgw 自身を再起動・更新しても、セッションは続きます。

### 🖧 1台ではなく、複数台で
複数のマシンをつなげます。Slack につながるのは**親**1台だけで、**子**は親に外向きに接続するので、子の側にポート開放は要りません。チャンネルごとに `route <マシン>` で担当を決められます。

### 🚀 マシンを足すのはコマンド1つ
`agentgw add-child user@host` が、ssh でバイナリを届け、サービスとして入れ、親につなぎ、親から見えるまで待ちます。つなぎ方も自分で決めます — 直結(例: Tailscale 経由)か、親が張り続ける ssh トンネルか。

### 👀 仕事の様子がその場で見える
スレッドに進捗のメッセージが1つ出て、ツールを使うたびに更新されます(読んだファイル、打ったコマンド、加えた変更)。ターンが終わると畳まれます。

### 🔐 ツールの許可を Slack で
`manual` モードでは、ワーカーが使おうとするツールごとに **Allow** / **Deny** のボタンが出ます。

### 🎛️ チャットから全部操れる
モデル・effort・権限モードの切り替え、コンテキストの圧縮、サブスクリプションの使用状況の確認、🛑 のリアクションでの停止、手元の端末でセッションを続けるためのコマンドの取り出し。

### ⚡ ワーカーを温めておける
よく使うチャンネルはワーカーを先に起こしておけます(`warm on`)。最初の返事で起動を待たずに済みます。

### 📦 バイナリ1つ
マシンごとに Rust のバイナリが1つだけ。`launchd` のエージェントか `systemd` のユーザーユニットとして入ります。ランタイムのインストールも、面倒を見る常駐物もありません。

## 仕組み

```
                 Socket Mode
   Slack  ◀──────────────────────▶  agentgw(親)  ──▶  tmux ─ Claude Code …
                                      ▲         ▲
                 外向きの WebSocket   │         │  外向きの WebSocket
                 (直結 or トンネル)   │         │
                                agentgw(子)  agentgw(子)
                                      │         │
                        tmux ─ Claude Code …   tmux ─ Claude Code …
```

1. **Slack に書く**(チャンネルかスレッドに)。
2. **親**が Slack の Socket Mode で受け取り、そのチャンネルの担当のマシンに渡す。担当が決まっていなければ親が自分で受ける。
3. **担当のマシン**が、そのスレッドのワーカーに渡す(居なければ起こす)。作業ディレクトリはチャンネルごとの設定。
4. **ワーカー**が agentgw の MCP ツールでスレッドに返事を書く。途中経過と許可の確認もスレッドに出る。

Slack のイベントを受け取るのは親だけです。子は bot トークンを接続のたびに親から受け取り、メモリにだけ置きます。app トークンは親から出ません。

## 必要なもの

- **macOS か Linux** — 配っているバイナリは Apple Silicon の Mac 用と x86_64 の Linux 用
- **[Claude Code](https://docs.claude.com/ja/docs/claude-code)** — すべてのマシンに入れて、サインインしておく
- **`tmux`** — 無ければインストーラが入れる
- **Slack アプリ**(Socket Mode)— [Slack アプリの作り方](#slack-アプリの作り方)を参照

## はじめかた

**1. 親のマシンに入れる**

```bash
bash -c "$(curl -fsSL https://raw.githubusercontent.com/takashito/agentgw/main/scripts/install.sh)"
```

このマシン用のバイナリを取ってきてサービスとして入れ、Slack のトークン(`xoxb-…` と `xapp-…`)を訊きます。

> [!NOTE]
> `curl … | bash` ではなく `bash -c "$(curl …)"` の形で実行してください。パイプで渡すと端末が無くなり、インストーラがトークンを訊けません。

**2. Slack でサインインする**

ボットに **`login`** と DM します。最初に DM した人が **Owner**(ボットが命令を聞く唯一の人)になります。

**3. 話しかける**

チャンネルでボットにメンションするか、DM します。スレッドごとにセッションが分かれます。

**4. マシンを足す(任意)**

親のマシンで:

```bash
agentgw add-child user@host
```

そのマシンに任せたいチャンネルで `@agentgw route <マシン>`。

<details>
<summary><b>ソースから入れる</b></summary>

```bash
cargo build --release
./scripts/install.sh --from target/release/agentgw
```

</details>

## Slack アプリの作り方

**[➜ 設定済みの Slack アプリを作る](https://api.slack.com/apps?new_app=1&manifest_json=%7B%22%5Fmetadata%22%3A%7B%22major%5Fversion%22%3A1%2C%22minor%5Fversion%22%3A1%7D%2C%22display%5Finformation%22%3A%7B%22background%5Fcolor%22%3A%22%23262626%22%2C%22description%22%3A%22Run%20Claude%20Code%20on%20your%20own%20machines%20and%20drive%20it%20from%20Slack%22%2C%22name%22%3A%22agentgw%22%7D%2C%22features%22%3A%7B%22app%5Fhome%22%3A%7B%22home%5Ftab%5Fenabled%22%3Afalse%2C%22messages%5Ftab%5Fenabled%22%3Atrue%2C%22messages%5Ftab%5Fread%5Fonly%5Fenabled%22%3Afalse%7D%2C%22bot%5Fuser%22%3A%7B%22always%5Fonline%22%3Atrue%2C%22display%5Fname%22%3A%22agentgw%22%7D%7D%2C%22oauth%5Fconfig%22%3A%7B%22scopes%22%3A%7B%22bot%22%3A%5B%22app%5Fmentions%3Aread%22%2C%22assistant%3Awrite%22%2C%22channels%3Ahistory%22%2C%22channels%3Aread%22%2C%22chat%3Awrite%22%2C%22files%3Aread%22%2C%22files%3Awrite%22%2C%22groups%3Ahistory%22%2C%22groups%3Aread%22%2C%22im%3Ahistory%22%2C%22im%3Aread%22%2C%22im%3Awrite%22%2C%22mpim%3Ahistory%22%2C%22mpim%3Aread%22%2C%22reactions%3Aread%22%2C%22reactions%3Awrite%22%2C%22users%3Aread%22%5D%7D%7D%2C%22settings%22%3A%7B%22event%5Fsubscriptions%22%3A%7B%22bot%5Fevents%22%3A%5B%22app%5Fmention%22%2C%22member%5Fjoined%5Fchannel%22%2C%22message%2Echannels%22%2C%22message%2Egroups%22%2C%22message%2Eim%22%2C%22message%2Empim%22%2C%22reaction%5Fadded%22%2C%22reaction%5Fremoved%22%5D%7D%2C%22interactivity%22%3A%7B%22is%5Fenabled%22%3Atrue%7D%2C%22socket%5Fmode%5Fenabled%22%3Atrue%2C%22token%5Frotation%5Fenabled%22%3Afalse%7D%7D)**

このリンクを開くと、Slack の「マニフェストからアプリを作成」画面が、[`slack-app-manifest.json`](slack-app-manifest.json) の内容(権限、イベント、Socket Mode、Interactivity、DM のタブ)を入れた状態で開きます。ワークスペースを選んで **Create** を押してください。インストーラもトークンを訊くときに同じリンクを出し、ブラウザで開きます。

作ったら、トークンを2つ用意します。

1. **Install to Workspace** を押して、**Bot User OAuth Token**(`xoxb-…`)をコピーする
2. **Basic Information → App-Level Tokens** で、`connections:write` の権限を付けたトークン(`xapp-…`)を作る

<details>
<summary><b>手で設定する場合</b></summary>

[api.slack.com/apps](https://api.slack.com/apps) で **From a manifest** を選び、[`slack-app-manifest.json`](slack-app-manifest.json) の中身を貼ってください。抜けていても何も言われずに動かなくなるのは、次の3つです。

- **Socket Mode** を On — agentgw は外から受ける口を開けません
- **Interactivity** を On — Socket Mode でも要ります。無いと Allow / Deny のボタンが届きません
- **App Home → Messages Tab** を On、読み取り専用にしない — 無いとボットに DM できず、`login` が打てません

</details>

## マシンを足す

```bash
agentgw add-child user@host                   # ssh で入れる相手なら何でも(~/.ssh/config の別名も可)
agentgw add-child user@host --name build-box  # マシンの名前を決める(既定はホスト名)
```

`add-child` は相手のマシンを見て、合うバイナリとインストーラを届け、親につなぐ設定を書き、サービスを起こし、**親から見えるようになるまで待ちます**。あとでもう一度打てば、そのマシンを親と同じ版に入れ替えます。

親と子のつなぎ方は、推測ではなく試して決めます。

| | どうつなぐか | いつ使うか |
|---|---|---|
| **直結** | 子が親の公開名(`AGENTGW_LINK_PUBLIC_URL`、無ければ親の Tailscale の名前)に接続する | 親に届く場合 — 例: 親で `tailscale serve --bg 8787` |
| **ssh トンネル** | 親が子へ `ssh -N -R` を張り続け、子は自分の loopback に接続する | 20秒待っても直結でつながらなかった場合 |

親で `agentgw status` を打つと、子ごとにどちらでつながっているかが出ます。

## Slack のコマンド

ボットにメンションする(`@agentgw <コマンド>`)か、ボットが作業中のスレッドに打ちます。

| コマンド | すること |
|---|---|
| `stop` | 実行中のターンを止める(🛑 のリアクションでも可) |
| `exit` / `bye` / `done` | このスレッドのワーカーを終える(次に書けばスレッドは再開する) |
| `compact` | コンテキストを圧縮する(進捗バー付き) |
| `model [fable\|opus\|sonnet\|haiku]` | モデルを見る・切り替える |
| `effort [low\|medium\|high\|xhigh\|max\|…]` | effort を見る・決める |
| `mode [manual\|plan\|edit\|auto]` | 権限モードを見る・切り替える |
| `context` | このワーカーのコンテキストの使用量 |
| `resume` | このセッションを手元の端末で続けるコマンドを出す |
| `usage` | Claude のサブスクリプションの使用状況 |
| `status` | 版・動いているスレッド・温めてあるワーカー |
| `route [マシン]` | 担当の一覧を見る / このチャンネルの担当を決める |
| `pwd [絶対パス]` | このチャンネルの作業ディレクトリを見る・決める |
| `warm on\|off` | このチャンネルのワーカーを温めておくか |
| `set-home` | 通知(online / offline / エラー)をこのチャンネルに出す |
| `allow-bot @bot` / `remove-bot @bot` | ほかのボットのメッセージを通す / 通さない |
| `login` / `logout` | サインイン(DM で)/ サインアウトして全ワーカーを止める |
| `help` | コマンドの一覧 |

## CLI

```bash
agentgw status             # 役割、子とそのつなぎ方、チャンネルの担当
agentgw restart            # 新しいバイナリに入れ替える(動いているワーカーはそのまま)
agentgw shutdown           # サービスとワーカーを止める
agentgw start              # サービスを起こす
agentgw uninstall          # サービスの定義を消す(トークンと状態は残る)
agentgw add-child <ssh>    # マシンを足す・入れ替える
agentgw --version
```

## 置かれるファイル

どのマシンも同じ形です。

| 場所 | 中身 |
|---|---|
| `~/.local/bin/agentgw` | 本体 |
| `~/.local/state/agentgw/` | 状態の置き場(`AGENTGW_STATE_DIR` で変えられる) |
| ├ `.env` | 設定とトークン(権限 600) |
| ├ `access.json` | Owner、通知先、チャンネルの担当、作業ディレクトリ、温めたワーカー |
| ├ `threads.json` | Slack のスレッドとセッションの対応 |
| ├ `plugin-debug.log` | 全体のログ(大きくなると `.1` に回す) |
| ├ `logs/` | スレッドごと・セッションごとのログと、サービス自身の `service.out.log` / `service.err.log` |
| └ `inbox/` | ダウンロードした Slack の添付 |
| `~/Library/LaunchAgents/com.agentgw.bridge.plist` | サービスの定義(macOS) |
| `~/.config/systemd/user/agentgw-bridge.service` | サービスの定義(Linux) |
| `$TMPDIR/agentgw-agentgw/` | ワーカーに渡す hook と MCP の設定(起動のたびに作り直す) |
| tmux のセッション `agentgw-workers` | ワーカー1本 = 窓1つ |

## セキュリティ

- **Owner の言うことしか聞かない。** 最初に `login` した人が Owner になり、ほかの人のコマンドは断ります。
- **ワーカーは agentgw を入れたユーザーの権限で動きます。** そのユーザーにできることは何でもできます。分けたいときは `root` ではなく専用のユーザーで入れてください(例: `agentgw add-child agent@host`)。
- **トークンは手元にだけ置きます。** `.env` に権限 600 で置きます。子は app トークンを受け取らず、bot トークンを接続のたびに受け取ってメモリにだけ置きます。
- **親と子の接続の鍵はパスワードと同じです。** 持っている人は子として参加し、そのマシン宛のメッセージを受け取れます。`add-child` は ssh の標準入力で渡し、コマンドラインには載せません。
- **子にポート開放は要りません**(外向きに接続します)。親の口は既定で `0.0.0.0:8787` に開き、TLS は自分では終端しません。Tailscale か ssh トンネルの内側に置くか、`AGENTGW_LINK_LISTEN=127.0.0.1:8787` にしてください。
- **macOS のコード署名。** 端末から実行すると、インストーラが自己署名の署名用の身元を一度だけ作ります。これで更新しても macOS の許可が消えません。無い場合は、更新のたびに macOS が許可を訊き直します。

## 困ったとき

| 症状 | 見るところ |
|---|---|
| Slack で何も起きない | `agentgw status`、次に `~/.local/state/agentgw/plugin-debug.log` |
| サービスが起きない・再起動を繰り返す | `~/.local/state/agentgw/logs/service.err.log` |
| Allow / Deny のボタンを押しても何も起きない | Slack アプリの設定で **Interactivity** を On に |
| 一部のメッセージにしか返事が来ない | 同じ app トークンで別のプロセスもつながっている(Slack はイベントを振り分ける) |
| 子がオフラインになっている | 親の `agentgw status` でつなぎ方を見る。子の `service.err.log` を見る |
| 更新後、ワーカーが新しいツールの引数を使えない | そのワーカーで `/mcp` を打つ(起動時のツール一覧を持ち続けているため) |

## 開発

```bash
cargo build && cargo test
AGENTGW_STATE_DIR=$HOME/.local/state/agentgw-dev ./target/debug/agentgw serve

cargo dist              # 配るバイナリを作る(macOS arm64 と Linux x86_64 の musl 版)
./scripts/release.sh    # GitHub の release に上げる
```

`cargo dist` には [`cargo-zigbuild`](https://github.com/rust-cross/cargo-zigbuild)、zig、対象ごとの `rustup target add` が要ります。

同じマシンで別の agentgw を並べて動かしても構いません。ただし `AGENTGW_STATE_DIR` を分け、**Slack アプリも別のもの**を使ってください。同じ app トークンに2つつながると、Slack はイベントを振り分けるので、両方が半分ずつ取りこぼします。

## ライセンス

[MIT](LICENSE)
