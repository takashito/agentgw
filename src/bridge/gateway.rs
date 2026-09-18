//! 親子の link に関わるものは全部ここ — フレームの契約から、誰を中に入れるか、
//! 届いたものを誰に渡すか、フリートの出入りまで。
//!
//! 節の並び:
//!
//! 1. プロトコル(`mod link`)  フレーム / 接続文字列(`Invite`)/ subprotocol — 純データ
//! 2. 受け入れの判断           `Admit`
//! 3. 接続簿                   `Conn` / `LinkServer`
//! 4. 届いたものの読み方と行き先 `Event` / `Click` / `Delivery` / `NoticeCooldown`
//! 5. コマンドの判断           `CommandCtx` / `DmOnboardingCtx`
//! 6. presence                 `Presence` / `FleetView`
//! 7. 親の口                   `Fleet`(axum のハンドラと link 1本の一生、親→子 dial)
//! 8. 子の口                   `Inlet`(親に迎えに来てもらう構成でだけ開く)
//! 9. CLI                      `Cli`(`status` のフリート欄)
//!
//! **1〜6 は純関数**(時計もソケットもワーカーも知らない)。テストが全部同期で回るのは
//! そのためで、**7〜9 のものをここへ持ち込んだ日にそれが壊れる** — 以前は別ファイルなので
//! grep で確かめられたが、同じモジュールに入った今は、この並びとテストの回り方が代わりの担保。

// ── 節1: プロトコル ─────────────────────────────────────
pub mod wire {
    //! Relay ⇄ Bridge の link プロトコル — WebSocket に載るフレームと、どこへ dial すればいいかを
    //! 伝える接続文字列。純データだけで I/O を持たない(Relay と Bridge の**両方**が読む契約なので、
    //! どちらかの都合を1行でも混ぜたら二枚舌になる)。
    //!
    //! **握手はここに無い。** 誰を入れるか(api トークン)・同じ言葉を喋るか(版)・どのマシンか
    //! (名乗り)・受け入れたか、の4つは**すべて WebSocket の upgrade でやる**:
    //!
    //! ```text
    //! GET /bridge/desktop HTTP/1.1
    //! Upgrade: websocket
    //! Authorization: Bearer <api token>
    //! Sec-WebSocket-Protocol: sclink.1
    //! ```
    //!
    //! 通れば 101、通らなければ 401(トークン)/ 426(版)/ 400(パス)。だから
    //! 「最初のフレームは名乗りでなければならない」という状態機械も、名乗らない接続を切るための
    //! 期限タイマーも要らない — **フレームが流れる時点で、相手はもう認証を通っている**。
    //!
    //! 移植元(Bun)は hello / welcome / reject という3つのフレームで同じことをやっていた。
    //! それはワイヤ互換のために踏襲する価値があったが、互換を取らないと決めた以上、
    //! 既にあるものの作り直しでしかない。

    use serde::{Deserialize, Serialize};

    /// upgrade で突き合わせる版。フレームの形か意味を変えたら上げる。
    ///
    /// バイナリの semver とはわざと別物にしてある — Relay と Bridge は別々のリリース周期を持つ
    /// 別プログラムで、版が揃わないのが常態。揃っていなければならないのはフレームの**意味**だけ。
    /// 食い違う相手は upgrade の時点で断る(426)。新旧が黙ってすれ違って話し続ける方がずっと悪い。
    pub const LINK_SUBPROTOCOL: &str = "sclink.1";

    /// Bridge が dial するパスの頭。この後ろ1セグメントがマシンの名前。
    const BRIDGE_PATH: &str = "/bridge/";

    /// `link` が自己到達を確かめるときに叩くパス。
    ///
    /// **マシンの名前空間に予約語を置かない。** 別のパスにしてあるので、どんな名前のマシンとも
    /// 衝突しないし、名前として通せる字かどうかの検査に巻き込まれることもない
    /// (`__link_probe__` という予約名でやろうとして、名前の検査に弾かれた — 実機で発覚)。
    pub const PROBE_PATH: &str = "/probe";

    /// 握手の**後**に流れるフレーム。4種しかなく、**全部 Relay → Bridge 向き**
    /// (Bridge は握手を済ませたら何も送り返さない — 送るべきことが無い)。
    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
    #[serde(tag = "t", rename_all = "snake_case")]
    pub enum LinkFrame {
        /// 101 の直後に Relay が投げる最初の1本。
        ///
        /// Slack の **bot** トークンを渡す(Bridge は Slack へ自分で書くので要る)。**app トークンは
        /// 渡さない** — あれは Socket Mode を開くためだけのもので、イベント列の2人目の消費者は
        /// この設計が防いでいる失敗そのもの。Bridge はこの bot トークンを**メモリにだけ**置く。
        ///
        /// `home` は毎回の握手で現在値を渡す。後から(再)接続したマシンが、`set-home` を聞き逃して
        /// いても追いつくのはこれのおかげ。欠けている = まだ home 未設定で、Bridge 側の現在値は
        /// そのまま(「消す」という指示は無い)。
        Ready {
            bot_token: String,
            #[serde(skip_serializing_if = "Option::is_none")]
            home: Option<String>,
        },
        /// Slack のイベント1つを、そのチャンネルの担当マシンへ転送する。
        ///
        /// `event` は slack-morphism の型を `to_value` したもの。受け側は同じ型に戻すので、
        /// 同じ serde 実装での往復になり無損失。
        Event {
            name: String,
            event: serde_json::Value,
        },
        /// ボタン1押しを同じ経路で転送する。
        ///
        /// Slack への `ack` は**もう Relay が返している**(3秒以内にソケットを持っている側が返せ、
        /// というのが Slack の要求)。Bridge は ack せず、判断だけする。書き戻しは
        /// `body.response_url` — あれはトークン不要なので「Slack への書き込みは Bridge から」の
        /// 約束が保たれる。
        ///
        /// **`action_id` は持たない** — `action.action_id` を読めば済む。移植元は Bolt が別々に
        /// 渡してくるという都合で同じ値を2回入れていた。
        Action {
            action: serde_json::Value,
            body: serde_json::Value,
        },
        /// Owner がこのマシンをある場所(DM / チャンネル)の担当に決めた、という通知。
        ///
        /// これが来るまで Relay 経由の Bridge は Slack 上の自分の身元を何も知らない(持っているのは
        /// Relay)。誰が Owner で、どこ(チャンネルとスレッド)へ返せばいいかの3つだけを渡す。
        /// トークンは渡さない — 書くのは Bridge が bot トークンで自分でやる。
        Linked {
            owner_user_id: String,
            channel: String,
            thread_ts: String,
        },
    }

    /// WebSocket が既にメッセージを区切ってくれるので、1メッセージ = 1フレーム。
    /// UDS の NDJSON と違い「途中で切れた尻尾」を考えなくていい。
    pub fn encode(f: &LinkFrame) -> String {
        serde_json::to_string(f).unwrap_or_default()
    }

    /// 1メッセージを1フレームに。知らないもの・形の違うものは `None`(= フレームではない)。
    pub fn decode(raw: &str) -> Option<LinkFrame> {
        serde_json::from_str(raw).ok()
    }

    /// dial 先のパスからマシンの名前を読む。`/bridge/desktop` → `desktop`。
    ///
    /// 通すのは1セグメントだけで、字も絞る(`[A-Za-z0-9][A-Za-z0-9_.-]*`)。名前はログにも
    /// `route` の表にも出るし、`..` のようなものを名前として受けて得することは何も無い。
    pub fn bridge_id_of_path(path: &str) -> Option<&str> {
        let id = path.strip_prefix(BRIDGE_PATH)?;
        let mut cs = id.chars();
        let first = cs.next()?;
        (first.is_ascii_alphanumeric()
            && cs.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-'))
        .then_some(id)
    }

    /// マシンが dial するパスを組む(`link` が接続文字列に URL を載せるときは付けない —
    /// 付けるのは繋ぐ側)。
    pub fn path_for(bridge_id: &str) -> String {
        format!("{BRIDGE_PATH}{bridge_id}")
    }

    // ── 節1b: 接続文字列(相手に貼らせる1本) ───────────────────────────
    // マシンが Relay に届くには2つ要る: dial 先の URL と、提示する秘密。別々に打たせるのは
    // 「何にも繋がらないのに理由を言えないマシン」を作る機会が2回あるということなので、Relay は
    // 1本の文字列として出し、向こうはその1本を貼る。**この文字列はパスワードそのもの** —
    // 持っている者は Bridge として繋ぎ、そのマシン宛の Slack メッセージを受け取れる。
    //
    // (握手を upgrade に移しても、ここは何も変わらない — URL と秘密を1本で渡す価値は
    //  ワイヤ互換とは無関係だから。)

    /// 接頭辞が版。将来フォーマットを変えたら「読み違い」でなく「はっきりした拒否」になる。
    const PREFIX: &str = "SCLINK1-";

    /// 相手に貼らせる1本の中身 — dial 先と鍵。**接続簿の [`Conn`](super::Conn) とは別物**
    /// (あちらは繋がった link の書き手側)。
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Invite {
        /// dial 先。公開 WebSocket URL(実運用では `wss://…`)。**パスは付けない。**
        pub url: String,
        /// 中に入れてもらうために提示する秘密。`Authorization: Bearer` に載る。
        pub api_token: String,
    }

    /// 中身は `{"u":<url>,"t":<apiToken>}` の base64url(padding 無し)。
    ///
    /// **キーの順は `u` → `t`**。`json!` マクロで組むと `serde_json::Map` が BTreeMap なので
    /// アルファベット順(`t` が先)になる — だから struct で書いて宣言順を固定する。
    pub fn encode_connection(c: &Invite) -> String {
        #[derive(Serialize)]
        struct Wire<'a> {
            u: &'a str,
            t: &'a str,
        }
        let json = serde_json::to_string(&Wire {
            u: &c.url,
            t: &c.api_token,
        })
        .unwrap_or_default();
        format!("{PREFIX}{}", b64url_encode(json.as_bytes()))
    }

    /// 読み戻す。ちょうど1本の接続文字列でないものは全部断る。
    ///
    /// 実際に起きる失敗は**貼り付けの千切れ**だ — チャットが折り返した行の半分でも、それらしい
    /// blob に見えてしまう。半分の秘密を持ったマシンが黙って出来上がるのが最悪なので、断る文面は
    /// 「どこが欠けたのか」を名前で言う。
    pub fn decode_connection(raw: &str) -> Result<Invite, String> {
        let text = raw.trim();
        let Some(body) = text.strip_prefix(PREFIX) else {
            return Err(format!(
                "not a connection string — it must start with \"{PREFIX}\" (did the paste lose its beginning?)"
            ));
        };
        let v: serde_json::Value = b64url_decode(body)
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .ok_or(
            "the connection string is damaged — it does not decode (a truncated or re-wrapped paste?)",
        )?;
        let Some(o) = v.as_object() else {
            return Err(
                "the connection string is damaged — it does not decode to a connection".into(),
            );
        };
        let field = |k: &str| o.get(k).and_then(|v| v.as_str()).filter(|s| !s.is_empty());
        match (field("u"), field("t")) {
            (Some(url), Some(api_token)) => Ok(Invite {
                url: url.to_string(),
                api_token: api_token.to_string(),
            }),
            (u, t) => {
                // 欠けたものを**名前で**言う。黙って既定値で埋めない。
                let missing: Vec<&str> = [("url", u.is_none()), ("api token", t.is_none())]
                    .into_iter()
                    .filter(|(_, m)| *m)
                    .map(|(n, _)| n)
                    .collect();
                Err(format!(
                    "the connection string is incomplete — it is missing its {}",
                    missing.join(" and ")
                ))
            }
        }
    }

    /// `base64` クレートの URL-safe・パディング無し。**アルファベット外の1文字でも `None`** —
    /// 緩く読むと千切れた貼り付けが通ってしまう(`+` や `/` は標準 base64 の字なので受けない)。
    fn b64url_encode(bytes: &[u8]) -> String {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    }

