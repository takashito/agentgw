# agentgw

Slack から、手元のマシンで動く Claude Code を操るためのゲートウェイ。

Slack のスレッドがそのまま作業単位になります。スレッドに書くと、そのスレッド専属の
ワーカー(tmux の中で動く Claude Code)が仕事をして、Slack に返事を書きます。
**複数のマシン**をつなげば、チャンネルごとに担当のマシンを決められます。

```
Slack ──(Socket Mode)── agentgw(親) ─── tmux + Claude Code
                            │
                ┌───────────┴───────────┐
          agentgw(子)             agentgw(子)
         tmux + Claude Code      tmux + Claude Code
```

- Slack につながるのは**親1台だけ**です。子は親に外向きに接続するので、子の側に
  ポート開放は要りません
- 親も子もワーカーを持ちます。担当を決めていないチャンネルは親が受けます
- 子が1台も無ければ、1台だけで完結します

---

## 要るもの

- macOS か Linux(サービスは launchd / systemd の user unit で動きます)
- `tmux`(無ければ `install.sh` が入れます)
- [Claude Code](https://docs.claude.com/en/docs/claude-code) — 入れてサインインしておく
- Slack アプリ — **Socket Mode を On**、bot トークン(`xoxb-…`)と app トークン(`xapp-…`)
  - **Interactivity も On** にしてください。ツール許可のボタンが届かなくなります
    (Socket Mode でも必要。Request URL は要りません)

## 入れる

```bash
bash -c "$(curl -fsSL https://raw.githubusercontent.com/takashito/agentgw/main/scripts/install.sh)"
```

(`curl … | bash` ではなくこの形にしてください。パイプで渡すと標準入力が端末でなくなり、
トークンを訊けません。)

GitHub の release からこのマシン用のバイナリを取り、`~/.local/bin/agentgw` に置いて、
サービスとして起動します。初回は「親か子か」と Slack のトークンを訊きます。

入ったら、Slack で自分がボットに **`login` を DM** してサインインを済ませます。
最初に DM した人が Owner(ボットに命令できる人)になります。

Mac では、初回に**署名の身元**(自己署名の証明書)を login キーチェーンに1つ作ります。
入れ直しても macOS の許可(ファイアウォールやファイルへのアクセス)が消えないようにするためで、
作るときにキーチェーンのパスワードを訊かれます。

<details>
<summary>ソースから入れる</summary>

```bash
cargo build --release
./scripts/install.sh --from target/release/agentgw
```

</details>

## マシンを足す

親のマシンから、ssh で入れる相手を指定します。

```bash
agentgw add-child user@host
```

これ1本で、相手にバイナリを届け、親につなぐ設定を書き、サービスとして起動し、
**親から見えるようになるまで待ちます**。ssh 先は `~/.ssh/config` の別名でも構いません
(鍵も踏み台も ssh の設定に任せます)。

子から親への通り道は、試して決めます。

1. **直結** — 親の公開名(`AGENTGW_LINK_PUBLIC_URL`、無ければ Tailscale のマシン名)で
   つながるかを試します。いちばん手軽なのは親で `tailscale serve --bg 8787`
2. **ssh トンネル** — 直結で届かなければ、親が子へ ssh トンネルを張ります。トンネルは
   親の agentgw が動いている間だけ張られます

もう一度 `add-child` を打てば、その子を親と同じ版に入れ替えます。

## Slack で使う

ボットにメンションするか、スレッドで話しかけます。主なコマンド:

| コマンド | すること |
|---|---|
| `route <マシン>` | このチャンネルの担当マシンを決める |
| `route` | ここの担当と、ほかのチャンネル・マシンの様子 |
| `set-home` | このチャンネルを通知先(home)にする |
| `pwd <絶対パス>` | このチャンネルで作業するディレクトリを決める |
| `stop` | 実行中のターンを止める(🛑 のリアクションでも可) |
| `model` / `effort` / `mode` | このスレッドのワーカーの設定を見る・変える |
| `usage` | Claude のサブスクの使用状況 |
| `status` | 稼働状況(版・動いているスレッド) |
| `help` | コマンドの一覧 |

## 運用

```bash
agentgw status      # 役割・子の一覧と経路(直結 / ssh トンネル)・チャンネルの担当
agentgw restart     # 入れ替え(走っているワーカーは畳まない)
agentgw shutdown    # 止める(ワーカーも畳む)
agentgw uninstall   # サービスの定義を消す(トークンと状態は残る)
```

### 置かれるもの

親も子も同じ形です。

| 場所 | 中身 |
|---|---|
| `~/.local/bin/agentgw` | 本体(1ファイル) |
| `~/.local/state/agentgw/` | 状態の置き場(`AGENTGW_STATE_DIR` で変えられる) |
| ├ `.env` | 設定(権限 600)。親は Slack のトークンと子を迎える口・鍵、子は親の URL と鍵 |
| ├ `access.json` | Owner・通知先・チャンネルの担当・作業ディレクトリ・ワーカーの在庫 |
| ├ `threads.json` | Slack のスレッドとワーカーのセッションの対応 |
| ├ `plugin-debug.log` | 全体のログ(大きくなると `.1` に回す) |
| ├ `logs/` | スレッドごと・セッションごとのログと、サービスの標準出力・エラー(`service.*.log`) |
| └ `inbox/` | Slack の添付をダウンロードした先 |
| サービス定義 | macOS: `~/Library/LaunchAgents/com.agentgw.bridge.plist` / Linux: `~/.config/systemd/user/agentgw-bridge.service` |
| `$TMPDIR/agentgw-agentgw/` | ワーカーに渡す hook と MCP の設定(起動のたびに作り直す) |
| tmux のセッション `agentgw-workers` | ワーカー1本 = 窓1つ |

調べものはまず `plugin-debug.log`、起動しない・すぐ落ちるときは `logs/service.err.log` を
見てください。

サービスの定義には状態の場所が焼き込まれるので、試験用の2つ目を同じマシンで並べて
動かせます(`AGENTGW_STATE_DIR=~/.local/state/agentgw-dev ./scripts/install.sh`)。
その場合、Slack アプリも**別のもの**を使ってください — 同じアプリに2つ目がつながると、
Slack はイベントを複製せずに振り分けるので、両方が半分ずつ取りこぼします。

### 壊れ方

- **親が落ちる** → 新しいメッセージはどのマシンにも届きません。走っているワーカーは
  走り続け、子は裏で繋ぎ直します
- **子が落ちる** → そのチャンネルには「オフラインです」と返します。溜め込みません
- 子の出入りは home チャンネルに `online` / `disconnected` として出ます

## 開発

```bash
cargo build && cargo test
AGENTGW_STATE_DIR=$HOME/.local/state/agentgw-dev ./target/debug/agentgw serve

cargo dist                 # 配る物を作る(Mac 用 + Linux の musl 用)
./scripts/release.sh       # それを GitHub release に上げる
```

`cargo dist` には `cargo-zigbuild` と zig、それに `rustup target add <triple>` が要ります。

## ライセンス

MIT
