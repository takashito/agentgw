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

| | |
|---|---|
| 状態 | `~/.local/state/agentgw/`(`AGENTGW_STATE_DIR` で変えられる) |
| 設定 | 状態の中の `.env` |
| launchd | `com.agentgw.bridge` |
| systemd | `agentgw-bridge.service`(user unit) |

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