    fn b64url_decode(text: &str) -> Option<Vec<u8>> {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(text.trim_end_matches('='))
            .ok()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn frames() -> Vec<LinkFrame> {
            vec![
                LinkFrame::Ready {
                    bot_token: "xoxb-1".into(),
                    home: Some("C_HOME".into()),
                },
                LinkFrame::Ready {
                    bot_token: "xoxb-1".into(),
                    home: None,
                },
                LinkFrame::Event {
                    name: "message".into(),
                    event: serde_json::json!({"channel": "C1", "text": "hi"}),
                },
                LinkFrame::Action {
                    action: serde_json::json!({"action_id": "perm_allow"}),
                    body: serde_json::json!({"user": {"id": "U1"}}),
                },
                LinkFrame::Linked {
                    owner_user_id: "U_OWNER".into(),
                    channel: "D123".into(),
                    thread_ts: "1700000000.000100".into(),
                },
            ]
        }

        #[test]
        fn every_frame_round_trips() {
            for f in frames() {
                assert_eq!(decode(&encode(&f)).unwrap(), f, "{f:?}");
            }
        }

        /// home 未設定の Ready はキーごと出さない — Bridge 側の現在値をそのままにするため。
        #[test]
        fn ready_without_home_omits_the_key() {
            let f = LinkFrame::Ready {
                bot_token: "xoxb-1".into(),
                home: None,
            };
            assert_eq!(encode(&f), r#"{"t":"ready","bot_token":"xoxb-1"}"#);
        }

        /// 型が違えばフレームではない。serde が無料でやる厳しさに乗っている。
        #[test]
        fn a_frame_with_a_wrong_type_is_not_a_frame() {
            for bad in [
                r#"{"t":"ready","bot_token":0}"#,
                r#"{"t":"ready"}"#,                  // bot_token が無い
                r#"{"t":"event","name":"message"}"#, // event が無い
                r#"{"t":"linked","owner_user_id":"U","channel":"D1"}"#, // thread_ts が無い
                r#"{"t":"linked","owner_user_id":"U","channel":"D1","thread_ts":12}"#,
            ] {
                assert!(decode(bad).is_none(), "{bad}");
            }
        }

        /// 旧 Bun の握手フレームは、もう「知らないフレーム」でしかない。
        #[test]
        fn junk_and_unknown_frame_kinds_decode_to_nothing() {
            for raw in [
                "",
                "not json",
                "[]",
                "null",
                "\"ready\"",
                r#"{"t":"hello","protocol":4,"bridgeId":"a","apiToken":"b"}"#,
                r#"{"t":"welcome","protocol":4,"botToken":"x"}"#,
                r#"{"t":"reject","reason":"bad_api_token","message":"no"}"#,
                r#"{"t":"from-the-future"}"#,
            ] {
                assert!(decode(raw).is_none(), "{raw}");
            }
        }

        /// **回帰**: 到達確認のパスは、名前の検査に巻き込まれない別物であること。
        /// 予約名 `__link_probe__` を `/bridge/` の下に置いたとき、先頭が英数字でないという理由で
        /// 400 になり、`relay link` が自分に届かないと報告した(実機で発覚)。
        #[test]
        fn the_probe_has_its_own_path_outside_the_machine_namespace() {
            assert!(bridge_id_of_path(PROBE_PATH).is_none());
            assert!(!PROBE_PATH.starts_with(BRIDGE_PATH));
        }

        #[test]
        fn a_bridge_id_is_read_from_the_path() {
            for (path, want) in [
                ("/bridge/desktop", Some("desktop")),
                ("/bridge/mac-mini.local", Some("mac-mini.local")),
                ("/bridge/a_1", Some("a_1")),
                ("/bridge/A", Some("A")),
            ] {
                assert_eq!(bridge_id_of_path(path), want, "{path}");
            }
            for bad in [
                "/bridge/",         // 名前が無い
                "/bridge/a/b",      // 2セグメント
                "/",                // そもそも違う
                "/bridge",          // 区切りが無い
                "/bridge/../etc",   // 通す理由が無い
                "/bridge/-leading", // 先頭は英数字だけ
                "/bridge/with space",
                "/status",
            ] {
                assert!(bridge_id_of_path(bad).is_none(), "{bad}");
            }
        }

        #[test]
        fn a_path_round_trips_with_its_id() {
            for id in ["desktop", "mac-mini.local", "a_1"] {
                assert_eq!(bridge_id_of_path(&path_for(id)), Some(id));
            }
        }

        fn conn() -> Invite {
            Invite {
                url: "wss://remote.example.com".into(),
                api_token: "a-very-long-secret".into(),
            }
        }

        #[test]
        fn connection_string_round_trips() {
            let s = encode_connection(&conn());
            assert!(s.starts_with("SCLINK1-"));
            assert_eq!(decode_connection(&s).unwrap(), conn());
            // 貼り付けに付いてくる空白・改行は落とす
            assert_eq!(decode_connection(&format!("\n  {s}  \n")).unwrap(), conn());
        }

        /// キーの順が `u` → `t` に固定されていること(BTreeMap で組むと入れ替わる)。
        #[test]
        fn the_payload_keeps_its_key_order() {
            let s = encode_connection(&conn());
            let json = String::from_utf8(b64url_decode(&s[PREFIX.len()..]).unwrap()).unwrap();
            assert!(json.starts_with(r#"{"u":"#), "{json}");
        }

        /// 版が先頭にあるので、将来のフォーマットは「読み違い」でなく拒否になる。
        #[test]
        fn a_future_prefix_is_refused() {
            let s = encode_connection(&conn());
            assert!(decode_connection(&format!("SCLINK9-{}", &s[8..])).is_err());
        }

        #[test]
        fn a_truncated_paste_is_refused_by_name() {
            let s = encode_connection(&Invite {
                url: "wss://h:1".into(),
                api_token: "t".into(),
            });
            let err = decode_connection(&s[..s.len() - 8]).unwrap_err();
            assert!(
                err.contains("damaged") || err.contains("incomplete"),
                "{err}"
            );
            // 長い方も、どこで千切れても半分の秘密を作らない
            let whole = encode_connection(&conn());
            for cut in [3, 5, 7, 9] {
                let half = &whole[..whole.len() * cut / 10];
                assert!(decode_connection(half).is_err(), "{half}");
            }
        }

        #[test]
        fn junk_is_refused() {
            for junk in ["", "hello", "SCLINK1-", "SCLINK1-!!!!"] {
                assert!(decode_connection(junk).is_err(), "{junk}");
            }
        }

        /// 欠けたフィールドは名前で言う — 黙って既定値で埋めない。
        #[test]
        fn a_string_missing_a_field_is_refused_by_name() {
            let half = format!(
                "SCLINK1-{}",
                b64url_encode(br#"{"u":"wss://x"}"#.as_slice())
            );
            assert!(decode_connection(&half).unwrap_err().contains("api token"));
        }

        #[test]
        fn base64url_handles_every_tail_length() {
            for n in 0..8usize {
                let bytes: Vec<u8> = (0..n as u8).map(|i| i.wrapping_mul(37) ^ 0xF0).collect();
                let s = b64url_encode(&bytes);
                assert!(!s.contains('='), "{s}");
                assert_eq!(b64url_decode(&s).unwrap(), bytes);
            }
            assert!(b64url_decode("a").is_none()); // 4n+1 は正しい符号化ではない
            assert!(b64url_decode("ab+d").is_none()); // 標準 base64 の文字は受けない
        }
    }
}

use crate::bridge::state::LogCtx;
use wire::LINK_SUBPROTOCOL;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

/// チャンネル id → 担当マシンの名前。access.json の `routes[ch].bridge` から起こす
/// ([`crate::bridge::state::Access::bridges`])。
pub type Routes = BTreeMap<String, String>;

/// この節の1行ログ。component は `relay` で固定 — フリートの出来事だけをここに集める。
pub(super) fn rlog(level: &str, message: &str) {
    let ctx = LogCtx::default();
    match level {
        "error" => ctx.error("relay", message),
        "debug" => ctx.debug("relay", message),
        _ => ctx.info("relay", message),
    }
}

// ── 節2: 誰を中に入れるか(upgrade を通すかどうか) ──────────────────────
// 握手は WebSocket の upgrade でやる(link.rs のモジュール doc)。ここはその判断だけを持つ
// 純関数で、ソケットもヘッダの型も知らない — だからテストが同期で全部回る。

/// upgrade を通すか、断るなら何を返すか。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admit {
    /// 通す。名乗ったマシンの名前。
    Ok(String),
    /// `link` の到達確認。**通すが接続簿に載せない** — 載せると一瞬「新しいマシンが繋がった」
    /// 扱いになり、home に 🟢 が出てしまう。専用のパスなので、どんなマシン名とも衝突しない。
    Probe,
    /// 401 — api トークンが無い / 違う。**誰が繋いでよいかを決めるただ1つの検査**。
    Unauthorized,
    /// 426 — 同じ link プロトコルを喋っていない。新しいコードで再起動すれば治る種類。
    WrongVersion,
    /// 400 — パスにマシンの名前が無い(または名前として通せない字が入っている)。
    BadPath,
}

impl Admit {
    /// 断るときに返す HTTP ステータス。通すときは 101 なので `None`。
    pub fn status(&self) -> Option<u16> {
        match self {
            Admit::Ok(_) | Admit::Probe => None,
            Admit::Unauthorized => Some(401),
            Admit::WrongVersion => Some(426),
            Admit::BadPath => Some(400),
        }
    }

    /// 断った理由を1行で。**黙って 401 を返すと1時間デバッグさせる。**
    pub fn why(&self) -> &'static str {
        match self {
            Admit::Ok(_) => "admitted",
            Admit::Probe => "admitted (reachability probe)",
            Admit::Unauthorized => "the api token is missing or wrong",
            Admit::WrongVersion => "the two sides do not speak the same link protocol",
            Admit::BadPath => "the path carries no usable Bridge ID",
        }
    }

    /// upgrade の3点(パス / `Authorization` / `Sec-WebSocket-Protocol`)を見て決める。
    ///
    /// **順序が仕様**: ①同じ言葉を喋るか → ②api トークン → ③**はじめて**名乗りを信じる。
    /// `desktop` と名乗れることが desktop 宛の Slack メッセージを受け取れる理由になってはいけない
    /// 版を先に見るのは、食い違いを「トークンが違う」と誤って報告しないため。
    pub fn of(
        path: &str,
        authorization: Option<&str>,
        subprotocol: Option<&str>,
        api_token: &str,
    ) -> Admit {
        // ① Sec-WebSocket-Protocol はカンマ区切りで複数来うる。1つでも一致すればよい。
        let speaks =
            subprotocol.is_some_and(|v| v.split(',').any(|p| p.trim() == LINK_SUBPROTOCOL));
        if !speaks {
            return Admit::WrongVersion;
        }
        // ② `Bearer <token>`。ここを通るまで、名乗りはただの文字列。
        let presented = authorization
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("");
        if !secret_eq(presented, api_token) {
            return Admit::Unauthorized;
        }
        // ③ ここでようやく名前を読む。到達確認は名前を名乗らない(専用のパス)。
        if path == wire::PROBE_PATH {
            return Admit::Probe;
        }
        match wire::bridge_id_of_path(path) {
            Some(id) => Admit::Ok(id.to_string()),
            None => Admit::BadPath,
        }
    }
}

/// 定数時間の秘密比較。長さ違いは中身を見ずに `false`(長さは秘密ではない)。
///
/// このためだけに依存を1つ増やさない。
pub(crate) fn secret_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

// ── 節3: いま誰が繋がっているか(接続簿) ───────────────────────────

/// 1本の link の書き手側。中身は「文字列を投げる口」だけ — 接続簿はソケットを知らないので、
/// テストがソケット無しで回る。
#[derive(Clone)]
pub struct Conn(Arc<tokio::sync::mpsc::UnboundedSender<String>>);

impl Conn {
    pub fn new(tx: tokio::sync::mpsc::UnboundedSender<String>) -> Self {
        Self(Arc::new(tx))
    }

    /// フレームを1本投げる。`false` = その link はもう死んでいる(受け手が落ちた)。
    pub fn send(&self, frame: &wire::LinkFrame) -> bool {
        self.0.send(wire::encode(frame)).is_ok()
    }

    /// **同じ link か**(中身の等値ではなく同一性)。古い link の後始末が新しい登録を
    /// 巻き込まないための鍵。
    fn is(&self, other: &Conn) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl std::fmt::Debug for Conn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Conn({:p})", Arc::as_ptr(&self.0))
    }
}

/// 繋がった結果。presence(🟢/🔴)に出すかどうかがここで決まる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Joined {
    /// 新しく来た。Owner に知らせる。
    New,
    /// 既に繋がっていたマシンが繋ぎ直した(Wi-Fi の瞬断など)。**知らせない** —
    /// Owner から見れば、そのマシンは居なくならなかった。
    Reconnected,
}

/// Bridge ID → いまその名前を務めている link。**転送はこの表だけを見る。**
#[derive(Default)]
pub struct LinkServer {
    bridges: Mutex<HashMap<String, Conn>>,
}

impl LinkServer {
    pub fn new() -> Self {
        Self::default()
    }

    /// 認証を通った link を登録する。返り値は「古いのを押しのけたか」と、押しのけられた link。
    ///
    /// **いちばん新しい link が勝つ。** Wi-Fi が切れたマシンは、こちらの古いソケットがまだ
    /// 死んだと分からないうちに繋ぎ直してくる。新参を断ると、死んだソケットが時間切れになるまで
    /// そのマシンは行方不明になる。
    pub fn register(&self, bridge_id: &str, conn: Conn) -> (Joined, Option<Conn>) {
        let mut bridges = self.bridges.lock().unwrap();
        match bridges.insert(bridge_id.to_string(), conn) {
            Some(old) => {
                rlog(
                    "info",
                    &format!("{bridge_id}: reconnected — closing its previous link"),
                );
                (Joined::Reconnected, Some(old))
            }
            None => {
                rlog(
                    "info",
                    &format!("{bridge_id}: linked (connected: {})", Self::names(&bridges)),
                );
                (Joined::New, None)
            }
        }
    }

    /// link が閉じた。`true` = **本当に居なくなった**(presence に出す)。
    ///
    /// `false` になるのは、押しのけられた古い link が後から閉じたとき。そこで名前ごと消すと、
    /// 生きている新しい link の登録が道連れになり、マシンが行方不明になる
    /// (移植元bridgeId を先に外していたのと同じ落とし穴)。
    pub fn unregister(&self, bridge_id: &str, conn: &Conn) -> bool {
        let mut bridges = self.bridges.lock().unwrap();
        match bridges.get(bridge_id) {
            Some(current) if current.is(conn) => {
                bridges.remove(bridge_id);
                rlog(
                    "info",
                    &format!(
                        "{bridge_id}: link closed (connected: {})",
                        Self::names(&bridges)
                    ),
                );
                true
            }
            _ => {
                rlog(
                    "debug",
                    &format!("{bridge_id}: a displaced link closed — the newer one stays"),
                );
                false
            }
        }
    }

    /// いま繋がっている Bridge ID。`route` が指し先にできるのはこれだけで、転送もこれを見る。
    pub fn connected(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.bridges.lock().unwrap().keys().cloned().collect();
        ids.sort();
        ids
    }

    pub fn is_connected(&self, bridge_id: &str) -> bool {
        self.bridges.lock().unwrap().contains_key(bridge_id)
    }

    /// 1本のマシンへフレームを投げる。`false` = そこには居なかった(表を引いてから投げるまでの
    /// 間に落ちた場合も含む)。**届いたふりをしない。**
    pub fn send_to(&self, bridge_id: &str, frame: &wire::LinkFrame) -> bool {
        let conn = self.bridges.lock().unwrap().get(bridge_id).cloned();
        conn.is_some_and(|c| c.send(frame))
    }

    fn names(bridges: &HashMap<String, Conn>) -> String {
        let mut ids: Vec<&str> = bridges.keys().map(String::as_str).collect();
        ids.sort();
        if ids.is_empty() {
            "none".to_string()
        } else {
            ids.join(", ")
        }
    }
}

// ── 節4: 届いたものの読み方と行き先 ─────────────────────────────
// route 表は「チャンネル → マシン」だけで、スレッド → マシンの表は**わざと持たない**。
// スレッドはチャンネルの中に居るので、チャンネルの持ち主がその中の全スレッドの持ち主。

/// Slack のイベント1件。**読み方をここに集める** — 生の `Value` を引数で回すと、同じ
/// `get("…")` が散らばって、どれが本文でどれがスレッドなのか追えなくなる。
pub struct Event<'a> {
    pub name: &'a str,
    pub raw: &'a serde_json::Value,
}

impl<'a> Event<'a> {
    pub fn new(name: &'a str, raw: &'a serde_json::Value) -> Self {
        Self { name, raw }
    }

    fn str_at(&self, key: &str) -> Option<&'a str> {
        self.raw.get(key)?.as_str()
    }

    /// このイベントが起きたチャンネル。
    ///
    /// ボタン押しは [`Click`] が持って来る — だからクリックに特別な routing が要らない
    /// (そして**決して撒いてはいけない**: 依頼を知らないマシンは「これはもう期限切れです」と
    /// 答えてしまう。それは嘘になる)。
    pub fn channel(&self) -> Option<&'a str> {
        match self.name {
            "message" | "member_joined_channel" => self.str_at("channel"),
            "reaction_added" | "reaction_removed" => self.raw.get("item")?.get("channel")?.as_str(),
            _ => None,
        }
    }

    pub fn user(&self) -> Option<&'a str> {
        self.str_at("user")
    }

    /// 人が実際に書いた文字。編集は1段下(`message.text`)に入る。
    pub fn text(&self) -> Option<&'a str> {
        self.str_at("text")
            .or_else(|| self.raw.get("message")?.get("text")?.as_str())
    }

    /// この message についての返事はどこへ入るか。スレッドの中なら中、無ければその下。
    pub fn thread(&self) -> Option<&'a str> {
        self.str_at("thread_ts").or_else(|| self.str_at("ts"))
    }

    /// bot 自身が書いたもの — **この断りたち自身を含む**。これが無いと、担当未設定の
    /// チャンネルは自分の「担当が居ません」に「担当が居ません」で答え続ける。
    pub fn from_a_bot(&self) -> bool {
        self.raw.get("bot_id").is_some_and(|v| !v.is_null())
            || self.str_at("subtype") == Some("bot_message")
    }

    /// Owner が自分の発言を**取り消した**。オンラインなら転送する(Bridge が中断に変える)が、
    /// 届けられないときは**再送するものも言うことも無い** — 黙って捨てる(ログには出す)。
    /// 編集は取り消しではない。
    pub fn is_retraction(&self) -> bool {
        self.name == "message" && self.str_at("subtype") == Some("message_deleted")
    }

    /// この投稿を bot として扱うか。**Owner の Web API 投稿だけは人**(
    /// `Access::gate` が既に持っている規則を、コマンドの入口にも効かせる)。
    ///
    /// Web API 経由の投稿には人が書いたものでも `bot_id` が付くが、Slack は本当の `user` も
    /// 刻む(トークン由来なので本文からは詐称できない)。Owner 以外・user 無し・Owner 未設定は
    /// bot 扱い = **閉じる方に倒す**(自分の投稿を自分で解釈して自分に返す道を作らない)。
    pub fn speaks_as_a_bot(&self, owner: Option<&str>) -> bool {
        if !self.from_a_bot() {
            return false;
        }
        !matches!((self.user(), owner), (Some(u), Some(o)) if u == o)
    }

    /// **親が配達より先に自分で見るか**(コマンド・名乗りの候補か)。
    ///
    /// 候補になるのは「チャンネルが読めた人の message」だけ。リアクションや join は
    /// コマンドになりえない。
    pub fn is_a_command_candidate(&self, owner: Option<&str>) -> bool {
        self.name == "message" && self.channel().is_some() && !self.speaks_as_a_bot(owner)
    }

    /// 親がこのイベントについて口を開いてよいか。
    ///
    /// リアクションや join は誰に宛てたものでもない。bot の言葉に答えてはいけない
    /// (無限ループ)。チャンネルでは名指しが要る — 親は Bridge のゲートより**わざと厳しい**:
    /// Bridge は「動いているスレッドの続き」にも応じるが、どのスレッドが生きているかを
    /// 知っているのはそのマシン自身で、留守のときに限って訊けないから。
    pub fn may_answer(&self, bot_user_id: Option<&str>) -> bool {
        let Some(channel) = self.channel() else {
            return false;
        };
        if self.name != "message" || self.from_a_bot() || self.is_retraction() {
            return false;
        }
        crate::bridge::command::SlackId::is_dm(channel)
            || crate::bridge::command::Message::new(self.text().unwrap_or(""), bot_user_id)
                .mentions_bot()
    }
}

/// ボタン1押し(Slack が寄越す `block_actions` の body)。
pub struct Click<'a>(pub &'a serde_json::Value);

impl Click<'_> {
    /// 押されたチャンネル。prompt が投稿された場所そのものなので、routing は message と同じ。
    pub fn channel(&self) -> Option<&str> {
        self.0.get("channel")?.get("id")?.as_str()
    }
}

/// 配達の判断。**マシンを勝手に選ばないし、留守のマシンのために預かりもしない** —
/// 当てずっぽうも溜め込みも黙って失敗するので、この設計は代わりに声に出す方を選ぶ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    /// 担当マシンが繋がっている。渡す。
    Forward(String),
    /// 担当は居るが、いまオフライン。
    Offline(String),
    /// このマシンが自分で処理する。**担当が書かれていない = ここ**(単独 Bridge の既定)。
    Local,
    /// どのチャンネルで起きたのかすら読めなかった。**黙って捨てず必ずログに出す。**
    UnknownChannel,
}

impl Delivery {
    pub fn decide(
        channel_id: Option<&str>,
        routes: &Routes,
        self_id: &str,
        is_connected: impl Fn(&str) -> bool,
    ) -> Delivery {
        let Some(channel) = channel_id else {
            return Delivery::UnknownChannel;
        };
        match routes.get(channel) {
            // **担当が決まっていないチャンネルは自分でやる。** 子を1台も持たない Bridge は
            // 表が空なので、ここを通って今までどおりに動く
            None => Delivery::Local,
            Some(bridge_id) if bridge_id == self_id => Delivery::Local,
            Some(bridge_id) if is_connected(bridge_id) => Delivery::Forward(bridge_id.clone()),
            Some(bridge_id) => Delivery::Offline(bridge_id.clone()),
        }
    }

    /// 担当マシンが留守のときに言うこと。**その場で言う。預からない。**
    /// 1時間後に、もう関心を失った人へ届く返事は、正直に断るより悪い。
    pub fn offline_notice(bridge_id: &str, connected: &[String]) -> String {
        let online = if connected.is_empty() {
            crate::t!("none", "なし")
        } else {
            connected.join(", ")
        };
        crate::t!(
            "*{bridge_id}*, the machine for this channel, is offline. Your message wasn't kept — \
             send it again once {bridge_id} is back.\nOnline now: {online}",
            "このチャンネルを受け持つマシン *{bridge_id}* はオフラインです。メッセージは保存していないので、\
             {bridge_id} が戻ってからもう一度送ってください。\nオンラインのマシン: {online}"
        )
    }
}

// ── 断りの上限 ───────────────────────────────────────
// マシンが留守の間もイベントは止まらないし、ひとつの質問は複数のイベントになる(打鍵・編集・
// 再配達)。だからチャンネルごとに1分1回。**「一度言ったら二度と言わない」にはしない** —
// 長い沈黙こそ、この断りが防いでいるものだから。届いたら忘れる(`delivered`)ので、
// 復帰したあとの最初の失敗はすぐ言う。
pub const NOTICE_COOLDOWN_MS: u64 = 60_000;

/// 「いま言ってよいか」と「前に言ってから何本呑み込んだか」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoticeDecision {
    pub say: bool,
    pub swallowed: u32,
}

#[derive(Default)]
pub struct NoticeCooldown {
    /// チャンネル → 最後に言った時刻
    said_at: HashMap<String, u64>,
    /// チャンネル → それから呑み込んだ数(次に喋るとき報告する)
    held: HashMap<String, u32>,
}

impl NoticeCooldown {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn take(&mut self, channel_id: &str, now_ms: u64) -> NoticeDecision {
        if let Some(last) = self.said_at.get(channel_id) {
            if now_ms.saturating_sub(*last) < NOTICE_COOLDOWN_MS {
                let swallowed = self.held.entry(channel_id.to_string()).or_insert(0);
                *swallowed += 1;
                return NoticeDecision {
                    say: false,
                    swallowed: *swallowed,
                };
            }
        }
        self.said_at.insert(channel_id.to_string(), now_ms);
        NoticeDecision {
            say: true,
            swallowed: self.held.remove(channel_id).unwrap_or(0),
        }
    }

    /// 何かがこのチャンネルの担当マシンへ届いた。**苦情は終わり** — 忘れる。
    pub fn delivered(&mut self, channel_id: &str) {
        self.said_at.remove(channel_id);
        self.held.remove(channel_id);
    }
}

// ── 節5: 親自身が答えるコマンド ───────────────────────────────
// ここに居るのは「どのマシンにも訊けないこと」だけ: `route`(訊く先のマシンこそが変更対象)、
// `set-home`(フリート全体への放送)、DM の名乗り(Owner が生まれる瞬間)。
// 判断は純関数にして、実行(保存・投稿・転送)は呼ぶ側がやる。

/// 1つのコマンド判定に要る文脈。
pub struct CommandCtx<'a> {
    pub channel_id: &'a str,
    pub user_id: Option<&'a str>,
    pub text: &'a str,
    /// Owner が決まるまでは `None` — その間は誰も命令できない。
    pub owner_user_id: Option<&'a str>,
    /// Relay が自分の id を解決するまでは `None` ⇒ チャンネルのコマンドは1つも成立しない。
    pub bot_user_id: Option<&'a str>,
}

impl CommandCtx<'_> {
    fn is_dm(&self) -> bool {
        crate::bridge::command::SlackId::is_dm(self.channel_id)
    }

    fn msg(&self) -> crate::bridge::command::Message<'_> {
        crate::bridge::command::Message::new(self.text, self.bot_user_id)
    }

    /// このコマンドは bot に宛てられているか。DM は名指し不要。
    ///
    /// 宛てられていない `route` は**断りもしない** — 人の会話に混ざった1語に「それは
    /// Owner だけです」と割り込むのは、bot が入っていない会話への闖入だから。
    fn addressed(&self) -> bool {
        self.is_dm() || self.msg().mentions_bot()
    }

    /// 本文が `verb` で始まるコマンドなら、その後ろの語。宛てられていなければ `None`。
    fn verb_args(&self, verb: &str) -> Option<Vec<String>> {
        self.addressed()
            .then(|| self.msg().verb_args(verb))
            .flatten()
    }

    /// Owner の検査。**要求が well-formed かは、その人が要求してよいかの後**。
    /// 書き手の分からないメッセージ(bot)も断る。
    fn refuse_if_not_owner(&self, command: &str) -> Option<String> {
        match (self.user_id, self.owner_user_id) {
            (Some(u), Some(o)) if u == o => None,
            _ => Some(crate::t!(
                "Only the owner can use `{command}`.",
                "`{command}` を使えるのは Owner だけです。"
            )),
        }
    }

/// `route` — Owner がこのチャンネルの担当マシンを決める。
///
/// **Owner だけ**。これが無いと、共有チャンネルに居る他人が `route 自分のマシン` と打つだけで
/// そのチャンネルを乗っ取れ、以後のメッセージがその人のマシン(その人の権限)へ流れる。
/// 飾りの検査ではない。
    /// `route` — Owner がこのチャンネルの担当マシンを決める。
    /// `self_id` は親の名前 — **担当の決まっていないチャンネルは親が受ける**ので、一覧で言う。
    pub fn route(&self, routes: &Routes, connected: &[String], self_id: &str) -> RouteOutcome {
        let ctx = self;
        let Some(args) = ctx.verb_args("route") else {
            return RouteOutcome::NotACommand;
        };
        if args.len() > 1 {
            // 文の中に紛れた `route` は文であってコマンドではない
            return RouteOutcome::NotACommand;
        }
        if let Some(reply) = ctx.refuse_if_not_owner("route")
        {
            return RouteOutcome::Refused(reply);
        }

        let Some(bridge_id) = args.first() else {
            return RouteOutcome::List(route_table(ctx.channel_id, routes, connected, self_id));
        };

        // **いま繋がっているマシンにしか向けられない。** 打ち間違いも、まだ起動していないマシンも
        // 扱いは同じ — 基準は「今ここに居るか」だけ。行き先の無い担当を黙って作らない。
        if !connected.iter().any(|c| c == bridge_id) {
            let here = if connected.is_empty() {
                crate::t!("none", "なし")
            } else {
                connected.join(", ")
            };
            return RouteOutcome::UnknownBridge(crate::t!(
                "No machine named *{bridge_id}* is connected. A channel can only be handed to a \
                 machine that's online — check the name, or start agentgw on that machine.\n\
                 Online now: {here}",
                "*{bridge_id}* という名前のマシンはつながっていません。チャンネルを任せられるのは\
                 オンラインのマシンだけです。名前を確かめるか、そのマシンで agentgw を起動してください。\n\
                 オンラインのマシン: {here}"
            ));
        }

        let mut lines = vec![crate::t!(
            "This channel is now handled by *{bridge_id}*.",
            "このチャンネルは *{bridge_id}* が受け持つようになりました。"
        )];
        if let Some(before) = routes.get(ctx.channel_id).filter(|b| *b != bridge_id) {
            // 正直な部分: 新しいマシンは Slack のスレッドを読み直せるが、前のマシンが**何をしたか**
            // (どのファイルを読み、何を試し、何を書き換えたか)は知りようがない。そう言っておくから、
            // 切り替えは他のことについて黙っていられる。
            lines.push(crate::t!(
                "(It was *{before}* before. Running threads continue on *{bridge_id}*, which reads \
                 the thread to catch up — but *it can't see what {before} actually did*.)",
                "(前は *{before}* でした。進行中のスレッドは *{bridge_id}* で続きます。スレッドは読み直して\
                 流れを把握しますが、*{before} が実際に行った作業の中身は分かりません*。)"
            ));
        }
        RouteOutcome::Set {
            bridge_id: bridge_id.clone(),
            reply: lines.join("\n"),
        }
    }

    /// `set-home` — 打ったチャンネルがフリート共通の home になる。引数の形は持たない。
    pub fn set_home(&self) -> SetHomeOutcome {
        let ctx = self;
        match ctx.verb_args("set-home") {
            Some(args) if args.is_empty() => {}
            _ => return SetHomeOutcome::NotACommand,
        }
        if let Some(reply) = ctx.refuse_if_not_owner("set-home") {
            return SetHomeOutcome::Refused(reply);
        }
        if ctx.is_dm() {
            return SetHomeOutcome::NeedsChannel(crate::t!(
                "Run `set-home` in the *channel* you want notices in. A DM can't be the notice channel.",
                "`set-home` は、通知を出したい *チャンネル* で実行してください。DM は通知先にできません。"
            ));
        }
        let ch = ctx.channel_id;
        SetHomeOutcome::Set(crate::t!(
            "Notices from every machine will now go to <#{ch}>.",
            "これからは、すべてのマシンの通知を <#{ch}> に出します。"
        ))
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum RouteOutcome {
    NotACommand,
    Refused(String),
    List(String),
    /// 担当が決まった。`reply` を返し、`bridge_id` をこのチャンネルに紐づける。
    Set {
        bridge_id: String,
        reply: String,
    },
    UnknownBridge(String),
}

/// 引数なしの `route` の答え。**打ったチャンネルがどうなっているか**を先に言い、
/// ほかのチャンネルとマシンの様子を後に並べる。どれにもオンラインかどうかを付ける。
///
/// 担当の決まっていないチャンネルは親が受ける — 一覧に「担当なし」とだけ書くと、
/// そこに書いたメッセージがどこへ行くのか読み手に分からない。
pub fn route_table(here: &str, routes: &Routes, connected: &[String], self_id: &str) -> String {
    let online = |id: &str| connected.iter().any(|c| c == id);
    let mark = |id: &str| {
        if online(id) {
            "🟢".to_string()
        } else {
            crate::t!("🔴 offline", "🔴 オフライン")
        }
    };
    // 1つの行き先を一言で(担当が無ければゲートウェイ)
    let dest = |id: Option<&String>| match id {
        Some(id) => format!("*{id}* {}", mark(id)),
        None => {
            let m = mark(self_id);
            crate::t!(
                "not assigned — the gateway *{self_id}* handles it {m}",
                "未設定(ゲートウェイの *{self_id}* が受け持ちます){m}"
            )
        }
    };

    let this = dest(routes.get(here));
    let mut out = vec![crate::t!(
        "*This channel (<#{here}>)*: {this}",
        "*このチャンネル(<#{here}>)*: {this}"
    )];

    let others: Vec<String> = routes
        .iter()
        .filter(|(ch, _)| ch.as_str() != here)
        .map(|(ch, id)| format!("• <#{ch}> → {}", dest(Some(id))))
        .collect();
    out.push(String::new());
    out.push(crate::t!("*Other channels*", "*ほかのチャンネル*"));
    if others.is_empty() {
        out.push(crate::t!(
            "(none assigned — the gateway handles every channel)",
            "(割り当てたチャンネルはありません。どのチャンネルもゲートウェイが受け持ちます)"
        ));
    } else {
        out.extend(others);
    }

    // マシン: 繋がっているもの + route にだけ名前がある(= 今は居ない)もの
    let mut machines: Vec<String> = connected.to_vec();
    for id in routes.values() {
        if !machines.contains(id) {
            machines.push(id.clone());
        }
    }
    if !machines.iter().any(|m| m == self_id) {
        machines.push(self_id.to_string());
    }
    machines.sort();
    let line = machines
        .iter()
        .map(|m| {
            let dot = if online(m) { "🟢" } else { "🔴" };
            let gateway = if m == self_id {
                crate::t!(" (gateway)", "(ゲートウェイ)")
            } else {
                String::new()
            };
            format!("{dot} {m}{gateway}")
        })
        .collect::<Vec<_>>()
        .join(" · ");
    out.push(String::new());
    out.push(crate::t!("*Machines*: {line}", "*マシン*: {line}"));
    out.join("\n")
}

#[derive(Debug, PartialEq, Eq)]
pub enum SetHomeOutcome {
    NotACommand,
    Refused(String),
    /// DM で打たれた。home は**チャンネル**でなければならない。
    NeedsChannel(String),
    Set(String),
}

// ── DM の名乗り — Owner が生まれる瞬間 ───────────────────────────
// 新しい Relay には Owner が居ない。立てた本人は、**接続文字列を DM する**ことでそれを証明する
// (あの秘密は Relay の運用者しか持っていない)。そのあと、接続中 0台なら結び先が無く、
// 1台なら自動、複数なら名前を訊く。これは `route` ではない — Owner が生まれる瞬間で、
// 続けて送る `Linked` が「あなたの Owner はこの人です」を Bridge に教える。

pub struct DmOnboardingCtx<'a> {
    pub text: &'a str,
    /// 書き手(bot / 匿名なら `None` — その人は Owner になれない)。
    pub user_id: Option<&'a str>,
    /// Relay 自身の api トークン。DM された接続文字列がこれを含むことが所有の証明。
    pub api_token: &'a str,
    pub current_owner: Option<&'a str>,
    pub connected: &'a [String],
    /// Owner が名乗り終えて、マシンの名前を待っている最中か。
    pub awaiting_selection: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum DmOnboarding {
    NotOnboarding,
    /// 接続文字列に見えたが、この bot のものではない(違うトークン / 千切れた貼り付け)。
    BadToken(String),
    /// 正しいトークンだが Owner は既に居る。**名乗り直させない。秘密も転送しない。**
    AlreadyConfigured(String),
    ClaimedAuto {
        owner_user_id: String,
        bridge_id: String,
        reply: String,
    },
    ClaimedPending {
        owner_user_id: String,
        reply: String,
    },
    ClaimedNoMachine {
        owner_user_id: String,
        reply: String,
    },
    Selected {
        bridge_id: String,
        reply: String,
    },
    SelectRetry(String),
}

impl DmOnboardingCtx<'_> {
    /// DM に貼られた接続文字列で Owner が決まる瞬間。
    pub fn decide(&self) -> DmOnboarding {
        let ctx = self;
        let list = if ctx.connected.is_empty() {
            crate::t!("none", "なし")
        } else {
            ctx.connected.join(", ")
        };

        // ① 名前を待っている最中: それを終わらせられるのは Owner の返信だけ。
        if ctx.awaiting_selection
            && ctx.current_owner.is_some()
            && ctx.user_id.is_some()
            && ctx.user_id == ctx.current_owner
        {
            let name = ctx.text.trim();
            if ctx.connected.iter().any(|c| c == name) {
                return DmOnboarding::Selected {
                    bridge_id: name.to_string(),
                    reply: crate::t!(
                        "*{name}* will handle this DM.",
                        "この DM は *{name}* が受け持ちます。"
                    ),
                };
            }
            return DmOnboarding::SelectRetry(crate::t!(
                "There's no online machine named *{name}*. Reply with just a machine name.\nOnline now: {list}",
                "*{name}* という名前のオンラインのマシンはありません。マシンの名前だけを返信してください。\nオンラインのマシン: {list}"
            ));
        }

        let text = ctx.text.trim();
        let token = if !text.starts_with("SCLINK1-") {
            None
        } else {
            Some(
                wire::decode_connection(text).is_ok_and(|c| secret_eq(&c.api_token, ctx.api_token)),
            )
        };

        // ② Owner が既に居る: トークンの DM は名乗り直しにならず、秘密も先へ渡さない。
        if ctx.current_owner.is_some() {
            return match token {
                Some(true) => DmOnboarding::AlreadyConfigured(crate::t!(
                    "This bot already has an owner.",
                    "このボットには既に Owner がいます。"
                )),
                _ => DmOnboarding::NotOnboarding,
            };
        }

        // ③ まだ Owner が居ない。
        match token {
            None => DmOnboarding::NotOnboarding, // 設定前の普通の DM
            Some(false) => DmOnboarding::BadToken(crate::t!(
                "That connection string doesn't match this bot — it belongs to another bot, or the paste was cut off.",
                "接続文字列がこのボットのものと一致しません。別のボットのものか、貼り付けが途中で切れています。"
            )),
            Some(true) => {
                let Some(owner) = ctx.user_id else {
                    return DmOnboarding::NotOnboarding;
                };
                match ctx.connected.len() {
                    0 => DmOnboarding::ClaimedNoMachine {
                        owner_user_id: owner.to_string(),
                        reply: crate::t!(
                            "You're now the owner. No machine is connected yet — add one with `agentgw add-machine`.",
                            "あなたが Owner になりました。まだつながっているマシンがありません。`agentgw add-machine` でマシンを加えてください。"
                        ),
                    },
                    1 => DmOnboarding::ClaimedAuto {
                        owner_user_id: owner.to_string(),
                        bridge_id: ctx.connected[0].clone(),
                        reply: {
                            let only = &ctx.connected[0];
                            crate::t!(
                                "You're now the owner. *{only}*, the only machine online, will handle this DM.",
                                "あなたが Owner になりました。この DM は、ただ1台オンラインの *{only}* が受け持ちます。"
                            )
                        },
                    },
                    _ => DmOnboarding::ClaimedPending {
                        owner_user_id: owner.to_string(),
                        reply: crate::t!(
                            "You're now the owner. Which machine should handle this DM? Reply with just its name.\nOnline now: {list}",
                            "あなたが Owner になりました。この DM をどのマシンに任せますか? マシンの名前だけを返信してください。\nオンラインのマシン: {list}"
                        ),
                    },
                }
            }
        }
    }
}

// ── 節6: presence(フリートの出入りを home に出す) ──────────────────────

/// 切断を握っておく猶予。Wi-Fi が瞬いたマシンは1〜2秒で戻ってくるので、その間に戻ったら
/// 何も言わない — Owner から見れば、そのマシンは居なくならなかった。
pub const PRESENCE_GRACE_MS: u64 = 5_000;

#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
struct Seen {
    /// Owner がいま「繋がっている」と思っているか。
    up: bool,
    /// 猶予の満了時刻(切断を握っている最中だけ `Some`)。
    down_due_ms: Option<u64>,
}

/// join / drop を home の1行に変える。時計は呼ぶ側から渡す(タイマーを持たない)。
#[derive(Default)]
pub struct Presence {
    seen: HashMap<String, Seen>,
    grace_ms: u64,
}

impl Presence {
    pub fn new() -> Self {
        Self {
            seen: HashMap::new(),
            grace_ms: PRESENCE_GRACE_MS,
        }
    }

    pub fn with_grace(grace_ms: u64) -> Self {
        Self {
            seen: HashMap::new(),
            grace_ms,
        }
    }

    /// 繋がった。**何も言わない** — 「繋がった」を人に知らせるのは子自身の `online`
    /// (版・pid・warm pool 付き)で、2か所で同じことを言わないため。ここは
    /// 「落ちていたのが戻った」の状態管理だけをする。
    pub fn on_connect(&mut self, bridge_id: &str) {
        let e = self.seen.entry(bridge_id.to_string()).or_default();
        // 瞬き(落ちて猶予の内に戻った)も、初めての接続も、扱いは同じ —
        // 握っていた「切断」を捨てて、繋がっていることにする
        e.down_due_ms = None;
        e.up = true;
    }

    /// 切れた。**すぐには言わない** — 猶予を置いて [`Presence::due`] が拾う。
    pub fn on_disconnect(&mut self, bridge_id: &str, now_ms: u64) {
        if let Some(e) = self.seen.get_mut(bridge_id) {
            if e.up && e.down_due_ms.is_none() {
                e.down_due_ms = Some(now_ms + self.grace_ms);
            }
        }
    }

    /// 猶予が切れた切断を回収する。定期的に呼ぶ。
    pub fn due(&mut self, now_ms: u64) -> Vec<String> {
        let mut out = Vec::new();
        for (id, e) in self.seen.iter_mut() {
            if e.down_due_ms.is_some_and(|due| now_ms >= due) {
                e.down_due_ms = None;
                e.up = false;
                out.push(crate::t!("🔴 Lost the connection to *{id}*", "🔴 *{id}* との接続が切れました"));
            }
        }
        out.sort();
        out
    }
}

/// `status` のフリート欄が出すもの。ディスクと env から起こす(I/O は呼ぶ側)。
pub struct FleetView {
    /// `host:port`。子を迎える口。
    pub listen: String,
    pub owner: Option<String>,
    pub home: Option<String>,
    pub routes: Routes,
}

/// 親が張っている ssh トンネル1本の様子。**親は自分で張っているので知っている**
/// ([`keep_tunnel`] が状態の変わり目で書く)。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Tunnel {
    /// ssh 先(`me@laptop` など)
    pub target: String,
    /// 張れていないときの理由。張れていれば `None`
    pub error: Option<String>,
}

/// 子の名前 → その子へのトンネル。載っていない子は直結で来ている。
pub type Tunnels = HashMap<String, Tunnel>;

/// 子1台の経路を一言で。
pub fn route_of(id: &str, tunnels: &Tunnels) -> String {
    match tunnels.get(id) {
        None => crate::t!("direct", "直結"),
        Some(Tunnel {
            target,
            error: None,
        }) => crate::t!("ssh tunnel ({target})", "ssh トンネル({target})"),
        Some(Tunnel {
            target,
            error: Some(why),
        }) => crate::t!(
            "ssh tunnel ({target}) — down: {why}",
            "ssh トンネル({target})— つながっていません: {why}"
        ),
    }
}

/// `status` のフリート欄の平文表示。**純関数** — I/O は呼ぶ側。
///
/// `connected` が `None` = 走っている Bridge が答えなかった(= 動いていない)。その場合でも
/// ディスクの route 表は出す — 「動いていない」と「設定が無い」を混ぜない。
pub fn format_fleet(
    f: &FleetView,
    connected: Option<&[String]>,
    tunnels: &Tunnels,
    names: &HashMap<String, String>,
) -> String {
    let label = |id: Option<&str>| -> String {
        match id {
            None => crate::t!("(not set)", "(未設定)"),
            Some(id) => match names.get(id) {
                Some(n) => format!("{n} ({id})"),
                None => id.to_string(),
            },
        }
    };
    // 見出しは**何のことか**を書く。生の `owner:` `home:` は、それが人なのか
    // チャンネルなのか、何に効くのかを読み手に一言も言っていなかった
    // **分かったのは「この口が答えたか」だけ。** 生死を名乗ると、launchd が running と
    // 言っている隣で「動いていません」と出て食い違う(再起動の直後は口が開く前のことが多い)
    let listen = &f.listen;
    let answer = if connected.is_some() {
        crate::t!("answering", "応答あり")
    } else {
        crate::t!("not answering", "応答なし")
    };
    let owner = label(f.owner.as_deref());
    let home = label(f.home.as_deref());
    let mut out = vec![
        crate::t!("● This gateway", "● このゲートウェイ"),
        crate::t!(
            "  Accepts machines on: {listen}   ({answer})",
            "  マシンを受け付ける場所: {listen}   ({answer})"
        ),
        crate::t!("  Owner:               {owner}", "  Owner:                  {owner}"),
        crate::t!("  Notices go to:       {home}", "  通知の宛先:             {home}"),
        String::new(),
    ];
    if let Some(conn) = connected {
        let n = conn.len();
        out.push(if conn.is_empty() {
            crate::t!("● Machines connected — none", "● つながっているマシン — なし")
        } else {
            crate::t!("● Machines connected — {n}", "● つながっているマシン — {n}")
        });
        out.extend(
            conn.iter()
                .map(|id| format!("  ● {id} — {}", route_of(id, tunnels))),
        );
        out.push(String::new());
    }
    let n = f.routes.len();
    if f.routes.is_empty() {
        out.push(crate::t!("● Channels assigned (route) — none", "● チャンネルの割り当て(route)— なし"));
    } else {
        out.push(crate::t!("● Channels assigned (route) — {n}", "● チャンネルの割り当て(route)— {n}"));
        for (ch, id) in &f.routes {
            let mark = match connected {
                None => String::new(),
                Some(c) if c.iter().any(|x| x == id) => crate::t!("  ● online", "  ● オンライン"),
                Some(_) => crate::t!("  ○ offline", "  ○ オフライン"),
            };
            out.push(format!("  {} → {id}{mark}", label(Some(ch))));
        }
    }
    out.push(String::new());
    out.join("\n")
}

// ── 節7: 親の口 ───────────────────────────────────────
// ここから下は I/O。上の節(1〜6)が決めたことを配線して回すだけで、判断は1つも持たない。

use crate::bridge::link as link_watch;
use crate::bridge::inbound::InboundMsg;
use crate::bridge::state::{Access, StateDir, now_ms};
use crate::chat::slack::{FleetEvent, PermClick};
use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::mpsc::Sender;

/// 子を持つ Bridge(= 親)が持って回る一式。
///
/// **owner / home / 担当表はディスク(access.json)が正。** Bridge 本体も同じファイルを
/// 持っているので、こちらが書いたら `reload` を1つ送って読み直させる — メモリを2つ持って
/// 食い違わせない。
pub struct Fleet {
    pub links: LinkServer,
    /// 子が提示する鍵。
    pub token: String,
    /// このマシンの名前。`route <自分の id>` の指名先。
    pub self_id: String,
    /// 子へ渡す Slack の bot トークン(`Ready` フレームで配る)。
    pub bot_token: String,
    pub api: crate::chat::ChatRef,
    pub dir: StateDir,
    pub cooldown: AsyncMutex<NoticeCooldown>,
    pub presence: AsyncMutex<Presence>,
    /// DM の名乗りが「マシンの名前待ち」で止まっている場所。メモリだけ。
    pub pending_selection: AsyncMutex<Option<(String, String)>>,
    /// 自分の Slack user id(`@mention` の判定)。auth.test の後に入る。
    pub bot_user_id: AsyncMutex<Option<String>>,
    /// ローカル配達の口。**直結モードと同じ2本**に流す。
    pub msg_tx: Sender<InboundMsg>,
    pub click_tx: Sender<PermClick>,
    /// access.json を書いたことを Bridge 本体に伝える口(SIGHUP と同じ再読込)。
    pub reload: Sender<()>,
    /// 自分が張っている ssh トンネル(`status` に経路を出すため)。
    pub tunnels: std::sync::Mutex<Tunnels>,
}

impl Fleet {
    fn access(&self) -> Access {
        Access::load(&self.dir)
    }

    /// access.json を書き換え、Bridge 本体に読み直させる。
    async fn edit_access(&self, f: impl FnOnce(&mut Access)) {
        let mut access = self.access();
        f(&mut access);
        if let Err(e) = access.save(&self.dir) {
            rlog("error", &format!("could not save access.json: {e}"));
            return;
        }
        let _ = self.reload.send(()).await;
    }

    fn owner(&self) -> Option<String> {
        let owner = self.access().owner;
        (!owner.is_empty()).then_some(owner)
    }

    fn home(&self) -> Option<String> {
        self.access().home_channel
    }

    /// 自分と、いま繋がっている子。**自分も担当になれる**ので一覧に居る。
    fn machines(&self) -> Vec<String> {
        let mut all = vec![self.self_id.clone()];
        all.extend(self.links.connected());
        all.sort();
        all.dedup();
        all
    }

    /// link が1本つながった。**接続簿に載せ、初参加なら home に知らせ、`Ready` を渡す。**
    ///
    /// 子から dial されたときも、こちらから迎えに行ったときも**同じ記帳** — 違うのは
    /// ソケットの型と、そこから先の送受の書き方だけ。
    async fn attach(
        &self,
        bridge_id: &str,
    ) -> (Conn, tokio::sync::mpsc::UnboundedReceiver<String>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let conn = Conn::new(tx);
        let (joined, displaced) = self.links.register(bridge_id, conn.clone());
        // 押しのけた古い link の書き手を終わらせる。登録は既に新しい方に差し替わっている
        drop(displaced);
        if joined == Joined::New {
            self.presence.lock().await.on_connect(bridge_id);
        }
        // 受理の1本目 — bot トークンと、いまの home
        let _ = conn.send(&wire::LinkFrame::Ready {
            bot_token: self.bot_token.clone(),
            home: self.home(),
        });
        (conn, rx)
    }

    /// link が1本切れた。**押しのけられた古い link では presence を鳴らさない**
    /// (`unregister` が「まだ自分が登録されているか」で見分ける)。
    async fn detach(&self, bridge_id: &str, conn: &Conn) {
        if self.links.unregister(bridge_id, conn) {
            self.presence
                .lock()
                .await
                .on_disconnect(bridge_id, now_ms());
        }
    }

    /// home に1行。home が未設定なら**投稿せずログに残す**(黙って捨てない)。
    async fn post_home(&self, text: &str) {
        let Some(home) = self.home() else {
            rlog(
                "info",
                &format!("home notice not posted (no home channel set yet): {text}"),
            );
            return;
        };
        self.post(&home, None, text).await;
    }

    /// 親が Slack に書く用事: 届けられないと言うこと、自分のコマンドに答えること、
    /// フリートの出入りを知らせること。**bot が実際にやることは各マシンが書く。**
    async fn post(&self, channel: &str, thread_ts: Option<&str>, text: &str) {
        if let Err(e) = self
            .api
            .post_message_no_unfurl(channel, text, thread_ts)
            .await
        {
            rlog("error", &format!("could not post to {channel}: {e}"));
        }
    }

// ── Slack から来たものを、どこへ渡すか ───────────────────────────

    /// presence の猶予切れを拾う番人。タイマーを持たない設計なので、ここが唯一の時計。
    pub async fn watch_presence(self: Arc<Self>) {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            tick.tick().await;
            let lines = self.presence.lock().await.due(now_ms());
            for line in lines {
                self.post_home(&line).await;
            }
        }
    }

    /// Slack の生イベントを1つ引き取る。**親だけがここを通る。**
    pub async fn on_fleet_event(self: &Arc<Self>, item: FleetEvent) {
        match item {
            FleetEvent::Event { name, event } => self.on_event(&name, &event).await,
            FleetEvent::Action { action, body } => self.on_click(action, body).await,
        }
    }

    async fn on_event(self: &Arc<Self>, name: &str, raw: &serde_json::Value) {
        let ev = Event::new(name, raw);
        let channel = ev.channel();
        let bot_user_id = self.bot_user_id.lock().await.clone();

        // 親自身が答えるもの(コマンドと名乗り)を**配達の判断より先に**。答えたらそこで終わり —
        // `route` を転送してしまうと、行き先を変える指示が古い行き先へ飛ぶ
        if ev.is_a_command_candidate(self.owner().as_deref())
            && let Some(ch) = channel
            && self
                .answer_own_commands(&ev, ch, bot_user_id.as_deref())
                .await
        {
            return;
        }

        let (routes, connected) = (self.access().bridges(), self.links.connected());
        match Delivery::decide(channel, &routes, &self.self_id, |b| {
            connected.iter().any(|c| c == b)
        }) {
            // このマシンの担当。**直結モードと同じ変換**を通して同じ口に流す
            Delivery::Local => {
                rlog("debug", &format!("{name} chan={channel:?} → local"));
                if let Some(ch) = channel {
                    self.cooldown.lock().await.delivered(ch);
                }
                if let Some(msg) = crate::chat::slack::inbound_from_relay(name, raw) {
                    let _ = self.msg_tx.send(msg).await;
                }
            }
            Delivery::Forward(bridge_id) => {
                let frame = wire::LinkFrame::Event {
                    name: name.to_string(),
                    event: raw.clone(),
                };
                if self.links.send_to(&bridge_id, &frame) {
                    rlog("debug", &format!("{name} chan={channel:?} → {bridge_id}"));
                    if let Some(ch) = channel {
                        self.cooldown.lock().await.delivered(ch);
                    }
                    return;
                }
                // 表を引いてから投げるまでの間に落ちた。**行ったふりをしない。**
                rlog(
                    "info",
                    &format!(
                        "{name} chan={channel:?} → {bridge_id} FAILED (the link just dropped)"
                    ),
                );
                self.cannot_deliver(
                    &ev,
                    &Delivery::offline_notice(&bridge_id, &connected),
                    "send failed mid-flight",
                )
                .await;
            }
            Delivery::Offline(bridge_id) => {
                rlog(
                    "info",
                    &format!("{name} chan={channel:?} — its machine \"{bridge_id}\" is offline"),
                );
                self.cannot_deliver(
                    &ev,
                    &Delivery::offline_notice(&bridge_id, &connected),
                    "machine offline",
                )
                .await;
            }
            Delivery::UnknownChannel => rlog(
                "info",
                &format!("{name} — could not tell which channel this happened in; dropped"),
            ),
        }
    }

    /// 「届けられませんでした」の一本道。**言ってよい相手にだけ、1分に1回。**
    async fn cannot_deliver(self: &Arc<Self>, ev: &Event<'_>, text: &str, why: &str) {
        let bot_user_id = self.bot_user_id.lock().await.clone();
        let Some(ch) = ev.channel() else { return };
        let name = ev.name;
        if !ev.may_answer(bot_user_id.as_deref()) {
            if ev.is_retraction() {
                rlog(
                    "info",
                    &format!(
                        "{name} chan={ch} — a deletion could not be delivered ({why}); dropped in silence"
                    ),
                );
            } else if !ev.from_a_bot() {
                rlog(
                    "debug",
                    &format!("{name} chan={ch} — {why}, but this was not addressed to the bot"),
                );
            }
            return;
        }
        let notice = self.cooldown.lock().await.take(ch, now_ms());
        if !notice.say {
            rlog(
                "info",
                &format!(
                    "{why} chan={ch} — notice held back: already said within {}s ({} held back since)",
                    NOTICE_COOLDOWN_MS / 1000,
                    notice.swallowed
                ),
            );
            return;
        }
        if notice.swallowed > 0 {
            rlog(
                "info",
                &format!(
                    "{why} chan={ch} — {} notice(s) were held back since the last one",
                    notice.swallowed
                ),
            );
        }
        self.post(ch, ev.thread(), text).await;
        rlog("info", &format!("{why} chan={ch} — told them so"));
    }

    /// 親が答えたら `true`(= 配達しない)。
    async fn answer_own_commands(
        self: &Arc<Self>,
        ev: &Event<'_>,
        channel: &str,
        bot_user_id: Option<&str>,
    ) -> bool {
        let user_id = ev.user();
        let text = ev.text().unwrap_or("");
        let thread = ev.thread().unwrap_or("").to_string();
        let owner = self.owner();
        let machines = self.machines();

        // ── DM の名乗り。`route` より先(route は Owner が既に居ることを前提にする)
        if crate::bridge::command::SlackId::is_dm(channel) {
            let awaiting = self.pending_selection.lock().await.is_some();
            let outcome = DmOnboardingCtx {
                text,
                user_id,
                api_token: &self.token,
                current_owner: owner.as_deref(),
                connected: &machines,
                awaiting_selection: awaiting,
            }
            .decide();
            let reply = match &outcome {
                DmOnboarding::NotOnboarding => None,
                DmOnboarding::BadToken(r)
                | DmOnboarding::AlreadyConfigured(r)
                | DmOnboarding::SelectRetry(r) => Some(r.clone()),
                DmOnboarding::ClaimedAuto {
                    owner_user_id,
                    bridge_id,
                    reply,
                } => {
                    let owner_user_id = owner_user_id.clone();
                    self.edit_access(move |a| a.owner = owner_user_id).await;
                    self.bind_and_link(bridge_id, channel, &thread).await;
                    *self.pending_selection.lock().await = None;
                    Some(reply.clone())
                }
                DmOnboarding::ClaimedPending {
                    owner_user_id,
                    reply,
                }
                | DmOnboarding::ClaimedNoMachine {
                    owner_user_id,
                    reply,
                } => {
                    let owner_user_id = owner_user_id.clone();
                    self.edit_access(move |a| a.owner = owner_user_id).await;
                    *self.pending_selection.lock().await =
                        Some((channel.to_string(), thread.clone()));
                    Some(reply.clone())
                }
                DmOnboarding::Selected { bridge_id, reply } => {
                    let pending = self.pending_selection.lock().await.take();
                    if let Some((ch, ts)) = pending {
                        self.bind_and_link(bridge_id, &ch, &ts).await;
                    }
                    Some(reply.clone())
                }
            };
            if let Some(reply) = reply {
                self.post(channel, Some(&thread), &reply).await;
                return true;
            }
        }

        let ctx = CommandCtx {
            channel_id: channel,
            user_id,
            text,
            owner_user_id: owner.as_deref(),
            bot_user_id,
        };

        // ── route — ここで答え、**決して配達しない**(行き先こそが変更対象)
        let routes = self.access().bridges();
        match ctx.route(&routes, &machines, &self.self_id) {
            RouteOutcome::NotACommand => {}
            RouteOutcome::Set { bridge_id, reply } => {
                self.bind_and_link(&bridge_id, channel, &thread).await;
                self.post(channel, Some(&thread), &reply).await;
                return true;
            }
            RouteOutcome::Refused(reply) => {
                rlog(
                    "info",
                    &format!("route REFUSED chan={channel} by={user_id:?} — not the Owner"),
                );
                self.post(channel, Some(&thread), &reply).await;
                return true;
            }
            RouteOutcome::List(reply) | RouteOutcome::UnknownBridge(reply) => {
                self.post(channel, Some(&thread), &reply).await;
                return true;
            }
        }

        // ── set-home — ここで保存し、**繋がっている全マシンへ同じイベントを配る**
        //    (各自のゲートを通す)
        match ctx.set_home() {
            SetHomeOutcome::NotACommand => {}
            SetHomeOutcome::Set(reply) => {
                let home = channel.to_string();
                self.edit_access(move |a| a.home_channel = Some(home)).await;
                let frame = wire::LinkFrame::Event {
                    name: "message".to_string(),
                    event: ev.raw.clone(),
                };
                let delivered = self
                    .links
                    .connected()
                    .iter()
                    .filter(|id| self.links.send_to(id, &frame))
                    .count();
                rlog(
                    "info",
                    &format!(
                        "set-home chan={channel} — saved and broadcast to {delivered} machine(s)"
                    ),
                );
                self.post(channel, Some(&thread), &reply).await;
                return true;
            }
            SetHomeOutcome::Refused(reply) | SetHomeOutcome::NeedsChannel(reply) => {
                self.post(channel, Some(&thread), &reply).await;
                return true;
            }
        }
        false
    }

    /// ある場所の担当を決め、そのマシンに「あなたが担当です」を渡す。
    /// **自分自身を担当にしたときは何も送らない** — 自分に電話はかけない。
    async fn bind_and_link(self: &Arc<Self>, bridge_id: &str, channel: &str, thread_ts: &str) {
        let (ch, id) = (channel.to_string(), bridge_id.to_string());
        self.edit_access(move |a| a.set_bridge(&ch, &id)).await;
        if bridge_id == self.self_id {
            rlog(
                "info",
                &format!("bound {channel} → {bridge_id} (this machine)"),
            );
            return;
        }
        let Some(owner) = self.owner() else {
            rlog(
                "error",
                &format!("cannot send linked to {bridge_id} — no Owner set"),
            );
            return;
        };
        let sent = self.links.send_to(
            bridge_id,
            &wire::LinkFrame::Linked {
                owner_user_id: owner,
                channel: channel.to_string(),
                thread_ts: thread_ts.to_string(),
            },
        );
        rlog(
            "info",
            &format!(
                "bound {channel} → {bridge_id}{}",
                if sent {
                    " and sent linked"
                } else {
                    " but linked FAILED (the link just dropped)"
                }
            ),
        );
    }

    async fn on_click(self: &Arc<Self>, action: serde_json::Value, body: serde_json::Value) {
        let channel = Click(&body).channel().map(str::to_string);
        let (routes, connected) = (self.access().bridges(), self.links.connected());

        match Delivery::decide(channel.as_deref(), &routes, &self.self_id, |b| {
            connected.iter().any(|c| c == b)
        }) {
            Delivery::Local => {
                rlog("debug", &format!("action chan={channel:?} → local"));
                if let Some(click) = crate::chat::slack::perm_click_from_relay(&action, &body) {
                    let _ = self.click_tx.send(click).await;
                }
            }
            Delivery::Forward(bridge_id) => {
                let ok = self
                    .links
                    .send_to(&bridge_id, &wire::LinkFrame::Action { action, body });
                rlog(
                    if ok { "debug" } else { "info" },
                    &format!(
                        "action chan={channel:?} → {bridge_id}{}",
                        if ok {
                            ""
                        } else {
                            " FAILED (link just dropped)"
                        }
                    ),
                );
                if ok && let Some(ch) = channel.as_deref() {
                    self.cooldown.lock().await.delivered(ch);
                }
            }
            // 押した人はそれが何かすることを見ている。**呑み込むのがいちばん悪い。**
            other => {
                let Some(ch) = channel.as_deref() else { return };
                let Delivery::Offline(id) = &other else {
                    return;
                };
                let text = Delivery::offline_notice(id, &connected);
                rlog("info", &format!("action chan={ch} — {other:?}"));
                let notice = self.cooldown.lock().await.take(ch, now_ms());
                if notice.say {
                    self.post(ch, None, &text).await;
                }
            }
        }
    }

// ── 親→子 dial(親が NAT の内側にいるときだけ) ────────────────────────
//
// **既定は子から dial。** ここに来るのは、親が家のルータの内側などに居て、子から繋ぎに
// 行けない構成だけ。フレームの向きは変わらない(`Ready` / `Event` / `Action` / `Linked` は
// 常に親→子)ので、変わるのは**どちらが電話をかけるか**だけ。

/// 子1台へ繋ぎ続ける。**戻ってこない。**
///
/// 繋がったら [`LinkServer`] に登録するので、配達も presence も set-home の一斉配りも
/// 子から dial された link と1行も変わらない扱いになる。
    /// `AGENTGW_CHILD_URLS` に書かれた子へ、1台につき1本ずつ繋ぎに行く。
    pub fn dial_children(self: &Arc<Self>, targets: Vec<(String, String)>) {
        for (bridge_id, url) in targets {
            if bridge_id == self.self_id {
                rlog(
                    "error",
                    &format!("AGENTGW_CHILD_URLS lists this machine's own name ({bridge_id}) — skipping it"),
                );
                continue;
            }
            tokio::spawn(self.clone().dial_child(bridge_id, url));
        }
    }

    /// 子1台へ繋ぎ続ける。**戻ってこない。**
    ///
    /// 繋がったら [`LinkServer`] に登録するので、配達も presence も set-home の一斉配りも
    /// 子から dial された link と1行も変わらない扱いになる。
    async fn dial_child(self: Arc<Self>, bridge_id: String, url: String) {
        let mut backoff = crate::bridge::link::RECONNECT_MIN_MS;
        loop {
            match self.dial_child_once(&bridge_id, &url).await {
                // 握手が通った回。次の再接続は短い待ちから始めてよい
                Ok(true) => backoff = crate::bridge::link::RECONNECT_MIN_MS,
                Ok(false) => {}
                Err(why) => {
                    // 話し合いでは解決しない断り。**ループで埋めない** — 1回、大きな声で
                    rlog(
                        "error",
                        &format!("dial {bridge_id}: {why} — not retrying"),
                    );
                    return;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(backoff)).await;
            backoff = (backoff * 2).min(crate::bridge::link::RECONNECT_MAX_MS);
        }
    }

    /// 1回の接続。`Ok(true)` = 握手まで通った(その後切れた)。`Err` = 設定を直すまで無駄。
    async fn dial_child_once(&self, bridge_id: &str, url: &str) -> Result<bool, &'static str> {
        use futures_util::SinkExt;
        // **鍵は1本。** どちらから dial しても同じ `AGENTGW_LINK_TOKEN` を見せる
        let target = format!("{url}{}", wire::path_for(&self.self_id));
        let request = match crate::bridge::link::build_request(&target, &self.token) {
            Ok(r) => r,
            Err(e) => {
                rlog("error", &format!("dial {bridge_id}: {e}"));
                return Err("unusable URL");
            }
        };
        let (mut socket, _) = match tokio_tungstenite::connect_async(request).await {
            Ok(ok) => ok,
            Err(e) => {
                if let tokio_tungstenite::tungstenite::Error::Http(resp) = &e {
                    match resp.status().as_u16() {
                        401 => return Err("wrong key (AGENTGW_LINK_TOKEN must match)"),
                        426 => return Err("incompatible versions (upgrade both machines)"),
                        _ => {}
                    }
                }
                rlog("info", &format!("dial {bridge_id}: {e}"));
                return Ok(false);
            }
        };
        rlog("info", &format!("dial {bridge_id}: linked ({url})"));

        let (conn, mut rx) = self.attach(bridge_id).await;

        // 迎えに行った先も黙って消える(相手の VM がサスペンドすれば FIN は来ない)。
        // 叩いて確かめないと、死んだ子を掴んだまま配達を捨て続ける
        let mut watch = link_watch::IdleWatch::default();
        loop {
            use tokio_tungstenite::tungstenite::protocol::Message as M;
            tokio::select! {
                outgoing = rx.recv() => match outgoing {
                    Some(text) => {
                        watch.on_traffic();
                        if socket.send(M::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                },
                // **受信は `beat` 経由だけ。** 直に `next()` を待つと half-open で永久に止まる
                incoming = link_watch::beat(&mut socket, &mut watch) => match incoming {
                    link_watch::Beat::Text(_) | link_watch::Beat::Alive => {} // 送ってくるものは無いはず
                    link_watch::Beat::Ping => {
                        if socket.send(M::Ping(Default::default())).await.is_err() {
                            break;
                        }
                    }
                    link_watch::Beat::Gone(why) => {
                        rlog("info", &format!("dial {bridge_id}: link closed ({why})"));
                        break;
                    }
                },
            }
        }
        self.detach(bridge_id, &conn).await;
        rlog("info", &format!("dial {bridge_id}: link closed"));
        Ok(true)
    }
}

// ── HTTP handlers ──────────────────────────────────────

/// upgrade を通してよいか。**通すなら名乗り**、通さない/到達確認なら**返す応答**。
///
/// 親(子を迎える口)と子(親を迎える口)で、判断も断り方も同じ — 違うのは通ったあとだけ。
pub(super) fn admit_upgrade(
    headers: &HeaderMap,
    uri: &axum::http::Uri,
    token: &str,
    who: &str,
    ws: WebSocketUpgrade,
) -> Result<(String, WebSocketUpgrade), axum::response::Response> {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    match Admit::of(
        uri.path(),
        header("authorization"),
        header("sec-websocket-protocol"),
        token,
    ) {
        Admit::Ok(id) => Ok((id, ws)),
        // 到達確認 — upgrade は通すが、接続簿には載せない
        Admit::Probe => {
            rlog("debug", "answered a reachability probe — not recorded");
            Err(ws
                .protocols([LINK_SUBPROTOCOL])
                .on_upgrade(|_socket| async {}))
        }
        decision => {
            // **黙って断らない。** 何が駄目だったのかを、こちらのログにも1行残す
            rlog(
                "info",
                &format!("refused {who} on {}: {}", uri.path(), decision.why()),
            );
            let code = decision.status().unwrap_or(400);
            Err((
                StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_REQUEST),
                decision.why(),
            )
                .into_response())
        }
    }
}

/// 子が dial してくる口。`/bridge/{id}`。
async fn on_upgrade(
    State(fleet): State<Arc<Fleet>>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    ws: WebSocketUpgrade,
) -> axum::response::Response {
    let (bridge_id, ws) = match admit_upgrade(&headers, &uri, &fleet.token, "a link", ws) {
        Ok(ok) => ok,
        Err(response) => return response,
    };
    // 自分と同じ名前は通さない。通すと `route <自分の id>` の行き先が2つになる
    if bridge_id == fleet.self_id {
        rlog(
            "info",
            &format!("refused a link named \"{bridge_id}\" — that is this machine's own name"),
        );
        return (StatusCode::CONFLICT, "that name is taken by the parent").into_response();
    }
    // upgrade の応答に subprotocol を返すのが作法(返さないと厳しいクライアントは切る)
    ws.protocols([LINK_SUBPROTOCOL])
        .on_upgrade(move |socket| on_socket(fleet, bridge_id, socket))
}

/// axum 側の読み口。差を埋めるだけ — 見張りの時計と状態機械は `link::beat` に1つしか無い。
impl link_watch::LinkRead for WebSocket {
    async fn read_frame(&mut self) -> link_watch::Frame {
        match self.recv().await {
            Some(Ok(Message::Text(t))) => link_watch::Frame::Text(t.to_string()),
            Some(Ok(Message::Close(_))) | None => {
                link_watch::Frame::Closed("closed by the other side".to_string())
            }
            Some(Ok(_)) => link_watch::Frame::Other,
            Some(Err(e)) => link_watch::Frame::Closed(e.to_string()),
        }
    }
}

/// 1本の link の一生(**子が dial してきた側**)。
async fn on_socket(fleet: Arc<Fleet>, bridge_id: String, mut socket: WebSocket) {
    let (conn, mut rx) = fleet.attach(&bridge_id).await;

    // ソケットを分割しない(futures_util の Sink 側を使わない)。**握手のあと子は何も送って
    // こない**ので、この1本のループで「送る」と「閉じるのを待つ」を兼ねられる。
    //
    // **黙って消えた子を掴んだままにしない。** 握手のあと子は何も送ってこないので、この口は
    // 無通信が正常。だから Ping で叩かないと half-open と区別がつかず、居ない子が
    // `status` に「つながっています」と出続け、その子宛の配達が宙に消える
    let mut watch = link_watch::IdleWatch::default();
    loop {
        tokio::select! {
            outgoing = rx.recv() => match outgoing {
                Some(text) => {
                    watch.on_traffic();
                    if socket.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                None => break,
            },
            // **受信は `beat` 経由だけ。** 直に `recv()` を待つと half-open で永久に止まる
            incoming = link_watch::beat(&mut socket, &mut watch) => match incoming {
                link_watch::Beat::Text(_) | link_watch::Beat::Alive => {} // 送ってくるものは無いはず
                link_watch::Beat::Ping => {
                    if socket.send(Message::Ping(Default::default())).await.is_err() {
                        break;
                    }
                }
                link_watch::Beat::Gone(why) => {
                    rlog("info", &format!("{bridge_id}: link closed ({why})"));
                    break;
                }
            },
        }
    }
    // axum の WebSocket は drop で閉じる
    fleet.detach(&bridge_id, &conn).await;
}

/// 走っている親に「いま誰が繋がっているか」を聞く口。**生きているプロセスしか知らない。**
/// loopback にしか bind しないが、前段の proxy が全パスを転送する構成もありうるので鍵で守る。
async fn on_status(
    State(fleet): State<Arc<Fleet>>,
    headers: HeaderMap,
) -> axum::response::Response {
    let presented = headers
        .get("x-api-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !secret_eq(presented, &fleet.token) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let tunnels = fleet.tunnels.lock().unwrap().clone();
    axum::Json(serde_json::json!({ "connected": fleet.links.connected(), "tunnels": tunnels }))
        .into_response()
}

/// 子を迎える口を開ける。**戻ってこない。**
///
/// bind に失敗しても Bridge は止めない — 自分のワーカーは動き続ける。ただし子は1台も
/// 繋がらないので、error で大きく残す(黙ると「繋がらない」の原因がどこにも出ない)。
pub async fn serve_children(fleet: Arc<Fleet>, addr: std::net::SocketAddr) {
    let Some(listener) = bind_link_port(addr, "children").await else {
        return;
    };
    rlog("info", &format!("listening on {addr}/bridge/<id>"));
    serve_children_on(fleet, listener).await;
}

/// link の口を開ける。**開けなくても Bridge は止めない** — 自分のワーカーは動き続ける。
/// ただし相手は1台も繋がらないので、error で大きく残す(黙ると原因がどこにも出ない)。
pub(super) async fn bind_link_port(addr: std::net::SocketAddr, what: &str) -> Option<tokio::net::TcpListener> {
    match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => Some(l),
        Err(e) => {
            rlog(
                "error",
                &format!("cannot listen on {addr} for the {what} ({e}) — nothing can connect"),
            );
            None
        }
    }
}

/// **この口に載っているのは link の3本だけ。** hook intake(`bridge.rs`)と MCP は
/// **別の口**で、あちらは loopback + トークンで守られている。こちらは前段の proxy 経由で
/// 公開されるので、同じ口に載せると hook と MCP が外から叩けるようになる。
async fn serve_children_on(fleet: Arc<Fleet>, listener: tokio::net::TcpListener) {
    let app = Router::new()
        .route("/status", get(on_status))
        .route(wire::PROBE_PATH, get(on_upgrade))
        .route("/bridge/{id}", get(on_upgrade))
        .with_state(fleet);
    if let Err(e) = axum::serve(listener, app).await {
        rlog("error", &format!("the link server stopped: {e}"));
    }
}

/// トンネルを使うときの、**マシンの loopback 側**のポート。
pub const TUNNEL_PORT: u16 = 8799;

/// ゲートウェイが張り続ける ssh の引数。**-N でコマンドは流さない。**
///
/// `ExitOnForwardFailure=yes` が要る — 無いと転送に失敗しても ssh だけ生き残り、
/// 「繋がっているのに届かない」状態になる。
pub fn tunnel_ssh_args(target: &str, remote_port: u16, parent_addr: &str) -> Vec<String> {
    [
        "-N",
        "-o",
        "BatchMode=yes",
        "-o",
        "ExitOnForwardFailure=yes",
        "-o",
        "ServerAliveInterval=30",
        "-o",
        "ServerAliveCountMax=3",
        "-R",
        &format!("127.0.0.1:{remote_port}:{parent_addr}"),
        target,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// ゲートウェイの agentgw の中で、マシン1台分の ssh トンネルを張り続ける。
///
/// **別サービスにしない。** agentgw が動いている間だけ見張ればよいので、子プロセス(OS の意味)として持つ。
///
/// **ssh の多重化(`ControlMaster auto` + `ControlPersist`)はそのまま使う。** そのときの ssh は
/// 既にある親玉に転送を預けて、すぐ**終了 0** で抜ける(2026-09-18 実機)。これは失敗ではない —
/// 転送は親玉の中で生きている。なので 0 で抜けたら間を空けて頼み直すだけにする(親玉が
/// 居なくなっていれば、次の ssh が新しい親玉になって転送を持つ)。多重化を使っていない
/// 設定なら ssh は前に居続け、`kill_on_drop` で agentgw と一緒に消える。
pub async fn keep_tunnel(
    fleet: Arc<Fleet>,
    child: String,
    target: String,
    parent_addr: String,
) {
    use crate::bridge::state::LogCtx;
    let args = tunnel_ssh_args(&target, TUNNEL_PORT, &parent_addr);
    // ログは状態が変わったときだけ(1分ごとの頼み直しで plugin-debug.log を埋めない)
    let mut was_ok: Option<bool> = None;
    loop {
        let out = tokio::process::Command::new("ssh")
            .args(&args)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output()
            .await;
        let (ok, why) = match out {
            Ok(o) if o.status.success() => (true, String::new()),
            // 抜けた理由を残す。黙って張り直し続けると、鍵が無いのか相手が居ないのか分からない
            Ok(o) => (
                false,
                format!(
                    "exited ({}) {}",
                    o.status,
                    String::from_utf8_lossy(&o.stderr).trim()
                ),
            ),
            Err(e) => (false, format!("could not start ssh: {e}")),
        };
        if was_ok != Some(ok) {
            // `status` に経路を出すため、ゲートウェイの手元に今の様子を置く
            fleet.tunnels.lock().unwrap().insert(
                child.clone(),
                Tunnel {
                    target: target.clone(),
                    error: (!ok).then(|| why.clone()),
                },
            );
            if ok {
                LogCtx::default().info(
                    "relay",
                    &format!(
                        "tunnel {child}: up via ssh {target} (child 127.0.0.1:{TUNNEL_PORT} -> {parent_addr})"
                    ),
                );
            } else {
                LogCtx::default().error("relay", &format!("tunnel {child}: {why} — retrying"));
            }
            was_ok = Some(ok);
        }
        let wait = if ok { 60 } else { 5 };
        tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
    }
}

// ── 節9: CLI(`status` のフリート欄) ─────────────────────────────────

pub(crate) const DEFAULT_LISTEN: &str = "0.0.0.0:8787";

/// `status` のフリート欄。**人に見せる文字列はここだけ。**
pub struct Cli;

impl Cli {
    pub(crate) fn env_of(dir: &StateDir) -> HashMap<String, String> {
        dir.load_env().unwrap_or_default().into_iter().collect()
    }

    pub(crate) fn write_env(dir: &StateDir, pairs: &[(&str, String)]) -> std::io::Result<()> {
        let path = dir.path().join(".env");
        let before = std::fs::read_to_string(&path).unwrap_or_default();
        let after = crate::setup::set_env_keys(&before, pairs);
        crate::bridge::state::write_atomic_mode(&path, &after, Some(0o600))
    }

    /// 秘密を1本作る。`/dev/urandom` を16進に。
    ///
    /// **32バイトだけ読む。** `/dev/urandom` は EOF を返さないので `fs::read` は永久に読み続ける
    /// (2026-08-01 実機で確認 — Relay から持ってきたこの1行が接続文字列を固めた)。
    fn mint_token() -> String {
        let mut bytes = [0u8; 32];
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            use std::io::Read;
            let _ = f.read_exact(&mut bytes);
        }
        // 万一読めなくても全ゼロを秘密にしない
        let pid = std::process::id().to_be_bytes();
        let now = now_ms().to_be_bytes();
        for (i, b) in pid.iter().chain(now.iter()).enumerate() {
            bytes[i] ^= b;
        }
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// 子に見せる鍵。**一度作ったら変えない** — 作り直すと、繋がっている全マシンが一斉に
    /// 締め出される。返り値の `bool` は「いま作った」。
    pub(crate) fn key_for_invite(existing: Option<&str>) -> (String, bool) {
        match existing {
            Some(t) if !t.trim().is_empty() => (t.trim().to_string(), false),
            _ => (Self::mint_token(), true),
        }
    }

    /// `status` の頭の1行。**役割は `.env` だけで決まる** — プロセスを見に行かないので
    /// Bridge が走っていなくても出る(走っていないときこそ読みたい)。
    /// `Wiring::resolve` が断る設定なら、その理由をそのまま出す — 起動できない Bridge の
    /// 理由をログを開かずに知れる唯一の場所になる。
    pub fn role_line(env: &HashMap<String, String>) -> String {
        use crate::bridge::link::{Mode, Wiring};
        let wiring = match Wiring::resolve(|k| env.get(k).cloned()) {
            Ok(w) => w,
            Err(why) => {
                let why = why.lines().next().unwrap_or("");
                return crate::t!("Role: can't tell — {why}", "役割: 判定できません — {why}");
            }
        };
        let name = wiring
            .self_id
            .as_deref()
            .map(|n| crate::t!(" \"{n}\"", "「{n}」"))
            .unwrap_or_default();
        match (&wiring.upstream, &wiring.children, &wiring.inlet) {
            (Mode::Direct { .. }, Some(l), _) => {
                let addr = l.addr;
                crate::t!(
                    "Role: gateway{name} — connected to Slack, accepts machines on {addr}",
                    "役割: ゲートウェイ{name} — Slack に接続、マシンを {addr} で受け付け"
                )
            }
            (Mode::Direct { .. }, None, _) => crate::t!(
                "Role: gateway{name} — connected to Slack, no other machines",
                "役割: ゲートウェイ{name} — Slack に接続、ほかのマシンなし"
            ),
            (Mode::Relay { url, .. }, ..) => crate::t!(
                "Role: machine{name} — connects to the gateway at {url}",
                "役割: マシン{name} — ゲートウェイ {url} につなぐ"
            ),
            (Mode::AwaitParent, _, Some(l)) => {
                let addr = l.addr;
                crate::t!(
                    "Role: machine{name} — waits for the gateway to connect on {addr}",
                    "役割: マシン{name} — ゲートウェイからの接続を {addr} で待つ"
                )
            }
            (Mode::AwaitParent, _, None) => crate::t!(
                "Role: machine{name} — waits for the gateway, but has no address to listen on",
                "役割: マシン{name} — ゲートウェイを待っているが、受け付ける場所が未設定"
            ),
        }
    }

    /// 開いている口。ワーカーはここに繋ぎ返してくるので、繋がらないときに最初に見る数字。
    ///
    /// **ここでは割り当てない** — 記録されている物だけ読む(status がポートを増やしたら
    /// 本末転倒だし、Bridge が握っている番号と食い違う)。
    pub fn ports_line(dir: &StateDir) -> String {
        let endpoints =
            dir.read_json_or("access.json", serde_json::Value::Null)["endpoints"].clone();
        let open: Vec<String> = [("MCP", "mcp"), ("hook", "hook")]
            .into_iter()
            .filter_map(|(label, which)| {
                endpoints[which]["port"]
                    .as_u64()
                    .map(|p| format!("{label} 127.0.0.1:{p}"))
            })
            .collect();
        if open.is_empty() {
            // 記録が無いことしか分からない — 起動直後と未起動を見分けられない
            crate::t!(
                "Local ports: none yet (agentgw just started, or isn't running)",
                "ローカルのポート: まだありません(起動した直後か、動いていません)"
            )
        } else {
            let open = open.join(" / ");
            crate::t!("Local ports: {open}", "ローカルのポート: {open}")
        }
    }

    /// `status` のフリート欄。役割の1行は**必ず出す**(設定を間違えたとき最初に見る場所が
    /// ここなので、子で黙っていると何も手掛かりが無い)。表の方は子を迎える設定が無ければ
    /// 出さない — 単独 Bridge のときに空の表を見せない。
    pub async fn print_fleet(dir: &StateDir) {
        let env = Self::env_of(dir);
        println!("\n{}", Self::role_line(&env));
        println!("{}", Self::ports_line(dir));
        let (Some(listen), Some(token)) = (
            env.get("AGENTGW_LINK_LISTEN"),
            env.get("AGENTGW_LINK_TOKEN"),
        ) else {
            return;
        };
        let access = Access::load(dir);
        let view = FleetView {
            listen: listen.clone(),
            owner: (!access.owner.is_empty()).then(|| access.owner.clone()),
            home: access.home_channel.clone(),
            routes: access.bridges(),
        };
        let reply = Self::ask_status(listen, token).await;
        let connected = reply.as_ref().map(|r| r.0.clone());
        let tunnels = reply.map(|r| r.1).unwrap_or_default();
        let names = Self::names_of(&env, &view).await;
        println!();
        print!(
            "{}",
            format_fleet(&view, connected.as_deref(), &tunnels, &names)
        );
    }

    /// status 口を叩く。答えなければ `None`(= この口が答えなかった、それだけ)。
    ///
    /// **3回試す。** `install.sh` は再起動の直後に status を出すので、1回きりだと
    /// 「まだ口が開いていない」を「動いていない」と読み違える。
    pub(crate) async fn ask_connected(listen: &str, token: &str) -> Option<Vec<String>> {
        Self::ask_status(listen, token).await.map(|r| r.0)
    }

    /// 走っている親の `status` 口の答え — 繋がっている子と、親が張っているトンネル。
    async fn ask_status(listen: &str, token: &str) -> Option<(Vec<String>, Tunnels)> {
        for i in 0..3 {
            if i > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(700)).await;
            }
            if let Some(v) = Self::ask_status_once(listen, token).await {
                return Some(v);
            }
        }
        None
    }

    async fn ask_status_once(listen: &str, token: &str) -> Option<(Vec<String>, Tunnels)> {
        // 依存を増やさないために curl に聞く(この1回だけの用事に HTTP クライアントを宣言しない)
        let out = tokio::process::Command::new("curl")
            .args([
                "-s",
                "--max-time",
                "3",
                "-H",
                &format!("x-api-token: {token}"),
                &format!("http://{listen}/status"),
            ])
            .output()
            .await
            .ok()?;
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
        let connected = v
            .get("connected")?
            .as_array()?
            .iter()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect();
        // 古い親は tunnels を返さない — そのときは「全部直結」として読む
        let tunnels = v
            .get("tunnels")
            .and_then(|t| serde_json::from_value(t.clone()).ok())
            .unwrap_or_default();
        Some((connected, tunnels))
    }

    /// id → 読める名前(best-effort)。引けなかったものは生の id のまま出す。
    async fn names_of(env: &HashMap<String, String>, view: &FleetView) -> HashMap<String, String> {
        let mut names = HashMap::new();
        let Some(bot) = env.get("SLACK_BOT_TOKEN") else {
            return names;
        };
        let Ok(api) = crate::chat::slack::Api::new(bot) else {
            return names;
        };
        // home も route と同じチャンネル。owner だけが人なので users.info の側で引く
        for id in view.routes.keys().chain(view.home.iter()) {
            if let Some(n) = api.channel_display_name(id).await {
                names.insert(id.clone(), n);
            }
        }
        if let Some(owner) = &view.owner
            && let Some(n) = api.user_display_name(owner).await
        {
            names.insert(owner.clone(), n);
        }
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "the-shared-secret";

    /// `status` の頭の1行。**役割を取り違えたまま黙るのが一番困る**ので、4つの形と
    /// 「決められない」を固定する。
    #[test]
    fn role_line_names_the_role() {
        let env = |pairs: &[(&str, &str)]| -> HashMap<String, String> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        let line = |pairs: &[(&str, &str)]| Cli::role_line(&env(pairs));

        let solo = line(&[("SLACK_APP_TOKEN", "xapp-1"), ("SLACK_BOT_TOKEN", "xoxb-1")]);
        assert!(solo.starts_with("Role: gateway — connected to Slack, no other machines"), "{solo}");

        let parent = line(&[
            ("SLACK_APP_TOKEN", "xapp-1"),
            ("SLACK_BOT_TOKEN", "xoxb-1"),
            ("AGENTGW_BRIDGE_ID", "mac"),
            ("AGENTGW_LINK_LISTEN", "127.0.0.1:8787"),
            ("AGENTGW_LINK_TOKEN", "k"),
        ]);
        assert!(parent.contains("Role: gateway \"mac\""), "{parent}");
        assert!(parent.contains("127.0.0.1:8787"), "{parent}");

        let dialing = line(&[
            ("AGENTGW_RELAY_URL", "wss://p.example"),
            ("AGENTGW_RELAY_TOKEN", "k"),
            ("AGENTGW_BRIDGE_ID", "laptop"),
        ]);
        assert!(dialing.contains("Role: machine \"laptop\""), "{dialing}");
        assert!(dialing.contains("wss://p.example"), "{dialing}");

        let awaiting = line(&[
            ("AGENTGW_LINK_LISTEN", "127.0.0.1:8788"),
            ("AGENTGW_LINK_TOKEN", "k"),
            ("AGENTGW_BRIDGE_ID", "laptop"),
        ]);
        assert!(awaiting.contains("waits for the gateway"), "{awaiting}");

        // 起動できない設定こそ status で理由が要る(ログを開かずに分かるように)
        let broken = line(&[]);
        assert!(broken.starts_with("Role: can't tell"), "{broken}");
    }

    fn ok_admit(path: &str) -> Admit {
        Admit::of(
            path,
            Some(&format!("Bearer {TOKEN}")),
            Some(LINK_SUBPROTOCOL),
            TOKEN,
        )
    }

    #[test]
    fn a_correct_upgrade_is_admitted_under_its_name() {
        assert_eq!(ok_admit("/bridge/desktop"), Admit::Ok("desktop".into()));
        assert_eq!(ok_admit("/bridge/desktop").status(), None);
    }

    #[test]
    fn a_wrong_or_missing_token_is_401() {
        for auth in [
            None,
            Some("Bearer "),
            Some("Bearer wrong"),
            Some(TOKEN),                      // Bearer が無い
            Some("bearer the-shared-secret"), // 綴りは仕様どおり区別する
            Some("Basic dXNlcjpwYXNz"),
        ] {
            let got = Admit::of("/bridge/desktop", auth, Some(LINK_SUBPROTOCOL), TOKEN);
            assert_eq!(got, Admit::Unauthorized, "{auth:?}");
            assert_eq!(got.status(), Some(401));
        }
    }

    #[test]
    fn a_different_protocol_is_426() {
        for sub in [
            None,
            Some("sclink.0"),
            Some("sclink.2"),
            Some(""),
            Some("chat"),
        ] {
            let got = Admit::of(
                "/bridge/desktop",
                Some(&format!("Bearer {TOKEN}")),
                sub,
                TOKEN,
            );
            assert_eq!(got, Admit::WrongVersion, "{sub:?}");
            assert_eq!(got.status(), Some(426));
        }
    }

    /// ブラウザ流儀のカンマ区切りでも、1つ一致すれば通す。
    #[test]
    fn a_list_of_subprotocols_is_accepted_when_one_matches() {
        let got = Admit::of(
            "/bridge/desktop",
            Some(&format!("Bearer {TOKEN}")),
            Some("something-else, sclink.1"),
            TOKEN,
        );
        assert_eq!(got, Admit::Ok("desktop".into()));
    }

    /// 到達確認は**通るが名乗らない**。認証は他と同じく通す必要がある。
    #[test]
    fn the_probe_path_is_admitted_without_a_name() {
        assert_eq!(ok_admit(wire::PROBE_PATH), Admit::Probe);
        assert_eq!(ok_admit(wire::PROBE_PATH).status(), None);
        // 認証は素通しではない
        assert_eq!(
            Admit::of(
                wire::PROBE_PATH,
                Some("Bearer wrong"),
                Some(LINK_SUBPROTOCOL),
                TOKEN
            ),
            Admit::Unauthorized
        );
    }

    #[test]
    fn a_path_without_a_usable_name_is_400() {
        for path in ["/", "/bridge/", "/bridge/a/b", "/status", "/bridge/../x"] {
            let got = ok_admit(path);
            assert_eq!(got, Admit::BadPath, "{path}");
            assert_eq!(got.status(), Some(400));
        }
    }

    /// **検査の順序が仕様。** 版が違えば、トークンを見る前に断る — 版の食い違いを
    /// 「トークンが違う」と報告すると、直しようのない調査に人を送り込む。
    #[test]
    fn the_version_is_checked_before_the_token() {
        assert_eq!(
            Admit::of(
                "/bridge/desktop",
                Some("Bearer wrong"),
                Some("sclink.0"),
                TOKEN
            ),
            Admit::WrongVersion
        );
    }

    /// 名乗りを信じるのは**トークンが通ったあと**。通っていない相手のパスは読みもしない。
    #[test]
    fn the_name_is_read_only_after_the_token_passes() {
        assert_eq!(
            Admit::of(
                "/bridge/",
                Some("Bearer wrong"),
                Some(LINK_SUBPROTOCOL),
                TOKEN
            ),
            Admit::Unauthorized
        );
    }

    #[test]
    fn secret_compare_is_length_safe() {
        assert!(secret_eq("abc", "abc"));
        assert!(!secret_eq("abc", "abd"));
        assert!(!secret_eq("abc", "abcd"));
        assert!(!secret_eq("", "a"));
        assert!(secret_eq("", ""));
    }

    // ── 接続簿 ──────────────────────────────────────────────────────────────

    fn conn() -> (Conn, tokio::sync::mpsc::UnboundedReceiver<String>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (Conn::new(tx), rx)
    }

    fn ready() -> wire::LinkFrame {
        wire::LinkFrame::Ready {
            bot_token: "xoxb-1".into(),
            home: None,
        }
    }

    #[test]
    fn a_new_machine_joins_and_shows_up() {
        let s = LinkServer::new();
        let (c, _rx) = conn();
        assert_eq!(s.register("desktop", c).0, Joined::New);
        assert_eq!(s.connected(), vec!["desktop".to_string()]);
        assert!(s.is_connected("desktop"));
    }

    /// **回帰**: 同じ名前で繋ぎ直したら新しい方が残る。ここが壊れると、繋ぎ直すたびに
    /// マシンが行方不明になる。
    #[test]
    fn a_reconnect_keeps_the_newest_link() {
        let s = LinkServer::new();
        let (old, mut old_rx) = conn();
        let (new, mut new_rx) = conn();
        assert_eq!(s.register("desktop", old.clone()).0, Joined::New);

        let (joined, displaced) = s.register("desktop", new.clone());
        assert_eq!(joined, Joined::Reconnected); // 新規 join ではない = presence に出さない
        assert!(displaced.unwrap().is(&old));

        // 押しのけられた古い link が**後から**閉じても、新しい登録は消えない
        assert!(!s.unregister("desktop", &old));
        assert_eq!(s.connected(), vec!["desktop".to_string()]);

        // 届く先は新しい方だけ
        assert!(s.send_to("desktop", &ready()));
        assert!(new_rx.try_recv().is_ok());
        assert!(old_rx.try_recv().is_err());
    }

    #[test]
    fn a_real_close_is_reported_once() {
        let s = LinkServer::new();
        let (c, _rx) = conn();
        s.register("desktop", c.clone());
        assert!(s.unregister("desktop", &c)); // 本当に居なくなった
        assert!(!s.unregister("desktop", &c)); // 2度目は何も起きない
        assert!(s.connected().is_empty());
        assert!(!s.is_connected("desktop"));
    }

    #[test]
    fn sending_to_a_machine_that_is_not_here_fails_rather_than_pretending() {
        let s = LinkServer::new();
        assert!(!s.send_to("nobody", &ready()));

        // 受け手が落ちた link も「届いた」とは言わない
        let (c, rx) = conn();
        s.register("desktop", c);
        drop(rx);
        assert!(!s.send_to("desktop", &ready()));
    }

    #[test]
    fn several_machines_are_listed_in_a_stable_order() {
        let s = LinkServer::new();
        let keep: Vec<_> = ["laptop", "desktop", "vps"]
            .iter()
            .map(|id| {
                let (c, rx) = conn();
                s.register(id, c);
                rx
            })
            .collect();
        assert_eq!(s.connected(), ["desktop", "laptop", "vps"]);
        drop(keep);
    }

    // ── 配達の判断 ──────────────────────────────────────────────────────────

    fn routes(pairs: &[(&str, &str)]) -> Routes {
        pairs
            .iter()
            .map(|(c, b)| (c.to_string(), b.to_string()))
            .collect()
    }

    #[test]
    fn the_channel_is_read_from_each_kind_of_event() {
        assert_eq!(
            Event::new("message", &serde_json::json!({"channel": "C1"})).channel(),
            Some("C1")
        );
        assert_eq!(
            Event::new(
                "member_joined_channel",
                &serde_json::json!({"channel": "C2"})
            )
            .channel(),
            Some("C2")
        );
        for name in ["reaction_added", "reaction_removed"] {
            assert_eq!(
                Event::new(name, &serde_json::json!({"item": {"channel": "C3"}})).channel(),
                Some("C3")
            );
        }
        // 知らない種類・形が違うものは「読めなかった」
        assert_eq!(
            Event::new("app_mention", &serde_json::json!({"channel": "C1"})).channel(),
            None
        );
        assert_eq!(
            Event::new("message", &serde_json::json!({"channel": 7})).channel(),
            None
        );
        assert_eq!(
            Event::new("reaction_added", &serde_json::json!({})).channel(),
            None
        );
    }

    #[test]
    fn a_click_carries_its_own_channel() {
        assert_eq!(
            Click(&serde_json::json!({"channel": {"id": "C9"}})).channel(),
            Some("C9")
        );
        assert_eq!(Click(&serde_json::json!({})).channel(), None);
    }

    #[test]
    fn delivery_has_four_outcomes() {
        let r = routes(&[("C1", "desktop"), ("C2", "laptop"), ("C3", "vps")]);
        let up = |id: &str| id == "desktop";
        let d = |ch: Option<&str>| Delivery::decide(ch, &r, "vps", up);
        assert_eq!(d(Some("C1")), Delivery::Forward("desktop".into()));
        assert_eq!(d(Some("C2")), Delivery::Offline("laptop".into()));
        // 自分を名指しした行(`route <自分の id>`)は自分でやる
        assert_eq!(d(Some("C3")), Delivery::Local);
        // **未 route はローカル**。ここが単独 Bridge の既定
        assert_eq!(d(Some("C_NEW")), Delivery::Local);
        assert_eq!(d(None), Delivery::UnknownChannel);
    }

    /// 子を1台も持たない Bridge は表が空 — **すべてローカル**。回帰の本丸。
    #[test]
    fn a_bridge_with_no_children_keeps_everything() {
        let r = Routes::new();
        for ch in ["C1", "D9", "C_WHATEVER"] {
            assert_eq!(
                Delivery::decide(Some(ch), &r, "me", |_| false),
                Delivery::Local
            );
        }
    }

    // ── 親の口・鍵・順序 ──────────────────────────

    fn a_fleet() -> (Arc<Fleet>, tokio::sync::mpsc::Receiver<InboundMsg>) {
        let (msg_tx, msg_rx) = tokio::sync::mpsc::channel(4);
        let (click_tx, _click_rx) = tokio::sync::mpsc::channel(4);
        let (reload, _reload_rx) = tokio::sync::mpsc::channel(4);
        let dir = StateDir::at(
            std::env::temp_dir().join(format!("slack-relay-test-{}", std::process::id())),
        );
        let fleet = Arc::new(Fleet {
            links: LinkServer::new(),
            token: "s3cret".to_string(),
            self_id: "parent".to_string(),
            bot_token: "xoxb-test".to_string(),
            api: Arc::new(crate::chat::fake::FakeChat::default()),
            dir,
            cooldown: Default::default(),
            presence: Default::default(),
            pending_selection: Default::default(),
            bot_user_id: Default::default(),
            msg_tx,
            click_tx,
            reload,
            tunnels: Default::default(),
        });
        (fleet, msg_rx)
    }

    /// 生の HTTP を1本投げて、返ってきたステータス行だけ読む。
    /// (この1回の用事のために HTTP クライアントを宣言しない — 依存は7つのまま)
    async fn status_line(addr: std::net::SocketAddr, path: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(
            format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await
        .unwrap();
        let mut buf = Vec::new();
        let _ = s.read_to_end(&mut buf).await;
        String::from_utf8_lossy(&buf)
            .lines()
            .next()
            .unwrap_or_default()
            .to_string()
    }

    /// **子を迎える口に載っているのは link の3本だけ。**
    /// hook intake と MCP は別の口(loopback + トークン)で、こちらは前段の proxy 経由で
    /// **公開される** — 同じ口に載せた日に、hook と MCP が外から叩けるようになる。
    #[tokio::test]
    async fn the_children_port_carries_the_link_and_nothing_else() {
        let (fleet, _rx) = a_fleet();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve_children_on(fleet, listener));

        // Bridge の他の口はここに居ない
        for path in ["/hook", "/mcp", "/"] {
            assert!(
                status_line(addr, path).await.contains("404"),
                "{path} がこの口に居る"
            );
        }
        // 居るのは status(鍵で守られている)と link の2本
        assert!(status_line(addr, "/status").await.contains("401"));
        assert!(!status_line(addr, "/bridge/desktop").await.contains("404"));
    }

    /// **答えるものは配達より先に見る。** `route` を転送してしまうと、行き先を変える指示が
    /// 古い行き先へ飛ぶ(答えた後に `return` するのは呼び出し側の構造)。
    #[test]
    fn only_a_persons_channel_message_is_a_command_candidate() {
        let owner = Some("U_OWNER");
        let msg = serde_json::json!({"channel": "C1", "user": "U_OWNER", "text": "route"});
        assert!(Event::new("message", &msg).is_a_command_candidate(owner));
        // チャンネルが読めなければ答えようがない
        let nowhere = serde_json::json!({"user": "U_OWNER", "text": "route"});
        assert!(!Event::new("message", &nowhere).is_a_command_candidate(owner));
        // リアクションや join はコマンドになりえない
        for name in [
            "reaction_added",
            "reaction_removed",
            "member_joined_channel",
        ] {
            assert!(
                !Event::new(name, &msg).is_a_command_candidate(owner),
                "{name}"
            );
        }
        // 自分の投稿は見ない(Owner の Web API 投稿だけが例外 — speaks_as_a_bot)
        let from_bot = serde_json::json!({"channel": "C1", "user": "U_BOT", "bot_id": "B1"});
        assert!(!Event::new("message", &from_bot).is_a_command_candidate(owner));
    }

    /// **鍵は一度作ったら変えない。** 作り直すと、繋がっている全マシンが一斉に締め出される。
    #[test]
    fn the_key_is_minted_once_and_then_kept() {
        let (fresh, minted) = Cli::key_for_invite(None);
        assert!(minted);
        assert_eq!(fresh.len(), 64, "32バイトを16進で: {fresh}");
        assert!(fresh.chars().all(|c| c.is_ascii_hexdigit()));
        // 空・空白だけは「無い」と同じ
        assert!(Cli::key_for_invite(Some("   ")).1);
        // 既にあるものは**そのまま**返る
        assert_eq!(
            Cli::key_for_invite(Some(" kept ")),
            ("kept".to_string(), false)
        );
        // 作るたびに違う(全ゼロの秘密を作らない)
        assert_ne!(Cli::mint_token(), Cli::mint_token());
    }

    /// 同じ規則が**コマンドの入口にも**要る — `Access::gate` は Owner の
    /// Web API 投稿を人として通すのに、こちらが bot として弾いていたら `route` が打てない。
    #[test]
    fn only_the_owners_web_api_post_counts_as_a_person() {
        let bot_post = |user: &str| serde_json::json!({"user": user, "bot_id": "B1"});
        let owner = Some("U_OWNER".to_string());
        // Owner 本人 = 人として扱う
        assert!(!Event::new("message", &bot_post("U_OWNER")).speaks_as_a_bot(owner.as_deref()));
        // それ以外の bot(自分の投稿を含む)は落とす
        assert!(Event::new("message", &bot_post("U_BOT")).speaks_as_a_bot(owner.as_deref()));
        let no_user = serde_json::json!({"bot_id": "B1"});
        assert!(Event::new("message", &no_user).speaks_as_a_bot(owner.as_deref()));
        // Owner が未設定なら全部 bot(fail-closed)
        assert!(Event::new("message", &bot_post("U_OWNER")).speaks_as_a_bot(None));
        // 人の投稿はそのまま人
        let a_person = serde_json::json!({"user": "U_ANY"});
        assert!(!Event::new("message", &a_person).speaks_as_a_bot(owner.as_deref()));
    }

    #[test]
    fn the_notices_name_the_machines_that_are_here() {
        let some = vec!["desktop".to_string(), "vps".to_string()];
        assert!(
            Delivery::offline_notice("laptop", &some).contains("*laptop*, the machine for this channel, is offline")
        );
        assert!(Delivery::offline_notice("laptop", &some).contains("wasn't kept"));
        assert!(Delivery::offline_notice("laptop", &some).contains("desktop, vps"));
    }

    // ── 断りの上限 ──────────────────────────────────────────────────────────

    #[test]
    fn a_notice_is_said_once_a_minute_and_counts_what_it_swallowed() {
        let mut c = NoticeCooldown::new();
        assert_eq!(
            c.take("C1", 0),
            NoticeDecision {
                say: true,
                swallowed: 0
            }
        );
        assert_eq!(
            c.take("C1", 1_000),
            NoticeDecision {
                say: false,
                swallowed: 1
            }
        );
        assert_eq!(
            c.take("C1", 2_000),
            NoticeDecision {
                say: false,
                swallowed: 2
            }
        );
        // 1分の境目
        assert_eq!(
            c.take("C1", 59_999),
            NoticeDecision {
                say: false,
                swallowed: 3
            }
        );
        assert_eq!(
            c.take("C1", 60_000),
            NoticeDecision {
                say: true,
                swallowed: 3
            }
        );
        // 報告したら数え直し
        assert_eq!(
            c.take("C1", 120_000),
            NoticeDecision {
                say: true,
                swallowed: 0
            }
        );
    }

    #[test]
    fn each_channel_has_its_own_cooldown() {
        let mut c = NoticeCooldown::new();
        assert!(c.take("C1", 0).say);
        assert!(c.take("C2", 0).say); // 別のチャンネルは巻き添えにならない
        assert!(!c.take("C1", 100).say);
    }

    /// 届いたら苦情は終わり — 復帰後の最初の失敗はすぐ言う。
    #[test]
    fn a_delivery_resets_the_complaint() {
        let mut c = NoticeCooldown::new();
        assert!(c.take("C1", 0).say);
        assert!(!c.take("C1", 100).say);
        c.delivered("C1");
        assert_eq!(
            c.take("C1", 200),
            NoticeDecision {
                say: true,
                swallowed: 0
            }
        );
    }

    // ── 断ってよい相手か ────────────────────────────────────────────────────

    const BOT: Option<&str> = Some("U_BOT");

    fn said(text: &str) -> serde_json::Value {
        said_in("C1", text)
    }

    fn said_in(channel: &str, text: &str) -> serde_json::Value {
        serde_json::json!({ "channel": channel, "text": text, "user": "U1" })
    }

    #[test]
    fn a_dm_is_always_addressed_to_the_bot() {
        assert!(Event::new("message", &said_in("D1", "hello")).may_answer(BOT));
    }

    #[test]
    fn in_a_channel_only_a_mention_is_addressed_to_the_bot() {
        assert!(Event::new("message", &said("<@U_BOT> hi")).may_answer(BOT));
        assert!(!Event::new("message", &said("hi")).may_answer(BOT));
        // 自分の id をまだ知らない間は、チャンネルの何もこちらへの用件にならない
        assert!(!Event::new("message", &said("<@U_BOT> hi")).may_answer(None));
    }

    /// **これが無いと、担当未設定のチャンネルは自分の断りに断りを返し続ける。**
    #[test]
    fn the_bots_own_words_are_never_answered() {
        for ev in [
            serde_json::json!({"channel": "D1", "text": "…", "bot_id": "B1"}),
            serde_json::json!({"channel": "D1", "text": "…", "subtype": "bot_message"}),
        ] {
            assert!(!Event::new("message", &ev).may_answer(BOT), "{ev}");
        }
    }

    #[test]
    fn a_reaction_or_a_join_is_addressed_to_nobody() {
        for name in [
            "reaction_added",
            "reaction_removed",
            "member_joined_channel",
        ] {
            assert!(
                !Event::new(name, &said_in("D1", "<@U_BOT>")).may_answer(BOT),
                "{name}"
            );
        }
    }

    /// 取り消しは、届けられないなら黙って捨てる — 再送するものが無い。
    #[test]
    fn a_deletion_is_dropped_in_silence() {
        let ev = serde_json::json!({"channel": "D1", "subtype": "message_deleted"});
        assert!(Event::new("message", &ev).is_retraction());
        assert!(!Event::new("message", &ev).may_answer(BOT));
    }

    #[test]
    fn an_event_with_no_readable_channel_is_never_answered() {
        let nowhere = serde_json::json!({"text": "hi", "user": "U1"});
        assert!(!Event::new("message", &nowhere).may_answer(BOT));
    }

    #[test]
    fn an_edit_keeps_its_text_one_level_down() {
        let edited = serde_json::json!({"message": {"text": "<@U_BOT> revised"}});
        assert_eq!(
            Event::new("message", &edited).text(),
            Some("<@U_BOT> revised")
        );
        assert_eq!(Event::new("message", &said("plain")).text(), Some("plain"));
        let empty = serde_json::json!({});
        assert_eq!(Event::new("message", &empty).text(), None);
    }

    #[test]
    fn a_dm_channel_is_recognised_by_its_shape() {
        assert!(crate::bridge::command::SlackId::is_dm("D0123"));
        assert!(!crate::bridge::command::SlackId::is_dm("C0123"));
        assert!(!crate::bridge::command::SlackId::is_dm("G0123"));
        assert!(!crate::bridge::command::SlackId::is_dm(""));
    }

    // ── route / set-home ────────────────────────────────────────────────────

    const OWNER: &str = "U_OWNER";

    fn ctx<'a>(channel: &'a str, text: &'a str, user: Option<&'a str>) -> CommandCtx<'a> {
        CommandCtx {
            channel_id: channel,
            user_id: user,
            text,
            owner_user_id: Some(OWNER),
            bot_user_id: Some("U_BOT"),
        }
    }

    fn here() -> Vec<String> {
        vec!["desktop".to_string(), "vps".to_string()]
    }

    #[test]
    fn route_sets_this_channel_to_a_connected_machine() {
        let got = CommandCtx::route(
            &ctx("C1", "<@U_BOT> route desktop", Some(OWNER)),
            &routes(&[]),
            &here(),
            "parent",
        );
        match got {
            RouteOutcome::Set { bridge_id, reply } => {
                assert_eq!(bridge_id, "desktop");
                assert!(reply.contains("This channel is now handled by *desktop*."));
                assert!(!reply.contains("before")); // 初回は注記なし
            }
            other => panic!("{other:?}"),
        }
    }

    /// 担当を**変えた**ときは正直に言う — 話の流れは読めるが、作業の詳細は残っていない。
    #[test]
    fn changing_the_owner_of_a_channel_says_what_is_lost() {
        let got = CommandCtx::route(
            &ctx("C1", "<@U_BOT> route vps", Some(OWNER)),
            &routes(&[("C1", "desktop")]),
            &here(),
            "parent",
        );
        match got {
            RouteOutcome::Set { reply, .. } => {
                assert!(reply.contains("It was *desktop* before"));
                assert!(reply.contains("it can't see what desktop actually did"));
            }
            other => panic!("{other:?}"),
        }
    }

    /// **他人は使えない。** これが無いと共有チャンネルを乗っ取れる。
    #[test]
    fn only_the_owner_may_route() {
        for user in [Some("U_STRANGER"), None] {
            let got = CommandCtx::route(
                &ctx("C1", "<@U_BOT> route desktop", user),
                &routes(&[]),
                &here(),
                "parent",
            );
            assert!(
                matches!(got, RouteOutcome::Refused(_)),
                "{user:?} → {got:?}"
            );
        }
    }

    /// 繋がっていないマシンには向けられない — 打ち間違いも未起動も同じ扱い。
    #[test]
    fn a_route_to_a_machine_that_is_not_here_is_refused() {
        let got = CommandCtx::route(
            &ctx("C1", "<@U_BOT> route laptop", Some(OWNER)),
            &routes(&[]),
            &here(),
            "parent",
        );
        match got {
            RouteOutcome::UnknownBridge(reply) => {
                assert!(
                    reply.contains("No machine named *laptop* is connected.")
                );
                assert!(reply.contains("desktop, vps"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_bare_route_says_who_handles_this_channel_first() {
        // here() = ["desktop", "vps"](繋がっている)。laptop は route にだけ名前がある
        let got = CommandCtx::route(
            &ctx("C1", "<@U_BOT> route", Some(OWNER)),
            &routes(&[("C1", "desktop"), ("C2", "laptop")]),
            &here(),
            "vps",
        );
        let RouteOutcome::List(reply) = got else {
            panic!("{got:?}")
        };
        // 打ったチャンネルが先頭
        assert!(
            reply.starts_with("*This channel (<#C1>)*: *desktop* 🟢"),
            "{reply}"
        );
        // ほかのチャンネル。居ないマシンの担当はオフラインと言う
        assert!(
            reply.contains("• <#C2> → *laptop* 🔴 offline"),
            "{reply}"
        );
        assert!(
            !reply.contains("• <#C1>"),
            "ここは「ほか」に重ねて出さない: {reply}"
        );
        // マシン: 親の印、route にだけ居る(= 居ない)マシンも出す
        assert!(
            reply.contains("*Machines*: 🟢 desktop · 🔴 laptop · 🟢 vps (gateway)"),
            "{reply}"
        );
    }

    #[test]
    fn a_channel_without_a_route_says_the_parent_takes_it() {
        let got = CommandCtx::route(
            &ctx("C9", "<@U_BOT> route", Some(OWNER)),
            &routes(&[("C1", "desktop")]),
            &here(),
            "vps",
        );
        let RouteOutcome::List(reply) = got else {
            panic!("{got:?}")
        };
        assert!(
            reply.starts_with("*This channel (<#C9>)*: not assigned — the gateway *vps* handles it 🟢"),
            "{reply}"
        );
        assert!(reply.contains("• <#C1> → *desktop* 🟢"), "{reply}");
    }

    #[test]
    fn with_no_routes_at_all_everything_goes_to_the_parent() {
        let got = CommandCtx::route(
            &ctx("C1", "<@U_BOT> route", Some(OWNER)),
            &routes(&[]),
            &[],
            "vps",
        );
        let RouteOutcome::List(reply) = got else {
            panic!("{got:?}")
        };
        assert!(reply.contains("the gateway handles every channel"), "{reply}");
    }

    /// **名指しの無い `route` は断りもしない。** bot が入っていない会話への闖入になる。
    #[test]
    fn an_unaddressed_route_falls_through_without_a_refusal() {
        let got = CommandCtx::route(
            &ctx("C1", "route desktop", Some(OWNER)),
            &routes(&[]),
            &here(),
            "parent",
        );
        assert_eq!(got, RouteOutcome::NotACommand);
    }

    #[test]
    fn a_route_inside_a_sentence_is_a_sentence() {
        for text in [
            "<@U_BOT> please route desktop",
            "<@U_BOT> route desktop and also vps",
        ] {
            let got = CommandCtx::route(
                &ctx("C1", text, Some(OWNER)),
                &routes(&[]),
                &here(),
                "parent",
            );
            assert_eq!(got, RouteOutcome::NotACommand, "{text}");
        }
    }

    /// DM では名指しが要らない。
    #[test]
    fn in_a_dm_route_needs_no_mention() {
        let got = CommandCtx::route(
            &ctx("D1", "route desktop", Some(OWNER)),
            &routes(&[]),
            &here(),
            "parent",
        );
        assert!(matches!(got, RouteOutcome::Set { .. }), "{got:?}");
    }

    #[test]
    fn set_home_takes_the_channel_it_was_typed_in() {
        match CommandCtx::set_home(&ctx("C1", "<@U_BOT> set-home", Some(OWNER))) {
            SetHomeOutcome::Set(reply) => {
                assert!(reply.contains("Notices from every machine will now go to <#C1>."))
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn set_home_in_a_dm_has_nothing_to_set() {
        match CommandCtx::set_home(&ctx("D1", "set-home", Some(OWNER))) {
            SetHomeOutcome::NeedsChannel(reply) => {
                assert!(reply.contains("A DM can't be the notice channel."))
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn only_the_owner_may_set_home() {
        let got = CommandCtx::set_home(&ctx("C1", "<@U_BOT> set-home", Some("U_STRANGER")));
        assert!(matches!(got, SetHomeOutcome::Refused(_)), "{got:?}");
    }

    #[test]
    fn set_home_takes_no_argument() {
        for text in ["<@U_BOT> set-home C9", "<@U_BOT> sethome", "set-home"] {
            let got = CommandCtx::set_home(&ctx("C1", text, Some(OWNER)));
            assert_eq!(got, SetHomeOutcome::NotACommand, "{text}");
        }
    }

    /// 自分の id を知らないうちは、チャンネルのコマンドは1つも成立しない。
    #[test]
    fn no_channel_command_is_recognised_before_the_bot_knows_its_own_id() {
        let c = CommandCtx {
            channel_id: "C1",
            user_id: Some(OWNER),
            text: "<@U_BOT> route desktop",
            owner_user_id: Some(OWNER),
            bot_user_id: None,
        };
        assert_eq!(
            CommandCtx::route(&c, &routes(&[]), &here(), "parent"),
            RouteOutcome::NotACommand
        );
    }

    // ── DM の名乗り ─────────────────────────────────────────────────────────

    fn conn_string() -> String {
        wire::encode_connection(&wire::Invite {
            url: "wss://relay.example".into(),
            api_token: TOKEN.into(),
        })
    }

    fn dm<'a>(
        text: &'a str,
        connected: &'a [String],
        owner: Option<&'a str>,
    ) -> DmOnboardingCtx<'a> {
        DmOnboardingCtx {
            text,
            user_id: Some(OWNER),
            api_token: TOKEN,
            current_owner: owner,
            connected,
            awaiting_selection: false,
        }
    }

    #[test]
    fn one_connected_machine_is_bound_automatically() {
        let one = vec!["desktop".to_string()];
        match DmOnboardingCtx::decide(&dm(&conn_string(), &one, None)) {
            DmOnboarding::ClaimedAuto {
                owner_user_id,
                bridge_id,
                reply,
            } => {
                assert_eq!(owner_user_id, OWNER);
                assert_eq!(bridge_id, "desktop");
                assert!(reply.contains("*desktop*, the only machine online"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn several_machines_ask_which_one() {
        match DmOnboardingCtx::decide(&dm(&conn_string(), &here(), None)) {
            DmOnboarding::ClaimedPending { reply, .. } => {
                assert!(reply.contains("Reply with just its name"));
                assert!(reply.contains("desktop, vps"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn no_machine_means_there_is_nothing_to_bind_yet() {
        match DmOnboardingCtx::decide(&dm(&conn_string(), &[], None)) {
            DmOnboarding::ClaimedNoMachine { reply, .. } => {
                assert!(reply.contains("add one with `agentgw add-machine`"))
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_pending_selection_is_finished_by_naming_a_connected_machine() {
        let connected = here();
        let c = DmOnboardingCtx {
            text: "vps",
            awaiting_selection: true,
            current_owner: Some(OWNER),
            ..dm("", &connected, Some(OWNER))
        };
        match DmOnboardingCtx::decide(&c) {
            DmOnboarding::Selected { bridge_id, .. } => assert_eq!(bridge_id, "vps"),
            other => panic!("{other:?}"),
        }

        let wrong = DmOnboardingCtx {
            text: "laptop",
            ..c
        };
        assert!(matches!(
            DmOnboardingCtx::decide(&wrong),
            DmOnboarding::SelectRetry(_)
        ));
    }

    /// **Owner が居るなら名乗り直させない。** しかも秘密を先へ渡さない。
    #[test]
    fn an_existing_owner_is_not_reclaimed() {
        match DmOnboardingCtx::decide(&dm(&conn_string(), &here(), Some("U_SOMEONE"))) {
            DmOnboarding::AlreadyConfigured(reply) => {
                assert!(reply.contains("already has an owner"))
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_string_for_another_bot_is_refused() {
        let other = wire::encode_connection(&wire::Invite {
            url: "wss://relay.example".into(),
            api_token: "a-different-secret".into(),
        });
        assert!(matches!(
            DmOnboardingCtx::decide(&dm(&other, &here(), None)),
            DmOnboarding::BadToken(_)
        ));
        // 千切れた貼り付けも同じ扱い
        let cut = &conn_string()[..conn_string().len() - 6];
        assert!(matches!(
            DmOnboardingCtx::decide(&dm(cut, &here(), None)),
            DmOnboarding::BadToken(_)
        ));
    }

    #[test]
    fn an_ordinary_dm_is_not_onboarding() {
        for text in ["hello", "", "route desktop"] {
            assert_eq!(
                DmOnboardingCtx::decide(&dm(text, &here(), None)),
                DmOnboarding::NotOnboarding,
                "{text}"
            );
        }
    }

    /// 書き手の分からない DM(bot)は Owner になれない。
    #[test]
    fn an_authorless_dm_cannot_claim() {
        let (text, connected) = (conn_string(), here());
        let c = DmOnboardingCtx {
            user_id: None,
            ..dm(&text, &connected, None)
        };
        assert_eq!(DmOnboardingCtx::decide(&c), DmOnboarding::NotOnboarding);
    }

    // ── presence ────────────────────────────────────────────────────────────

    #[test]
    fn a_join_says_nothing() {
        // **「繋がった」を言うのは子自身の `online`**(版・pid・warm pool 付き)。
        // 2か所で同じことを言わない(2026-09-18、admin に3行並んだので直した)
        let mut p = Presence::new();
        p.on_connect("desktop");
        assert!(p.due(0).is_empty());
    }

    #[test]
    fn a_drop_is_announced_only_after_the_grace() {
        let mut p = Presence::new();
        p.on_connect("desktop");
        p.on_disconnect("desktop", 1_000);
        assert!(p.due(1_000).is_empty());
        assert!(p.due(5_999).is_empty());
        assert_eq!(p.due(6_000), ["🔴 Lost the connection to *desktop*"]);
        assert!(p.due(9_999).is_empty()); // 2度は言わない
    }

    /// **瞬き**: 猶予の内に戻ったら何も言わない — Owner から見れば居なくならなかった。
    #[test]
    fn a_flap_says_nothing_at_all() {
        let mut p = Presence::new();
        p.on_connect("desktop");
        p.on_disconnect("desktop", 1_000);
        p.on_connect("desktop"); // 戻ってきた
        assert!(p.due(60_000).is_empty()); // 握っていた 🔴 は消えている
    }

    #[test]
    fn a_machine_that_was_never_up_never_drops() {
        let mut p = Presence::new();
        p.on_disconnect("ghost", 0);
        assert!(p.due(60_000).is_empty());
    }

    // ── status の表示 ───────────────────────────────────────────────────────

    fn a_view() -> FleetView {
        FleetView {
            listen: "127.0.0.1:8787".to_string(),
            owner: Some("U_OWNER".to_string()),
            home: Some("C_HOME".to_string()),
            routes: routes(&[("C1", "desktop"), ("C2", "laptop")]),
        }
    }

    #[test]
    fn status_marks_which_machines_are_here() {
        let names = HashMap::from([("C1".to_string(), "#dev".to_string())]);
        let out = format_fleet(
            &a_view(),
            Some(&["desktop".to_string()]),
            &Tunnels::new(),
            &names,
        );
        assert!(out.contains("(answering)"), "{out}");
        assert!(out.contains("● Machines connected — 1"), "{out}");
        assert!(out.contains("#dev (C1) → desktop  ● online"), "{out}");
        assert!(out.contains("C2 → laptop  ○ offline"), "{out}"); // 名前が引けなければ生の id
    }

    /// **走っていない**と**設定が無い**を混ぜない — route 表はどちらでも出す。
    #[test]
    fn an_offline_bridge_still_shows_what_is_configured() {
        let out = format_fleet(&a_view(), None, &Tunnels::new(), &HashMap::new());
        assert!(out.contains("(not answering)"), "{out}");
        assert!(!out.contains("Machines connected"), "{out}");
        assert!(out.contains("● Channels assigned (route) — 2"), "{out}");
        assert!(out.contains("C1 → desktop"), "{out}");
        // 生死不明のときに印は付けない
        assert!(!out.contains("● online"), "{out}");
        assert!(!out.contains("○ offline"), "{out}");
    }

    #[test]
    fn each_child_says_how_it_reaches_the_parent() {
        // 親は自分でトンネルを張っているので、どの子がトンネルかを知っている。
        // 載っていない子は直結で来ている
        let mut tunnels = Tunnels::new();
        tunnels.insert(
            "desktop".to_string(),
            Tunnel {
                target: "me@desktop".to_string(),
                error: None,
            },
        );
        let out = format_fleet(
            &a_view(),
            Some(&["laptop".to_string(), "desktop".to_string()]),
            &tunnels,
            &HashMap::new(),
        );
        assert!(out.contains("  ● laptop — direct"), "{out}");
        assert!(out.contains("  ● desktop — ssh tunnel (me@desktop)"), "{out}");
    }

    #[test]
    fn a_broken_tunnel_says_why() {
        let mut tunnels = Tunnels::new();
        tunnels.insert(
            "desktop".to_string(),
            Tunnel {
                target: "me@desktop".to_string(),
                error: Some("Permission denied (publickey)".to_string()),
            },
        );
        assert_eq!(
            route_of("desktop", &tunnels),
            "ssh tunnel (me@desktop) — down: Permission denied (publickey)"
        );
    }

    #[test]
    fn a_fleet_with_nothing_set_says_so_rather_than_showing_blanks() {
        let view = FleetView {
            listen: "127.0.0.1:8787".to_string(),
            owner: None,
            home: None,
            routes: Routes::new(),
        };
        let out = format_fleet(&view, Some(&[]), &Tunnels::new(), &HashMap::new());
        assert!(out.contains("Owner:               (not set)"), "{out}");
        assert!(out.contains("Notices go to:       (not set)"), "{out}");
        assert!(out.contains("● Machines connected — none"), "{out}");
        assert!(out.contains("● Channels assigned (route) — none"), "{out}");
    }

    #[test]
    fn after_a_real_drop_the_next_drop_is_announced_again() {
        // 🔴 は1回の切断につき1回。戻ってきたら、また言えるようになる
        let mut p = Presence::with_grace(10);
        p.on_connect("desktop");
        p.on_disconnect("desktop", 0);
        assert_eq!(p.due(10).len(), 1);
        p.on_connect("desktop");
        p.on_disconnect("desktop", 100);
        assert_eq!(p.due(110).len(), 1, "2度目の切断も言う");
    }

    #[test]
    fn the_tunnel_forwards_the_childs_loopback_to_the_parents_listener() {
        let args = tunnel_ssh_args("me@laptop", 8799, "127.0.0.1:8787");
        assert!(
            args.contains(&"-N".to_string()),
            "コマンドは流さない: {args:?}"
        );
        assert!(
            args.contains(&"127.0.0.1:8799:127.0.0.1:8787".to_string()),
            "子の 8799 を親の listener へ: {args:?}"
        );
        assert!(
            args.contains(&"ExitOnForwardFailure=yes".to_string()),
            "転送に失敗したら黙って生き残らない: {args:?}"
        );
        assert_eq!(args.last().unwrap(), "me@laptop", "ssh 先は最後: {args:?}");
    }
}
