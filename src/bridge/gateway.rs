//! Everything about the gateway–machine link lives here — from the frame contract to who is let in,
//! who gets what arrives, and machines joining and leaving the fleet.
//!
//! Sections:
//!
//! 1. Protocol (`mod link`)     frames / connection string (`Invite`) / subprotocol — pure data
//! 2. Admission                 `Admit`
//! 3. Connection book           `Conn` / `LinkServer`
//! 4. Reading what arrives and where it goes `Event` / `Click` / `Delivery` / `NoticeCooldown`
//! 5. Command decisions         `CommandCtx` / `DmOnboardingCtx`
//! 6. presence                  `Presence` / `FleetView`
//! 7. Gateway side              `Fleet` (axum handlers, the life of one link, gateway→machine dial)
//! 8. Link watch                `IdleWatch` / `beat` — both ends read the link through it (the only I/O shared with `machine.rs`)
//! 9. CLI                       `Cli` (the fleet section of `status`)
//!
//! **Sections 1–6 are pure functions** (they know no clock, socket or agent). That's why every
//! test runs synchronously, and **it breaks the day something from 7–9 is pulled in here** — when these
//! were separate files grep could check that; now that they share a module, this ordering and how the tests run are the guard.

// ── Section 1: protocol ─────────────────────────────────────
pub mod link {
    //! The Relay ⇄ Bridge link protocol — the frames carried over WebSocket and the connection
    //! string that says where to dial. Pure data, no I/O (it's a contract **both** Relay and Bridge
    //! read, so mixing in even one line of either side's convenience would make it two-faced).
    //!
    //! **The handshake is not here.** Who gets in (api token), whether we speak the same language (version),
    //! which machine it is (its name) and whether it was accepted — **all four happen in the WebSocket upgrade**:
    //!
    //! ```text
    //! GET /bridge/desktop HTTP/1.1
    //! Upgrade: websocket
    //! Authorization: Bearer <api token>
    //! Sec-WebSocket-Protocol: sclink.1
    //! ```
    //!
    //! Pass gets 101; failure gets 401 (token) / 426 (version) / 400 (path). So there's no
    //! state machine for "the first frame must be the name", and no deadline timer to cut
    //! connections that never name themselves — **once frames flow, the peer has already authenticated**.
    //!
    //! The Bun original did the same with three frames: hello / welcome / reject.
    //! That was worth keeping for wire compatibility, but once we decided not to be compatible,
    //! it would only be rebuilding what the upgrade already does.

    use serde::{Deserialize, Serialize};

    /// The version matched during the upgrade. Bump it when a frame's shape or meaning changes.
    ///
    /// Deliberately separate from the binary's semver — Relay and Bridge are separate programs with
    /// their own release cycles, and mismatched versions are normal. Only the frames' **meaning** must match.
    /// A mismatched peer is refused at the upgrade (426). Old and new silently talking past each other is far worse.
    pub const LINK_SUBPROTOCOL: &str = "agentgw.1";

    /// The path prefix the Bridge dials. The one segment after it is the machine's name.
    const BRIDGE_PATH: &str = "/bridge/";

    /// The path `link` hits to check it can reach itself.
    ///
    /// **No reserved words in the machine namespace.** A separate path can't collide with any
    /// machine name, and doesn't get caught by the check on which characters a name may use
    /// (trying it with a reserved name `__link_probe__` got rejected by the name check — found on a real machine).
    pub const PROBE_PATH: &str = "/probe";

    /// Frames that flow **after** the handshake. All go gateway → machine except [`LinkFrame::ProjectSet`],
    /// the one answer a machine sends back (only it can see its own folders).
    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
    #[serde(tag = "t", rename_all = "snake_case")]
    pub enum LinkFrame {
        /// The first frame the Relay sends right after the 101.
        ///
        /// Hands over the Slack **bot** token (the Bridge writes to Slack itself, so it needs it). **The app
        /// token is not handed over** — it only opens Socket Mode, and a second consumer of the event stream
        /// is exactly the failure this design prevents. The Bridge keeps this bot token **in memory only**.
        ///
        /// `home` carries the current value on every handshake. That's how a machine that (re)connects later
        /// catches up even if it missed `set-home`. Missing = home not set yet, and the Bridge's current value
        /// stays as is (there is no "clear it" instruction).
        Ready {
            bot_token: String,
            #[serde(skip_serializing_if = "Option::is_none")]
            home: Option<String>,
            /// The gateway's own name, so a machine can say in `status` who it is linked to.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            gateway: Option<String>,
        },
        /// Forward one Slack event to the machine in charge of that channel.
        ///
        /// `event` is a slack-morphism type passed through `to_value`. The receiver turns it back into the
        /// same type, so it's a round trip through the same serde impl and lossless.
        Event {
            name: String,
            event: serde_json::Value,
        },
        /// Forward one button press along the same path.
        ///
        /// **The Relay has already sent** the `ack` to Slack (Slack requires whoever holds the socket to
        /// answer within 3 seconds). The Bridge doesn't ack; it only decides. It writes back through
        /// `body.response_url` — that needs no token, so the promise "writes to Slack come from the Bridge"
        /// holds.
        ///
        /// **No `action_id`** — reading `action.action_id` is enough. The original carried the same value
        /// twice only because Bolt passed them separately.
        Action {
            action: serde_json::Value,
            body: serde_json::Value,
        },
        /// Notice that the Owner put this machine in charge of a place (DM / channel).
        ///
        /// Until this arrives, a Bridge behind a Relay knows nothing of its own identity on Slack (the Relay
        /// holds it). Only three things are passed: who the Owner is, and where (channel and thread) to reply.
        /// No token — the Bridge does the writing itself with the bot token.
        Linked {
            owner_user_id: String,
            channel: String,
            thread_ts: String,
        },
        /// `pwd <machine>:<path>`: use `path` (as typed — `~` and `./` are resolved on the machine) as this
        /// channel's project folder. The gateway hands the channel over only after the machine answers
        /// with [`LinkFrame::ProjectSet`], so a folder that isn't there changes nothing.
        SetProject {
            channel: String,
            thread_ts: String,
            path: String,
        },
        /// The notice channel changed. Sent to every machine so they all write to the same place.
        Home { channel: String },
        /// `set-home`, asked from a machine — same reason as [`LinkFrame::Channels`].
        SetHome { channel: String, thread_ts: String },
        /// Where this machine's agents start when a channel has no folder of its own. Sent on connecting,
        /// so `channels` can show a real path for every channel instead of just the machine's name.
        MachineHome { path: String },
        /// Where this machine can be reached: its tailnet name and IP (empty when there is no tailnet).
        MachineHost { host: String, ip: String },
        /// `machines`, asked from a machine — same reason as [`LinkFrame::Channels`].
        Machines { channel: String, thread_ts: String },
        /// `channels`, asked from a machine. **The gateway only sees messages that mention the bot**, and a
        /// follow-up in a running thread doesn't; the machine that owns the thread asks on its behalf.
        Channels { channel: String, thread_ts: String },
        /// `pwd <machine>[:<path>]`, asked from a machine — same reason as [`LinkFrame::Channels`].
        PwdOn {
            channel: String,
            thread_ts: String,
            machine: String,
            path: Option<String>,
        },
        /// The machine's answer to `SetProject`: the absolute path it stored, or why it didn't (said to the
        /// person as is). Carries the channel and thread back, so the gateway keeps no table of what it asked.
        ProjectSet {
            channel: String,
            thread_ts: String,
            result: Result<String, String>,
        },
    }

    /// WebSocket already delimits messages, so one message = one frame.
    /// Unlike NDJSON over a UDS, there's no "tail cut off mid-way" to worry about.
    pub fn encode(f: &LinkFrame) -> String {
        serde_json::to_string(f).unwrap_or_default()
    }

    /// One message into one frame. Anything unknown or misshapen is `None` (= not a frame).
    pub fn decode(raw: &str) -> Option<LinkFrame> {
        serde_json::from_str(raw).ok()
    }

    /// Read the machine's name from the dial path. `/bridge/desktop` → `desktop`.
    ///
    /// Only one segment passes, with a narrow alphabet (`[A-Za-z0-9][A-Za-z0-9_.-]*`). The name shows up
    /// in logs and in the `channels` table, and nothing is gained by accepting something like `..` as a name.
    pub fn bridge_id_of_path(path: &str) -> Option<&str> {
        let id = path.strip_prefix(BRIDGE_PATH)?;
        crate::bridge::state::is_machine_name(id).then_some(id)
    }

    /// Build the path a machine dials (`link` leaves it off when putting the URL in the connection string —
    /// the connecting side adds it).
    pub fn path_for(bridge_id: &str) -> String {
        format!("{BRIDGE_PATH}{bridge_id}")
    }

    // ── Section 1b: connection string (the one line the other side pastes) ────────────
    // A machine needs two things to reach the Relay: the URL to dial and the secret to present. Asking for
    // them separately gives two chances to create "a machine that connects to nothing and can't say why",
    // so the Relay prints one string and the other side pastes that one string. **The string is the password
    // itself** — whoever holds it can connect as a Bridge and receive the Slack messages meant for that machine.
    //
    // (Moving the handshake into the upgrade changed nothing here — handing over URL and secret as one
    //  string is worth it regardless of wire compatibility.)

    /// The prefix is the version. A future format change becomes a "clear refusal" rather than a "misreading".
    const PREFIX: &str = "SCLINK1-";

    /// What the pasted line holds — where to dial and the key. **Not the same as [`Conn`](super::Conn) in the connection book**
    /// (that one is the writer side of a connected link).
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Invite {
        /// Where to dial. The public WebSocket URL (`wss://…` in real use). **No path.**
        pub url: String,
        /// The secret presented to be let in. Carried in `Authorization: Bearer`.
        pub api_token: String,
    }

    /// The content is `{"u":<url>,"t":<apiToken>}` as base64url (no padding).
    ///
    /// **Key order is `u` → `t`**. Built with the `json!` macro, `serde_json::Map` is a BTreeMap and sorts
    /// alphabetically (`t` first) — so it's written as a struct to pin the declaration order.
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

    /// Read it back. Refuse anything that isn't exactly one connection string.
    ///
    /// The failure that actually happens is a **torn paste** — even half of a line the chat wrapped looks
    /// like a plausible blob. Silently ending up with a machine holding half a secret is the worst outcome,
    /// so the refusal names what's missing.
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
                // Say **by name** what's missing. Don't silently fill in defaults.
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

    /// The `base64` crate's URL-safe, no-padding engine. **A single character outside the alphabet gives `None`** —
    /// reading loosely would let a torn paste through (`+` and `/` are standard base64 characters, so they're refused).
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

    /// Build the request carrying the three upgrade parts (path / `Authorization` / `Sec-WebSocket-Protocol`).
    pub fn build_request(
        target: &str,
        api_token: &str,
    ) -> Result<tokio_tungstenite::tungstenite::handshake::client::Request, String> {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let mut request = target
            .into_client_request()
            .map_err(|e| format!("not a usable address ({target}): {e}"))?;
        let headers = request.headers_mut();
        headers.insert(
            "authorization",
            format!("Bearer {api_token}")
                .parse()
                .map_err(|_| "the key can't go in a header".to_string())?,
        );
        headers.insert(
            "sec-websocket-protocol",
            LINK_SUBPROTOCOL
                .parse()
                .map_err(|_| "the subprotocol can't go in a header".to_string())?,
        );
        Ok(request)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn frames() -> Vec<LinkFrame> {
            vec![
                LinkFrame::Ready {
                    bot_token: "xoxb-1".into(),
                    home: Some("C_HOME".into()),
                    gateway: Some("hub".into()),
                },
                LinkFrame::Ready {
                    bot_token: "xoxb-1".into(),
                    home: None,
                    gateway: None,
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
                LinkFrame::SetProject {
                    channel: "C1".into(),
                    thread_ts: "1700000000.000100".into(),
                    path: "~/dev/x".into(),
                },
                LinkFrame::ProjectSet {
                    channel: "C1".into(),
                    thread_ts: "1700000000.000100".into(),
                    result: Ok("/home/me/dev/x".into()),
                },
                LinkFrame::ProjectSet {
                    channel: "C1".into(),
                    thread_ts: "1700000000.000100".into(),
                    result: Err("no folder".into()),
                },
            ]
        }

        #[test]
        fn every_frame_round_trips() {
            for f in frames() {
                assert_eq!(decode(&encode(&f)).unwrap(), f, "{f:?}");
            }
        }

        /// A Ready without home omits the key entirely — so the Bridge keeps its current value.
        #[test]
        fn ready_without_home_omits_the_key() {
            let f = LinkFrame::Ready {
                bot_token: "xoxb-1".into(),
                home: None,
                gateway: None,
            };
            assert_eq!(encode(&f), r#"{"t":"ready","bot_token":"xoxb-1"}"#);
        }

        /// A wrong type means it's not a frame. We ride on the strictness serde gives for free.
        #[test]
        fn a_frame_with_a_wrong_type_is_not_a_frame() {
            for bad in [
                r#"{"t":"ready","bot_token":0}"#,
                r#"{"t":"ready"}"#,                  // no bot_token
                r#"{"t":"event","name":"message"}"#, // no event
                r#"{"t":"linked","owner_user_id":"U","channel":"D1"}"#, // no thread_ts
                r#"{"t":"linked","owner_user_id":"U","channel":"D1","thread_ts":12}"#,
            ] {
                assert!(decode(bad).is_none(), "{bad}");
            }
        }

        /// The old Bun handshake frames are now just "unknown frames".
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

        /// **Regression**: the reachability-check path must be separate and not caught by the name check.
    /// Several addresses is one setting, not one host. Asking `http://a:1,b:1/status` reached nothing,
        /// and `status` drew a running gateway as "not answering" with its machines missing.
        #[test]
        fn the_gateway_is_asked_at_the_first_address_it_accepts_on() {
            assert_eq!(
                super::super::first_addr("127.0.0.1:8787,192.0.2.10:8787"),
                "127.0.0.1:8787"
            );
            assert_eq!(super::super::first_addr("127.0.0.1:8787"), "127.0.0.1:8787");
        }

        /// With the reserved name `__link_probe__` under `/bridge/`, it got 400 because it didn't start with
        /// an alphanumeric, and `relay link` reported it couldn't reach itself (found on a real machine).
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
                "/bridge/",         // no name
                "/bridge/a/b",      // two segments
                "/",                // not even close
                "/bridge",          // no separator
                "/bridge/../etc",   // no reason to allow it
                "/bridge/-leading", // must start with an alphanumeric
                "/bridge/with space",
                "/status",
            ] {
                assert!(bridge_id_of_path(bad).is_none(), "{bad}");
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
            // Drop the whitespace and newlines that come along with a paste
            assert_eq!(decode_connection(&format!("\n  {s}  \n")).unwrap(), conn());
        }

        /// The version comes first, so a future format is refused rather than misread.
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
            // The long one too: wherever it's torn, it never yields half a secret
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

        /// A missing field is named — never silently filled with a default.
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
            assert!(b64url_decode("a").is_none()); // 4n+1 is not a valid encoding
            assert!(b64url_decode("ab+d").is_none()); // standard base64 characters are refused
        }

        #[test]
        fn the_request_carries_the_three_things_the_upgrade_needs() {
            let req = build_request("wss://relay.example/bridge/desktop", "s3cret").unwrap();
            assert_eq!(req.uri().path(), "/bridge/desktop");
            assert_eq!(req.headers().get("authorization").unwrap(), "Bearer s3cret");
            assert_eq!(
                req.headers().get("sec-websocket-protocol").unwrap(),
                LINK_SUBPROTOCOL
            );
        }

        #[test]
        fn a_url_that_is_not_a_websocket_target_is_refused_rather_than_dialled() {
            assert!(build_request("not a url", "s").is_err());
            assert!(build_request("", "s").is_err());
        }
    }
}

use crate::log::LogCtx;
use link::LINK_SUBPROTOCOL;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

/// Channel id → name of the machine in charge. Built from `routes[ch].bridge` in access.json
/// ([`crate::bridge::state::Access::bridges`]).
pub type Routes = BTreeMap<String, String>;

/// One-line logs for this section. The component is fixed at `relay` — fleet events are gathered here.
pub(super) fn rlog(level: &str, message: &str) {
    let ctx = LogCtx::default();
    match level {
        "error" => ctx.error("relay", message),
        "debug" => ctx.debug("relay", message),
        _ => ctx.info("relay", message),
    }
}

// ── Section 2: who gets in (whether to pass the upgrade) ──────────────────────
// The handshake happens in the WebSocket upgrade (see the `wire` module doc). This holds only that
// decision as a pure function, knowing neither sockets nor header types — so the tests all run synchronously.

/// Whether to pass the upgrade, and if refused, what to return.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admit {
    /// Pass. The name the machine gave.
    Ok(String),
    /// `link`'s reachability check. **Passed but not put in the connection book** — putting it there would
    /// briefly count as "a new machine connected" and show 🟢 in home. Its own path, so it can't collide with any machine name.
    Probe,
    /// 401 — api token missing / wrong. **The one and only check deciding who may connect**.
    Unauthorized,
    /// 426 — not speaking the same link protocol. The kind of thing a restart with new code fixes.
    WrongVersion,
    /// 400 — the path has no machine name (or has characters a name can't use).
    BadPath,
}

impl Admit {
    /// The HTTP status returned on refusal. Passing is 101, so `None`.
    pub fn status(&self) -> Option<u16> {
        match self {
            Admit::Ok(_) | Admit::Probe => None,
            Admit::Unauthorized => Some(401),
            Admit::WrongVersion => Some(426),
            Admit::BadPath => Some(400),
        }
    }

    /// The refusal reason in one line. **A silent 401 costs someone an hour of debugging.**
    pub fn why(&self) -> &'static str {
        match self {
            Admit::Ok(_) => "admitted",
            Admit::Probe => "admitted (reachability probe)",
            Admit::Unauthorized => "the api token is missing or wrong",
            Admit::WrongVersion => "the two sides do not speak the same link protocol",
            Admit::BadPath => "the path carries no usable Bridge ID",
        }
    }

    /// Decide from the three upgrade inputs (path / `Authorization` / `Sec-WebSocket-Protocol`).
    ///
    /// **The order is the spec**: (1) same language? → (2) api token → (3) **only then** trust the name.
    /// Being able to call yourself `desktop` must not be a reason to receive Slack messages meant for desktop.
    /// The version comes first so a mismatch isn't wrongly reported as "wrong token".
    pub fn of(
        path: &str,
        authorization: Option<&str>,
        subprotocol: Option<&str>,
        api_token: &str,
    ) -> Admit {
        // (1) Sec-WebSocket-Protocol can carry several, comma-separated. One match is enough.
        let speaks =
            subprotocol.is_some_and(|v| v.split(',').any(|p| p.trim() == LINK_SUBPROTOCOL));
        if !speaks {
            return Admit::WrongVersion;
        }
        // (2) `Bearer <token>`. Until this passes, the name is just a string.
        let presented = authorization
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("");
        if !secret_eq(presented, api_token) {
            return Admit::Unauthorized;
        }
        // (3) Only now read the name. The reachability check gives no name (its own path).
        if path == link::PROBE_PATH {
            return Admit::Probe;
        }
        match link::bridge_id_of_path(path) {
            Some(id) => Admit::Ok(id.to_string()),
            None => Admit::BadPath,
        }
    }
}

/// Constant-time secret comparison. Different lengths are `false` without looking at the content (length isn't secret).
///
/// Not worth adding a dependency just for this.
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

// ── Section 3: who is connected right now (connection book) ───────────────────────────

/// The writer side of one link. It's only "somewhere to throw strings" — the connection book knows no sockets,
/// so tests run without them.
#[derive(Clone)]
pub struct Conn(Arc<tokio::sync::mpsc::UnboundedSender<String>>);

impl Conn {
    pub fn new(tx: tokio::sync::mpsc::UnboundedSender<String>) -> Self {
        Self(Arc::new(tx))
    }

    /// Send one frame. `false` = that link is already dead (the receiver is gone).
    pub fn send(&self, frame: &link::LinkFrame) -> bool {
        self.0.send(link::encode(frame)).is_ok()
    }

    /// **Is it the same link** (identity, not content equality). The key that keeps an old link's
    /// cleanup from sweeping up a newer registration.
    fn is(&self, other: &Conn) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl std::fmt::Debug for Conn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Conn({:p})", Arc::as_ptr(&self.0))
    }
}

/// The result of connecting. Decides whether it shows in presence (🟢/🔴).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Joined {
    /// Newly arrived. Tell the Owner.
    New,
    /// A machine that was already connected reconnected (a Wi-Fi blip, etc.). **Don't tell** —
    /// from the Owner's view, that machine never left.
    Reconnected,
}

/// Bridge ID → the link currently serving that name. **Forwarding looks only at this table.**
#[derive(Default)]
/// A panic while one of these locks is held poisons it. **Recover instead of panicking on it**: the
/// connection book and the tunnel list are read on every frame, so one poisoned lock would take the whole
/// fleet down, task by task, with the machines still connected and nothing forwarding.
pub struct LinkServer {
    bridges: Mutex<HashMap<String, Conn>>,
}

impl LinkServer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an authenticated link. Returns whether it displaced an older one, and the displaced link.
    ///
    /// **The newest link wins.** A machine whose Wi-Fi dropped reconnects before our old socket
    /// knows it's dead. Refusing the newcomer would leave that machine missing until the dead
    /// socket times out.
    pub fn register(&self, bridge_id: &str, conn: Conn) -> (Joined, Option<Conn>) {
        let mut bridges = self.bridges.lock().unwrap_or_else(|e| e.into_inner());
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

    /// A link closed. `true` = **it's really gone** (show it in presence).
    ///
    /// It's `false` when a displaced old link closes later. Removing the whole name there would take
    /// the live new link's registration down with it, and the machine would go missing
    /// (the same trap as the original removing the bridgeId first).
    pub fn unregister(&self, bridge_id: &str, conn: &Conn) -> bool {
        let mut bridges = self.bridges.lock().unwrap_or_else(|e| e.into_inner());
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

    /// The Bridge IDs connected right now. Only these can be a `pwd <machine>` target, and forwarding looks at them too.
    pub fn connected(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.bridges.lock().unwrap_or_else(|e| e.into_inner()).keys().cloned().collect();
        ids.sort();
        ids
    }

    pub fn is_connected(&self, bridge_id: &str) -> bool {
        self.bridges.lock().unwrap_or_else(|e| e.into_inner()).contains_key(bridge_id)
    }

    /// Send a frame to one machine. `false` = it wasn't there (including dropping between looking up the
    /// table and sending). **Never pretend it arrived.**
    pub fn send_to(&self, bridge_id: &str, frame: &link::LinkFrame) -> bool {
        let conn = self.bridges.lock().unwrap_or_else(|e| e.into_inner()).get(bridge_id).cloned();
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

// ── Section 4: reading what arrives and where it goes ─────────────────────────────
// The route table is only "channel → machine"; there is **deliberately no** thread → machine table.
// A thread lives inside a channel, so the channel's owner owns every thread in it.

/// One Slack event. **All the reading is gathered here** — passing a raw `Value` around scatters the same
/// `get("…")` everywhere, and you lose track of which is the text and which is the thread.
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

    /// The channel this event happened in.
    ///
    /// Button presses are carried by [`Click`] — so clicks need no special routing
    /// (and **must never be broadcast**: a machine that doesn't know the request would answer
    /// "this has expired", which would be a lie).
    pub fn channel(&self) -> Option<&'a str> {
        match self.name {
            "message"
            | "member_joined_channel"
            | "agent_session_stopped"
            | "agent_session_title_changed" => self.str_at("channel"),
            "reaction_added" | "reaction_removed" => self.raw.get("item")?.get("channel")?.as_str(),
            _ => None,
        }
    }

    pub fn user(&self) -> Option<&'a str> {
        self.str_at("user")
    }

    /// What the person actually wrote. Edits sit one level down (`message.text`).
    pub fn text(&self) -> Option<&'a str> {
        self.str_at("text")
            .or_else(|| self.raw.get("message")?.get("text")?.as_str())
    }

    /// Where a reply to this message goes. Inside the thread if in one, otherwise under it.
    pub fn thread(&self) -> Option<&'a str> {
        self.str_at("thread_ts").or_else(|| self.str_at("ts"))
    }

    /// Written by the bot itself — **including these very refusals**. Without this, a channel with no
    /// machine assigned keeps answering its own "no machine assigned" with "no machine assigned".
    pub fn from_a_bot(&self) -> bool {
        self.raw.get("bot_id").is_some_and(|v| !v.is_null())
            || self.str_at("subtype") == Some("bot_message")
    }

    /// The Owner **deleted** their message. If the machine is online it's forwarded (the Bridge turns it into an interrupt), but
    /// when it can't be delivered there's **nothing to resend and nothing to say** — drop it silently (it's still logged).
    /// An edit is not a deletion.
    pub fn is_retraction(&self) -> bool {
        self.name == "message" && self.str_at("subtype") == Some("message_deleted")
    }

    /// Whether to treat this post as a bot. **Only the Owner's Web API posts count as a person** (the
    /// rule `Access::gate` already has, applied at the command entry too).
    ///
    /// Posts via the Web API carry a `bot_id` even when a person wrote them, but Slack also stamps the real
    /// `user` (it comes from the token, so the text can't fake it). Not the Owner, no user, or no Owner set
    /// all count as a bot = **fail closed** (never open a path where our own post is parsed and answered by ourselves).
    pub fn speaks_as_a_bot(&self, owner: Option<&str>) -> bool {
        if !self.from_a_bot() {
            return false;
        }
        !matches!((self.user(), owner), (Some(u), Some(o)) if u == o)
    }

    /// **Whether the gateway looks at it itself before delivery** (is it a command or name-claim candidate).
    ///
    /// Only "a person's message whose channel could be read" qualifies. Reactions and joins
    /// can't be commands.
    pub fn is_a_command_candidate(&self, owner: Option<&str>) -> bool {
        self.name == "message" && self.channel().is_some() && !self.speaks_as_a_bot(owner)
    }

    /// Whether the gateway may speak up about this event.
    ///
    /// Reactions and joins aren't addressed to anyone. Never answer a bot's words
    /// (infinite loop). In channels a mention is required — the gateway is **deliberately stricter** than the Bridge's gate:
    /// the Bridge also responds to "a follow-up in a running thread", but only that machine knows
    /// which threads are alive, and it can't be asked exactly when it's away.
    pub fn may_answer(&self, bot_user_id: Option<&str>) -> bool {
        let Some(channel) = self.channel() else {
            return false;
        };
        if self.name != "message" || self.from_a_bot() || self.is_retraction() {
            return false;
        }
        crate::chat::slack::SlackId::is_dm(channel)
            || crate::bridge::command::Message::new(self.text().unwrap_or(""), bot_user_id)
                .mentions_bot()
    }
}

/// One button press (the `block_actions` body Slack sends).
pub struct Click<'a>(pub &'a serde_json::Value);

impl Click<'_> {
    /// The channel it was pressed in. That's where the prompt was posted, so routing is the same as for a message.
    pub fn channel(&self) -> Option<&str> {
        self.0.get("channel")?.get("id")?.as_str()
    }
}

/// The delivery decision. **Never picks a machine on its own, and never holds messages for an absent machine** —
/// guessing and hoarding both fail silently, so this design chooses to say it out loud instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    /// The machine in charge is connected. Hand it over.
    Forward(String),
    /// There is a machine in charge, but it's offline now.
    Offline(String),
    /// This machine handles it itself. **No machine written down = here** (the default for a lone Bridge).
    Local,
    /// Couldn't even read which channel it happened in. **Never drop it silently; always log it.**
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
            // **A channel with no machine assigned is handled here.** A Bridge with no machines has
            // an empty table, so it passes through here and works as before
            None => Delivery::Local,
            Some(bridge_id) if bridge_id == self_id => Delivery::Local,
            Some(bridge_id) if is_connected(bridge_id) => Delivery::Forward(bridge_id.clone()),
            Some(bridge_id) => Delivery::Offline(bridge_id.clone()),
        }
    }

    /// What to say when the machine in charge is away. **Say it on the spot. Don't hold the message.**
    /// A reply reaching someone an hour later, after they've lost interest, is worse than an honest refusal.
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

// ── Notice rate limit ───────────────────────────────────────
// Events don't stop while a machine is away, and one question becomes several events (typing, edits,
// redelivery). So once per minute per channel. **Not "say it once and never again"** —
// a long silence is exactly what this notice prevents. Delivery clears it (`delivered`), so
// the first failure after the machine comes back is reported right away.
pub const NOTICE_COOLDOWN_MS: u64 = 60_000;

/// "May we say it now" and "how many were swallowed since we last said it".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoticeDecision {
    pub say: bool,
    pub swallowed: u32,
}

#[derive(Default)]
pub struct NoticeCooldown {
    /// Channel → when we last said it
    said_at: HashMap<String, u64>,
    /// Channel → how many swallowed since then (reported next time we speak)
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

    /// Something reached this channel's machine. **The complaint is over** — forget it.
    pub fn delivered(&mut self, channel_id: &str) {
        self.said_at.remove(channel_id);
        self.held.remove(channel_id);
    }
}

// ── Section 5: commands the gateway answers itself ───────────────────────────────
// Only "things no machine can be asked" live here: `pwd <machine>` / `channels` (the machine you'd ask is the thing being changed),
// `set-home` (a broadcast to the whole fleet), and the DM name-claim (the moment the Owner is born).
// Decisions are pure functions; the caller does the execution (saving, posting, forwarding).

/// The context one command decision needs.
pub struct CommandCtx<'a> {
    pub channel_id: &'a str,
    pub user_id: Option<&'a str>,
    pub text: &'a str,
    /// `None` until the Owner is decided — until then nobody can give commands.
    pub owner_user_id: Option<&'a str>,
    /// `None` until the Relay resolves its own id ⇒ no channel command can succeed.
    pub bot_user_id: Option<&'a str>,
}

impl CommandCtx<'_> {
    fn is_dm(&self) -> bool {
        crate::chat::slack::SlackId::is_dm(self.channel_id)
    }

    fn msg(&self) -> crate::bridge::command::Message<'_> {
        crate::bridge::command::Message::new(self.text, self.bot_user_id)
    }

    /// Whether this command is addressed to the bot. DMs need no mention.
    ///
    /// An unaddressed `pwd` / `channels` **isn't even refused** — cutting into people's conversation over one word
    /// with "that's Owner-only" is barging into a conversation the bot isn't part of.
    fn addressed(&self) -> bool {
        self.is_dm() || self.msg().mentions_bot()
    }

    /// If the text is a command starting with `verb`, the words after it. `None` if not addressed.
    fn verb_args(&self, verb: &str) -> Option<Vec<String>> {
        self.addressed()
            .then(|| self.msg().verb_args(verb))
            .flatten()
    }

    /// The Owner check. **Whether a request is well-formed comes after whether that person may make it**.
    /// Messages whose author is unknown (bots) are refused too.
    fn refuse_if_not_owner(&self, command: &str) -> Option<String> {
        match (self.user_id, self.owner_user_id) {
            (Some(u), Some(o)) if u == o => None,
            _ => Some(crate::t!(
                "Only the owner can use `{command}`.",
                "`{command}` を使えるのは Owner だけです。"
            )),
        }
    }

/// `pwd <machine>` — the Owner picks the machine in charge of this channel.
///
/// **Owner only**. Without this, anyone else in a shared channel could type `pwd my-machine` and
/// hijack the channel, sending every later message to their machine (with their permissions).
/// This check is not decoration.
    /// `self_id` is the gateway's name — **channels with no machine assigned go to the gateway**, so the list says so.
    /// `channels` (the table) and `pwd <machine>[:<path>]` (handing this channel to a machine). Both are
    /// answered here — the gateway is the one that knows the machines. `pwd <path>` is not: it goes to the
    /// machine running this channel, which is where the folder is.
    pub fn route(&self, connected: &[String]) -> RouteOutcome {
        let ctx = self;
        let listing = ["channels", "channel"]
            .iter()
            .any(|verb| ctx.verb_args(verb).is_some_and(|args| args.is_empty()));
        if listing {
            if let Some(reply) = ctx.refuse_if_not_owner("channels") {
                return RouteOutcome::Refused(reply);
            }
            return RouteOutcome::List;
        }
        let pwd = ctx
            .addressed()
            .then(|| crate::bridge::command::Cmd::pwd(&ctx.msg()))
            .flatten();
        let Some(crate::bridge::command::PwdMode::On { machine: bridge_id, path }) = pwd else {
            return RouteOutcome::NotACommand;
        };
        if let Some(reply) = ctx.refuse_if_not_owner("pwd") {
            return RouteOutcome::Refused(reply);
        }
        let bridge_id = &bridge_id;

        // **Only a machine connected right now can be the target.** A typo and a machine not yet started
        // are treated the same — the only test is "is it here now". Never silently create an assignment with nowhere to go.
        if !connected.iter().any(|c| c == bridge_id) {
            return RouteOutcome::UnknownBridge(unknown_machine(bridge_id, connected));
        }        // The machine checks the folder first; the channel moves only if it's there. With no folder named,
        // that machine's home — so `pwd <machine>` records `<machine>:~` instead of leaving it unset
        RouteOutcome::SetProject {
            bridge_id: bridge_id.clone(),
            path: path.unwrap_or_else(|| "~".to_string()),
        }
    }


    /// `set-home` — the channel it's typed in becomes the fleet-wide home. Takes no arguments.
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
        SetHomeOutcome::Set(set_home_reply(ctx.channel_id))
    }
}

/// What `set-home` says once the notice channel is this one.
fn set_home_reply(ch: &str) -> String {
    crate::t!(
        "Notices from every machine will now go to <#{ch}>.",
        "これからは、すべてのマシンの通知を <#{ch}> に出します。"
    )
}

/// Why a name isn't a machine we can hand a channel to, and what is online instead.
fn unknown_machine(bridge_id: &str, connected: &[String]) -> String {
    let here = connected
        .iter()
        .map(|m| format!("`{m}`"))
        .collect::<Vec<_>>()
        .join(", ");
    let here = if here.is_empty() {
        crate::t!("none", "なし")
    } else {
        here
    };
    crate::t!(
        "No machine named `{bridge_id}` is connected.\nOnline machines: {here}",
        "`{bridge_id}` という名前のマシンはつながっていません。\nオンラインのマシン: {here}"
    )
}

/// What `pwd <machine>` says once the channel is handed over — including, when it moves between machines,
/// what the new machine can't know.
fn handover_reply(channel: &str, routes: &Routes, bridge_id: &str) -> String {
    let mut lines = vec![crate::t!(
        "This channel is now handled by *{bridge_id}*.",
        "このチャンネルは *{bridge_id}* が受け持つようになりました。"
    )];
    if let Some(before) = routes.get(channel).filter(|b| *b != bridge_id) {
        // The honest part: the new machine can reread the Slack thread, but it can't know **what the
        // previous machine did** (which files it read, what it tried, what it changed). Saying so lets
        // the handover stay quiet about everything else.
        lines.push(crate::t!(
            "(It was *{before}* before. Running threads continue on *{bridge_id}*, which reads \
             the thread to catch up — but *it can't see what {before} actually did*.)",
            "(前は *{before}* でした。進行中のスレッドは *{bridge_id}* で続きます。スレッドは読み直して\
             流れを把握しますが、*{before} が実際に行った作業の中身は分かりません*。)"
        ));
    }
    lines.join("\n")
}

#[derive(Debug, PartialEq, Eq)]
pub enum RouteOutcome {
    NotACommand,
    Refused(String),
    /// `channels` — the caller builds the table (it needs Slack, and this decision runs on every message).
    List,
    /// `pwd <machine>:<path>`: ask the machine to set `path` up; bind only on its yes.
    SetProject { bridge_id: String, path: String },
    UnknownBridge(String),
}

/// The answer to `channels`. Says **what's going on in the channel it was typed in** first,
/// then lists the other channels and machines. Each one is marked online or not.
///
/// Channels with no machine assigned go to the gateway — writing only "unassigned" in the list
/// leaves the reader unsure where messages written there go.
pub fn route_table(
    here: &str,
    access: &Access,
    connected: &[String],
    self_id: &str,
    homes: &HashMap<String, String>,
    // `in_slack`: channels the bot is in. One nobody was assigned is the gateway's, and there is
    // nothing in the assignments to learn it from
    in_slack: &[String],
) -> String {
    let online = |id: &str| connected.iter().any(|c| c == id);
    // Channel rows only flag trouble: a row per channel all marked 🟢 is noise (the machines line
    // at the bottom already shows who is up)
    let mark = |id: &str| {
        if online(id) {
            String::new()
        } else {
            crate::t!(" 🔴 offline", " 🔴 オフライン")
        }
    };
    // Where a channel's work happens: the machine, and the folder on it when one is set.
    // **No machine written down = the gateway** (the default for a lone Bridge)
    let dest = |ch: &str| {
        let route = access.routes.get(ch);
        let id = route
            .and_then(|r| r.bridge.as_deref())
            .unwrap_or(self_id)
            .to_string();
        // No folder of its own = that machine's home, which it told us when it connected
        let folder = route
            .and_then(|r| r.repo_path.as_deref())
            .filter(|p| !p.is_empty())
            .or_else(|| homes.get(&id).map(String::as_str));
        let where_ = match folder {
            Some(path) => format!("`{id}:{path}`"),
            None => format!("*{id}*"),
        };
        format!("{where_}{}", mark(&id))
    };

    let mut out = vec![
        crate::t!("*This channel is mapped to*", "*このチャンネルの割り当て*"),
        format!("<#{here}> → {}", dest(here)),
    ];

    let mut ids: Vec<&str> = access.routes.keys().map(String::as_str).collect();
    for ch in in_slack {
        if !ids.contains(&ch.as_str()) {
            ids.push(ch);
        }
    }
    ids.sort();
    let others: Vec<String> = ids
        .into_iter()
        .filter(|ch| *ch != here)
        .map(|ch| format!("- <#{ch}> → {}", dest(ch)))
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

    // Machines: those connected + those named only in routes (= not here now)
    let mut machines: Vec<String> = connected.to_vec();
    for id in access.routes.values().filter_map(|r| r.bridge.clone()) {
        if !machines.contains(&id) {
            machines.push(id);
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
    /// Typed in a DM. home must be a **channel**.
    NeedsChannel(String),
    Set(String),
}

// ── DM name-claim — the moment the Owner is born ───────────────────────────
// A new Relay has no Owner. The person who set it up proves they did by **DMing the connection string**
// (only the Relay's operator has that secret). After that: with 0 machines connected there's nothing to bind to,
// with 1 it's automatic, with several it asks for the name. This isn't `pwd <machine>` — it's the moment the Owner is born,
// and the `Linked` sent right after tells the Bridge "this person is your Owner".

pub struct DmOnboardingCtx<'a> {
    pub text: &'a str,
    /// The author (`None` for a bot / anonymous — that person can't become the Owner).
    pub user_id: Option<&'a str>,
    /// The Relay's own api token. A DMed connection string containing it is the proof of ownership.
    pub api_token: &'a str,
    pub current_owner: Option<&'a str>,
    pub connected: &'a [String],
    /// Whether the Owner has claimed and we're waiting for a machine name.
    pub awaiting_selection: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum DmOnboarding {
    NotOnboarding,
    /// Looked like a connection string, but not this bot's (different token / torn paste).
    BadToken(String),
    /// Right token, but there's already an Owner. **No re-claiming. The secret isn't forwarded either.**
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
    /// The moment a connection string pasted in a DM decides the Owner.
    pub fn decide(&self) -> DmOnboarding {
        let ctx = self;
        let list = if ctx.connected.is_empty() {
            crate::t!("none", "なし")
        } else {
            ctx.connected.join(", ")
        };

        // (1) Waiting for a name: only the Owner's reply can end it.
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
                link::decode_connection(text).is_ok_and(|c| secret_eq(&c.api_token, ctx.api_token)),
            )
        };

        // (2) There's already an Owner: a DMed token isn't a re-claim, and the secret isn't passed on.
        if ctx.current_owner.is_some() {
            return match token {
                Some(true) => DmOnboarding::AlreadyConfigured(crate::t!(
                    "This bot already has an owner.",
                    "このボットには既に Owner がいます。"
                )),
                _ => DmOnboarding::NotOnboarding,
            };
        }

        // (3) No Owner yet.
        match token {
            None => DmOnboarding::NotOnboarding, // an ordinary DM before setup
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

// ── Section 6: presence (post machines joining and leaving the fleet to home) ──────────────────────

/// How long a disconnect is held. A machine whose Wi-Fi blinked comes back in 1–2 seconds, so if it
/// returns within this window nothing is said — from the Owner's view, the machine never left.
pub const PRESENCE_GRACE_MS: u64 = 5_000;

#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
struct Seen {
    /// Whether the Owner currently believes it's "connected".
    up: bool,
    /// When the grace period ends (`Some` only while a disconnect is being held).
    down_due_ms: Option<u64>,
}

/// Turns joins / drops into one line in home. The clock is passed in by the caller (no timers here).
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

    /// Connected. **Say nothing** — telling people "connected" is the machine's own `online` notice
    /// (with version, pid and warm pool), so the same thing isn't said in two places. This only
    /// tracks the state for "the one that dropped came back".
    pub fn on_connect(&mut self, bridge_id: &str) {
        let e = self.seen.entry(bridge_id.to_string()).or_default();
        // A blink (dropped and back within the grace period) and a first connection are treated the same —
        // throw away the held "disconnect" and mark it connected
        e.down_due_ms = None;
        e.up = true;
    }

    /// Disconnected. **Not said right away** — after the grace period [`Presence::due`] picks it up.
    pub fn on_disconnect(&mut self, bridge_id: &str, now_ms: u64) {
        if let Some(e) = self.seen.get_mut(bridge_id) {
            if e.up && e.down_due_ms.is_none() {
                e.down_due_ms = Some(now_ms + self.grace_ms);
            }
        }
    }

    /// Collect disconnects whose grace period has run out. Call periodically.
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

/// What the fleet section of `status` shows. Built from disk and env (the caller does the I/O).
pub struct FleetView {
    /// `host:port`. Where machines are accepted.
    pub listen: String,
    pub owner: Option<String>,
    pub home: Option<String>,
    pub routes: Routes,
}

/// The state of one ssh tunnel the gateway keeps open. **The gateway opens it itself, so it knows**
/// ([`keep_tunnel`] writes it when the state changes).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Tunnel {
    /// ssh target (e.g. `me@laptop`)
    pub target: String,
    /// Why it's not up. `None` when it is
    pub error: Option<String>,
}

/// Machine name → the tunnel to that machine. Machines not listed connect directly.
pub type Tunnels = HashMap<String, Tunnel>;

/// One machine's route in a few words.
/// The name **this** resolver gives an address, asked of the OS. `None` when nothing answers.
///
/// Looked up here and not on the machine: the name is read by someone sitting here, and a machine can
/// call itself something nobody else can resolve — a Proxmox host naming itself `build-box.local` out of its
/// own `/etc/hosts` while the network's DNS calls it `build-box.lan` (seen on a real machine).
async fn name_here(ip: &str) -> Option<String> {
    if ip.is_empty() {
        return None;
    }
    // No resolver crate for one lookup: `getent` where there is one, `host` elsewhere (macOS has it)
    let ask = async |program: &str, args: Vec<&str>| -> Option<String> {
        let out = tokio::process::Command::new(program)
            .args(args)
            .output()
            .await
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).to_string())
    };
    let said = match tokio::time::timeout(std::time::Duration::from_secs(2), async {
        match ask("getent", vec!["hosts", ip]).await {
            Some(out) => Some(name_from_getent(&out)),
            None => ask("host", vec!["-W", "2", ip]).await.map(|o| name_from_host(&o)),
        }
    })
    .await
    {
        Ok(said) => said?,
        Err(_) => None,
    };
    said.filter(|n| !n.is_empty() && n != ip)
}

/// The address a name has **here**, asked of the OS. `None` when nothing answers.
///
/// The mirror of [`name_here`]: a machine behind an ssh tunnel dials its own loopback, so the address
/// it reports says nothing about where it is. What reaches it is the gateway's ssh target — a name,
/// which only this resolver can turn back into an address.
async fn address_here(name: &str) -> Option<String> {
    if name.is_empty() || name.parse::<std::net::IpAddr>().is_ok() {
        return None;
    }
    let ask = async |program: &str, args: Vec<&str>| -> Option<String> {
        let out = tokio::process::Command::new(program)
            .args(args)
            .output()
            .await
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).to_string())
    };
    let said = match tokio::time::timeout(std::time::Duration::from_secs(2), async {
        match ask("getent", vec!["hosts", name]).await {
            Some(out) => Some(address_from_getent(&out)),
            None => ask("host", vec!["-W", "2", name]).await.map(|o| address_from_host(&o)),
        }
    })
    .await
    {
        Ok(said) => said?,
        Err(_) => None,
    };
    said.filter(|a| !a.is_empty())
}

/// `192.0.2.20   build-box.lan` → `192.0.2.20`. The address comes first.
fn address_from_getent(out: &str) -> Option<String> {
    out.lines().next()?.split_whitespace().next().map(str::to_string)
}

/// `build-box.lan has address 192.0.2.20` → `192.0.2.20`.
fn address_from_host(out: &str) -> Option<String> {
    out.lines()
        .find_map(|l| l.rsplit_once("has address "))
        .map(|(_, ip)| ip.trim().to_string())
}

/// `192.0.2.20   build-box.lan build-box` → `build-box.lan`. The first name is the canonical one.
fn name_from_getent(out: &str) -> Option<String> {
    out.lines()
        .next()?
        .split_whitespace()
        .nth(1)
        .map(str::to_string)
}

/// `20.2.0.192.in-addr.arpa domain name pointer build-box.lan.` → `build-box.lan`.
fn name_from_host(out: &str) -> Option<String> {
    out.lines()
        .find_map(|l| l.rsplit_once("pointer "))
        .map(|(_, name)| name.trim().trim_end_matches('.').to_string())
}

/// The host part of an ssh target: `user@build-box.lan` → `build-box.lan`.
pub fn ssh_host_of(target: &str) -> &str {
    target.rsplit_once('@').map(|(_, host)| host).unwrap_or(target)
}

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

/// One line of the `machines` table. **The gateway takes one line per way in**, so several rows can
/// be the gateway — only the first of them carries the name.
struct MachineRow {
    id: String,
    host: String,
    ip: String,
    /// How the gateway reaches it: `direct`, `ssh tunnel (…)`, or `gateway` for ourselves
    link: String,
    online: bool,
    /// Why the tunnel is down. Kept out of the table — a long reason pulls the columns apart
    down: Option<String>,
    /// This row is one of the gateway's own ways in, not a machine.
    gateway: bool,
}

/// One way in to this gateway. The gateway dials nobody, so it has no interface of its own to name —
/// what it has is the ways machines arrive.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Entrance {
    /// `host:port` as a machine would dial it.
    pub at: String,
    /// What terminates TLS in front, when anything does (`tailscale serve`).
    pub front: Option<String>,
}

/// Every way in to this gateway: the addresses it holds itself, then the ones a front hands it.
///
/// **The two are not the same list.** A machine dialling `wss://<name>` reaches `tailscale serve` on
/// the tailnet's 443, which forwards to our loopback — so that entrance never appears among the
/// addresses we bind, and reading `AGENTGW_LINK_LISTEN` alone misses the one actually in use.
/// Loopback is listed first because it is the gateway's own way in, and what the fronts forward to.
///
/// `serve_json` is `tailscale serve status --json`. **Only its top-level `Web`** — `Services` holds
/// other machines' services, which say nothing about us.
///
/// **Pure function** — the caller does the I/O.
pub(crate) fn entrances(listen: &str, serve_json: Option<&str>) -> Vec<Entrance> {
    let held: Vec<&str> = listen.split(',').map(str::trim).filter(|a| !a.is_empty()).collect();
    let mut out: Vec<Entrance> = held
        .iter()
        .map(|a| Entrance { at: a.to_string(), front: None })
        .collect();
    let Some(web) = serve_json
        .and_then(|j| serde_json::from_str::<serde_json::Value>(j).ok())
        .and_then(|v| v.get("Web").cloned())
        .and_then(|w| w.as_object().cloned())
    else {
        return out;
    };
    for (at, entry) in web {
        // Ours only when it forwards to an address we hold. Anything else on 443 is someone else's
        let ours = entry
            .get("Handlers")
            .and_then(|h| h.as_object())
            .is_some_and(|hs| {
                hs.values().any(|h| {
                    h.get("Proxy")
                        .and_then(|p| p.as_str())
                        .and_then(|p| p.split("//").nth(1))
                        .is_some_and(|dest| held.contains(&dest))
                })
            });
        if ours && !out.iter().any(|e| e.at == at) {
            out.push(Entrance { at, front: Some("tailscale serve".to_string()) });
        }
    }
    out
}

/// The `machines` answer as a standard-Markdown table (so it must go out with `post_markdown`).
/// **Pure function** — the caller does the I/O.
fn machines_md(rows: &[MachineRow]) -> String {
    let cell = |s: &str| match s.is_empty() {
        true => String::new(),
        false => format!("`{s}`"),
    };
    let mut out = vec![
        crate::t!("**Machines**", "**マシン**"),
        String::new(),
        crate::t!(
            "| machine | host | address | link | status |",
            "| マシン | ホスト | アドレス | つなぎ方 | 状態 |"
        ),
        "|---|---|---|---|---|".to_string(),
    ];
    for r in rows {
        // Nothing at all to say about where it is — the one case that isn't a code span
        let host = match (r.host.is_empty(), r.ip.is_empty()) {
            (true, true) => crate::t!("(not known here)", "(こちらでは分かりません)"),
            _ => cell(&r.host),
        };
        let (id, ip, link) = (cell(&r.id), cell(&r.ip), &r.link);
        let state = match r.online {
            true => "🟢",
            false => "🔴",
        };
        out.push(format!("| {id} | {host} | {ip} | {link} | {state} |"));
    }
    // Only the gateway's own ways in — no machine has been added
    if rows.iter().all(|r| r.gateway) {
        out.push(String::new());
        out.push(crate::t!(
            "(no machines yet — run `agentgw add-machine user@host` here)",
            "(マシンはまだありません。ここで `agentgw add-machine user@host` を実行してください)"
        ));
    }
    let mut down: Vec<String> = Vec::new();
    for r in rows {
        let Some(why) = &r.down else { continue };
        let id = cell(&r.id);
        down.push(crate::t!(
            "{id} — the ssh tunnel is down: {why}",
            "{id} — ssh トンネルがつながっていません: {why}"
        ));
    }
    if !down.is_empty() {
        out.push(String::new());
        out.extend(down);
    }
    out.join("\n")
}

/// Plain-text rendering of the fleet section of `status`. **Pure function** — the caller does the I/O.
///
/// `connected` is `None` = the running Bridge didn't answer (= it's not running). Even then the route table
/// on disk is shown — don't mix up "not running" with "not configured".
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
    // Headings say **what the thing is**. The raw `owner:` `home:` never told the reader whether
    // it was a person or a channel, or what it affects
    // **All we know is "did this endpoint answer".** Claiming alive/dead would show "not running" right next to
    // launchd saying running (right after a restart the endpoint often isn't open yet)
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
        out.push(crate::t!("● Channels assigned — none", "● チャンネルの割り当て — なし"));
    } else {
        out.push(crate::t!("● Channels assigned — {n}", "● チャンネルの割り当て — {n}"));
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

// ── Section 7: gateway side ───────────────────────────────────────
// From here down is I/O. It only wires up and runs what sections 1–6 decided, and makes no decisions itself.

use crate::chat::InboundMsg;
use crate::bridge::state::Access;
use crate::state_dir::StateDir;
use crate::clock::now_ms;
use crate::chat::slack::{FleetEvent, PermClick};
use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::mpsc::Sender;

/// What a Bridge with machines (= the gateway) carries around.
///
/// **The disk (access.json) is the source of truth for owner / home / the assignment table.** The Bridge itself
/// holds the same file, so after writing here we send one `reload` to make it reread — never keep two copies
/// in memory that can drift apart.
pub struct Fleet {
    pub links: LinkServer,
    /// The key machines present.
    pub token: String,
    /// This machine's name. What `pwd <own id>` points at.
    pub self_id: String,
    /// machine → the folder its agents start in when a channel has no folder of its own. Filled when a
    /// machine connects; **memory only**, since it is only for showing `channels`.
    pub homes: tokio::sync::Mutex<HashMap<String, String>>,
    /// machine → (host, ip), as the machine itself reported on connecting. **Memory only** for the same reason.
    pub hosts: tokio::sync::Mutex<HashMap<String, (String, String)>>,
    /// The Slack bot token handed to machines (given out in the `Ready` frame).
    pub bot_token: String,
    pub api: crate::chat::ChatRef,
    pub dir: StateDir,
    pub cooldown: AsyncMutex<NoticeCooldown>,
    pub presence: AsyncMutex<Presence>,
    /// Where a DM name-claim is stuck "waiting for a machine name". Memory only.
    pub pending_selection: AsyncMutex<Option<(String, String)>>,
    /// Our own Slack user id (for detecting `@mention`). Filled after auth.test.
    pub bot_user_id: AsyncMutex<Option<String>>,
    /// Local delivery. Flows into **the same two channels as direct mode**.
    pub msg_tx: Sender<InboundMsg>,
    pub click_tx: Sender<PermClick>,
    /// Tells the Bridge itself that access.json was written (the same reload as SIGHUP).
    pub reload: Sender<()>,
    /// The ssh tunnels we keep open (to show the route in `status`).
    pub tunnels: std::sync::Mutex<Tunnels>,
    /// machine → the address of ours it arrived on. **Memory only.** A machine behind a front
    /// arrives on our loopback, which is the truth: that is the way in being used.
    ///
    /// **Never cleared when a machine goes away.** It is read to decide which addresses may be
    /// closed, and a machine that is merely offline right now must not have its way in taken away.
    pub entrance: tokio::sync::Mutex<HashMap<String, String>>,
}

impl Fleet {
    fn access(&self) -> Access {
        Access::load(&self.dir)
    }

    /// Rewrite access.json and make the Bridge itself reread it.
    async fn edit_access(&self, f: impl FnOnce(&mut Access)) {
        let mut access = self.access();
        f(&mut access);
        if let Err(e) = access.save(&self.dir) {
            rlog("error", &format!("could not save access.json: {e}"));
            return;
        }
        let _ = self.reload.send(()).await;
    }

    /// The `machines` list: the gateway first, then every machine it knows, with where it can be reached
    /// and whether it is connected. Hosts come from the machines themselves — only they can see their tailnet.
    async fn machines_table(self: &Arc<Self>) -> String {
        let hosts = self.hosts.lock().await.clone();
        // The gateway dials nobody, so `reachable_at` has no interface to measure — it answers with
        // this host's own name, which is what an address of ours is known by
        let tailscale = crate::setup::ssh::tailscale_json();
        let (me_host, _) = crate::setup::add_machine::reachable_at(
            tailscale.as_deref(),
            &crate::bridge::Host::name().await,
            crate::setup::ssh::fqdn().as_deref(),
            None,
        );
        let (tailnet, tailnet_ip) =
            crate::setup::add_machine::tailnet_identity(tailscale.as_deref());
        let tunnels = self.tunnels.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let connected = self.links.connected();
        let mut names: Vec<String> = connected.clone();
        for id in self.access().routes.values().filter_map(|r| r.bridge.clone()) {
            if id != self.self_id && !names.contains(&id) {
                names.push(id);
            }
        }
        names.sort();
        names.dedup();

        // **One row per way in.** The gateway has no route to itself, so what it has to say is where
        // machines arrive — and there can be several (an address it holds, a front that hands it on)
        let mut rows: Vec<MachineRow> = Vec::new();
        for e in entrances(
            &std::env::var("AGENTGW_LINK_LISTEN").unwrap_or_default(),
            crate::setup::ssh::tailscale_serve_json().as_deref(),
        ) {
            let (host, port) = e.at.rsplit_once(':').unwrap_or((e.at.as_str(), ""));
            // An address names no interface, so our own name goes beside it. A name that resolves
            // here (the tailnet's) gets its address spelled out instead
            let (name, at) = match host.parse::<std::net::IpAddr>() {
                Ok(_) => (me_host.clone(), e.at.clone()),
                Err(_) if host == tailnet && !tailnet_ip.is_empty() => {
                    (host.to_string(), format!("{tailnet_ip}:{port}"))
                }
                Err(_) => (host.to_string(), e.at.clone()),
            };
            rows.push(MachineRow {
                // Only the first of them carries the name — the rest are the same machine
                id: rows.is_empty().then(|| self.self_id.clone()).unwrap_or_default(),
                host: name,
                ip: at,
                link: match &e.front {
                    None => crate::t!("gateway", "ゲートウェイ"),
                    Some(f) => crate::t!("gateway ({f})", "ゲートウェイ({f})"),
                },
                online: true,
                down: None,
                gateway: true,
            });
        }
        // Nothing open at all (a Bridge that was never given a way in): still say who we are
        if rows.is_empty() {
            rows.push(MachineRow {
                id: self.self_id.clone(),
                host: me_host,
                ip: String::new(),
                link: crate::t!("gateway", "ゲートウェイ"),
                online: true,
                down: None,
                gateway: true,
            });
        }
        for id in &names {
            let (host, ip) = hosts.get(id).cloned().unwrap_or_default();
            // A machine behind an ssh tunnel dials its own loopback, so loopback is the interface it
            // names. What reaches *it* is the gateway's ssh target, which only the gateway knows
            let (host, ip) = match (ip.starts_with("127."), tunnels.get(id)) {
                (true, Some(t)) => {
                    let named = ssh_host_of(&t.target).to_string();
                    let at = address_here(&named).await.unwrap_or_default();
                    (named, at)
                }
                // What this resolver calls the address wins over what the machine calls itself
                _ => (name_here(&ip).await.unwrap_or(host), ip),
            };
            // How the gateway gets to it: a tunnel it opened itself (and knows), or straight there
            let (link, down) = match tunnels.get(id) {
                None => (crate::t!("direct", "直結"), None),
                // **Backticks, or Slack reads `user@host` as an email** and draws a mailto link
                Some(Tunnel { target, error }) => (
                    crate::t!("ssh tunnel (`{target}`)", "ssh トンネル(`{target}`)"),
                    error.clone(),
                ),
            };
            rows.push(MachineRow {
                id: id.clone(),
                host,
                ip,
                link,
                online: connected.iter().any(|c| c == id),
                down,
                gateway: false,
            });
        }
        machines_md(&rows)
    }

    /// The `channels` table. **`machines()` and not `links.connected()`**: the gateway is a machine too,
    /// and leaving itself out drew it as offline.
    async fn channels_table(self: &Arc<Self>, here: &str) -> String {
        let in_slack = match self.api.bot_channels().await {
            Ok(cs) => cs.into_iter().map(|(id, _)| id).collect(),
            Err(e) => {
                rlog("error", &format!("could not list the bot's channels: {e}"));
                Vec::new()
            }
        };
        route_table(
            here,
            &self.access(),
            &self.machines(),
            &self.self_id,
            &self.homes().await,
            &in_slack,
        )
    }

    /// Each machine's home folder, ours included (nobody tells us our own).
    async fn homes(&self) -> HashMap<String, String> {
        let mut homes = self.homes.lock().await.clone();
        homes
            .entry(self.self_id.clone())
            .or_insert_with(StateDir::home);
        homes
    }

    fn owner(&self) -> Option<String> {
        let owner = self.access().owner;
        (!owner.is_empty()).then_some(owner)
    }

    fn home(&self) -> Option<String> {
        self.access().home_channel
    }

    /// Ourselves plus the machines connected right now. **We can be assigned too**, so we're in the list.
    fn machines(&self) -> Vec<String> {
        let mut all = vec![self.self_id.clone()];
        all.extend(self.links.connected());
        all.sort();
        all.dedup();
        all
    }

    /// A link connected. **Put it in the connection book, tell home if it's new, and hand over `Ready`.**
    ///
    /// Dialed by a machine or fetched by us, **the bookkeeping is the same** — only the socket type
    /// and how sending and receiving are written differ from there on.
    async fn attach(
        &self,
        bridge_id: &str,
    ) -> (Conn, tokio::sync::mpsc::UnboundedReceiver<String>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let conn = Conn::new(tx);
        let (joined, displaced) = self.links.register(bridge_id, conn.clone());
        // End the writer of the displaced old link. The registration has already been swapped to the new one
        drop(displaced);
        if joined == Joined::New {
            self.presence.lock().await.on_connect(bridge_id);
        }
        // The first frame on acceptance — the bot token and the current home
        let _ = conn.send(&link::LinkFrame::Ready {
            bot_token: self.bot_token.clone(),
            home: self.home(),
            gateway: Some(self.self_id.clone()),
        });
        (conn, rx)
    }

    /// A link dropped. **A displaced old link doesn't ring presence**
    /// (`unregister` tells by "am I still the registered one").
    async fn detach(&self, bridge_id: &str, conn: &Conn) {
        if self.links.unregister(bridge_id, conn) {
            self.presence
                .lock()
                .await
                .on_disconnect(bridge_id, now_ms());
        }
    }

    /// One line to home. If home isn't set, **don't post; log it** (never drop it silently).
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

    /// What the gateway writes to Slack: saying something can't be delivered, answering its own commands,
    /// and announcing machines joining and leaving. **What the bot actually does is written by each machine.**
    async fn post(&self, channel: &str, thread_ts: Option<&str>, text: &str) {
        if let Err(e) = self
            .api
            .post_message_no_unfurl(channel, text, thread_ts)
            .await
        {
            rlog("error", &format!("could not post to {channel}: {e}"));
        }
    }

// ── Where what comes from Slack gets handed ───────────────────────────

    /// The watcher that picks up presence grace expiries. The design has no timers, so this is the only clock.
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

    /// Take one raw Slack event. **Only the gateway goes through here.**
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

        // What the gateway answers itself (commands and name-claims) comes **before the delivery decision**. Once answered, stop —
        // forwarding `pwd <machine>` would send the instruction to change the destination to the old destination
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
            // This machine's job. Goes through **the same conversion as direct mode** into the same channel
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
                let frame = link::LinkFrame::Event {
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
                // Dropped between looking up the table and sending. **Don't pretend it went.**
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

    /// The single path for "couldn't deliver". **Only to those we may answer, once a minute.**
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

    /// `true` if the gateway answered (= don't deliver).
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

        // ── DM name-claim. Before `pwd <machine>` / `channels` (they assume there's already an Owner)
        if crate::chat::slack::SlackId::is_dm(channel) {
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

        // ── route — answered here and **never delivered** (the destination is the very thing being changed)
        match ctx.route(&machines) {
            RouteOutcome::NotACommand => {}
            // The folder can only be checked where it is. Here: now. Elsewhere: ask, and bind on the answer
            RouteOutcome::SetProject { bridge_id, path } => {
                self.assign_project(channel, &thread, &bridge_id, Some(path)).await;
                return true;
            }
            RouteOutcome::Refused(reply) => {
                rlog(
                    "info",
                    &format!("channels/pwd REFUSED chan={channel} by={user_id:?} — not the Owner"),
                );
                self.post(channel, Some(&thread), &reply).await;
                return true;
            }
            RouteOutcome::List => {
                let table = self.channels_table(channel).await;
                self.post(channel, Some(&thread), &table).await;
                return true;
            }
            RouteOutcome::UnknownBridge(reply) => {
                self.post(channel, Some(&thread), &reply).await;
                return true;
            }
        }

        // ── set-home — saved here, and **the same event goes to every connected machine**
        //    (each passes it through its own gate)
        match ctx.set_home() {
            SetHomeOutcome::NotACommand => {}
            SetHomeOutcome::Set(_) => {
                self.set_home(channel, &thread).await;
                return true;
            }
            SetHomeOutcome::Refused(reply) | SetHomeOutcome::NeedsChannel(reply) => {
                self.post(channel, Some(&thread), &reply).await;
                return true;
            }
        }
        false
    }

    /// Assign a place to a machine and hand that machine "you're in charge".
    /// **Send nothing when assigning ourselves** — we don't phone ourselves.
    /// `pwd <this gateway>:<path>` — the folder is here, so check and store it without asking anyone.
    async fn set_project_here(self: &Arc<Self>, channel: &str, thread_ts: &str, path: &str) {
        let abs = crate::bridge::state::absolute_project_path(path, &StateDir::home());
        let result = if !std::path::Path::new(&abs).is_dir() {
            Err(crate::bridge::state::no_such_folder(&abs, &self.self_id))
        } else {
            let op = crate::bridge::state::AccessOp::SetRepo {
                channel: channel.to_string(),
                path: abs.clone(),
            };
            match self.access().apply(op) {
                Ok((next, ..)) => {
                    self.edit_access(move |a| *a = next).await;
                    Ok(abs)
                }
                Err(e) => Err(e),
            }
        };
        let me = self.self_id.clone();
        self.on_project_set(&me, channel, thread_ts, result).await;
    }

    /// A machine's answer to `pwd <machine>:<path>`. Yes → hand the channel over and say where it works;
    /// no → pass on why (nothing was changed).
    async fn on_project_set(
        self: &Arc<Self>,
        bridge_id: &str,
        channel: &str,
        thread_ts: &str,
        result: Result<String, String>,
    ) {
        if thread_ts.is_empty() {
            // A folder set on the machine itself: record it so `channels` shows where the work happens
            if let Ok(abs) = result {
                let (ch, id) = (channel.to_string(), bridge_id.to_string());
                self.edit_access(move |a| {
                    let r = a.routes.entry(ch).or_default();
                    r.bridge = Some(id);
                    r.repo_path = Some(abs);
                })
                .await;
            }
            return;
        }
        match result {
            Ok(abs) => {
                let mut reply = handover_reply(channel, &self.access().bridges(), bridge_id);
                let (ch, id, folder) = (channel.to_string(), bridge_id.to_string(), abs.clone());
                // Keep the folder here too: `channels` says where each channel's work happens, and only
                // the machine knows the absolute path
                self.edit_access(move |a| {
                    let r = a.routes.entry(ch).or_default();
                    r.bridge = Some(id);
                    r.repo_path = Some(folder);
                })
                .await;
                reply.push('\n');
                reply.push_str(&crate::t!("Project folder: `{abs}`", "作業ディレクトリ: `{abs}`"));
                self.bind_and_link(bridge_id, channel, thread_ts).await;
                self.post(channel, Some(thread_ts), &reply).await;
            }
            Err(why) => {
                rlog("info", &format!("pwd on {bridge_id} for {channel} refused: {why}"));
                self.post(channel, Some(thread_ts), &why).await;
            }
        }
    }

    /// Text a machine sent up its link: its answer to `SetProject`, or a command it was given in one of
    /// its threads (the gateway never saw it — a follow-up in a running thread carries no mention).
    async fn on_machine_text(self: &Arc<Self>, bridge_id: &str, raw: &str) {
        match link::decode(raw) {
            Some(frame) => self.on_machine_frame(bridge_id, frame).await,
            None => rlog("info", &format!("{bridge_id}: dropped an unreadable frame from a machine")),
        }
    }

    pub(crate) async fn on_machine_frame(self: &Arc<Self>, bridge_id: &str, frame: link::LinkFrame) {
        match frame {
            link::LinkFrame::ProjectSet {
                channel,
                thread_ts,
                result,
            } => self.on_project_set(bridge_id, &channel, &thread_ts, result).await,
            link::LinkFrame::Channels {
                channel,
                thread_ts,
            } => {
                let table = self.channels_table(&channel).await;
                self.post(&channel, Some(&thread_ts), &table).await;
            }
            link::LinkFrame::MachineHome { path } => {
                self.homes.lock().await.insert(bridge_id.to_string(), path);
            }
            link::LinkFrame::MachineHost { host, ip } => {
                self.hosts.lock().await.insert(bridge_id.to_string(), (host, ip));
            }
            link::LinkFrame::Machines {
                channel,
                thread_ts,
            } => {
                let table = self.machines_table().await;
                // A table only renders in a `markdown` block — mrkdwn has no table syntax
                if let Err(e) = self
                    .api
                    .post_markdown(&channel, &table, Some(&thread_ts))
                    .await
                {
                    rlog("error", &format!("could not post to {channel}: {e}"));
                }
            }
            link::LinkFrame::SetHome {
                channel,
                thread_ts,
            } => self.set_home(&channel, &thread_ts).await,
            link::LinkFrame::PwdOn {
                channel,
                thread_ts,
                machine,
                path,
            } => self.assign_project(&channel, &thread_ts, &machine, path).await,
            _ => rlog("info", &format!("{bridge_id}: dropped an unexpected frame from a machine")),
        }
    }

    /// The notice channel. **Saved here and pushed to every machine**, so notices from the whole fleet
    /// land in one place; a machine that was away catches up from the `home` in its next handshake.
    async fn set_home(self: &Arc<Self>, channel: &str, thread_ts: &str) {
        let home = channel.to_string();
        self.edit_access(move |a| a.home_channel = Some(home)).await;
        let frame = link::LinkFrame::Home {
            channel: channel.to_string(),
        };
        let delivered = self
            .links
            .connected()
            .iter()
            .filter(|id| self.links.send_to(id, &frame))
            .count();
        rlog(
            "info",
            &format!("set-home chan={channel} — saved and sent to {delivered} machine(s)"),
        );
        let reply = set_home_reply(channel);
        self.post(channel, Some(thread_ts), &reply).await;
    }

    /// `pwd <machine>[:<path>]` — check the machine is here, then let it check the folder.
    /// With no folder named, that machine's home.
    async fn assign_project(
        self: &Arc<Self>,
        channel: &str,
        thread_ts: &str,
        machine: &str,
        path: Option<String>,
    ) {
        let connected = self.machines();
        if !connected.iter().any(|m| m == machine) {
            let notice = unknown_machine(machine, &connected);
            self.post(channel, Some(thread_ts), &notice).await;
            return;
        }
        let path = path.unwrap_or_else(|| "~".to_string());
        if machine == self.self_id {
            self.set_project_here(channel, thread_ts, &path).await;
        } else if !self.links.send_to(
            machine,
            &link::LinkFrame::SetProject {
                channel: channel.to_string(),
                thread_ts: thread_ts.to_string(),
                path,
            },
        ) {
            let notice = Delivery::offline_notice(machine, &self.links.connected());
            self.post(channel, Some(thread_ts), &notice).await;
        }
    }

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
            &link::LinkFrame::Linked {
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
                    .send_to(&bridge_id, &link::LinkFrame::Action { action, body });
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
            // The person who pressed is watching for something to happen. **Swallowing it is the worst.**
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

// ── Gateway→machine dial (only when the gateway is behind NAT) ────────────────────────
//
// **By default the machine dials.** This is only for setups where the gateway sits behind a home router
// or similar and machines can't reach it. Frame direction doesn't change (`Ready` / `Event` / `Action` / `Linked`
// always go gateway→machine), so the only thing that changes is **who places the call**.

/// Keep connecting to one machine. **Never returns.**
///
/// Once connected it's registered in [`LinkServer`], so delivery, presence and the set-home broadcast
/// treat it exactly the same as a link dialed by the machine.
    /// Connect once to each machine listed in `AGENTGW_CHILD_URLS`.
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

    /// Keep connecting to one machine. **Never returns.**
    ///
    /// Once connected it's registered in [`LinkServer`], so delivery, presence and the set-home broadcast
    /// treat it exactly the same as a link dialed by the machine.
    async fn dial_child(self: Arc<Self>, bridge_id: String, url: String) {
        let mut backoff = RECONNECT_MIN_MS;
        loop {
            match self.dial_child_once(&bridge_id, &url).await {
                // The handshake got through. The next reconnect may start from the short wait
                Ok(true) => backoff = RECONNECT_MIN_MS,
                Ok(false) => {}
                Err(why) => {
                    // A refusal that talking won't fix. **Don't fill the log in a loop** — say it once, loudly
                    rlog(
                        "error",
                        &format!("dial {bridge_id}: {why} — not retrying"),
                    );
                    return;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(backoff)).await;
            backoff = (backoff * 2).min(RECONNECT_MAX_MS);
        }
    }

    /// One connection. `Ok(true)` = got through the handshake (and dropped later). `Err` = pointless until the config is fixed.
    async fn dial_child_once(self: &Arc<Self>, bridge_id: &str, url: &str) -> Result<bool, &'static str> {
        use futures_util::SinkExt;
        // **One key.** Whichever side dials, it presents the same `AGENTGW_LINK_TOKEN`
        let target = format!("{url}{}", link::path_for(&self.self_id));
        let request = match link::build_request(&target, &self.token) {
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

        // The machine we fetched can vanish silently too (if its VM suspends, no FIN comes).
        // Without probing, we'd hold a dead machine and keep throwing deliveries away
        let mut watch = IdleWatch::default();
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
                // **Receive only through `beat`.** Waiting on `next()` directly hangs forever on a half-open socket
                incoming = beat(&mut socket, &mut watch) => match incoming {
                    Beat::Text(t) => self.on_machine_text(bridge_id, &t).await,
                    Beat::Alive => {}
                    Beat::Ping => {
                        if socket.send(M::Ping(Default::default())).await.is_err() {
                            break;
                        }
                    }
                    Beat::Gone(why) => {
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

/// Whether to pass the upgrade. **If passed, the name**; if refused or a reachability probe, **the response to return**.
///
/// The gateway (accepting machines) and a machine (accepting the gateway) decide and refuse the same way — only what happens after passing differs.
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
        // Reachability probe — pass the upgrade, but don't put it in the connection book
        Admit::Probe => {
            rlog("debug", "answered a reachability probe — not recorded");
            Err(ws
                .protocols([LINK_SUBPROTOCOL])
                .on_upgrade(|_socket| async {}))
        }
        decision => {
            // **Never refuse silently.** Leave one line in our own log saying what was wrong
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

/// Where machines dial in. `/bridge/{id}`.
/// The address a request came in on, carried from the listener that accepted it.
#[derive(Clone)]
struct Entered(String);

async fn on_upgrade(
    State(fleet): State<Arc<Fleet>>,
    axum::Extension(Entered(at)): axum::Extension<Entered>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    ws: WebSocketUpgrade,
) -> axum::response::Response {
    let (bridge_id, ws) = match admit_upgrade(&headers, &uri, &fleet.token, "a link", ws) {
        Ok(ok) => ok,
        Err(response) => return response,
    };
    // Don't accept our own name. Accepting it would give `pwd <own id>` two destinations
    if bridge_id == fleet.self_id {
        rlog(
            "info",
            &format!("refused a link named \"{bridge_id}\" — that is this machine's own name"),
        );
        return (StatusCode::CONFLICT, "that name is taken by the parent").into_response();
    }
    // Returning the subprotocol in the upgrade response is the convention (strict clients hang up otherwise)
    ws.protocols([LINK_SUBPROTOCOL])
        .on_upgrade(move |socket| on_socket(fleet, bridge_id, at, socket))
}

/// The axum-side reader. It only bridges the difference — the watch clock and state machine live only in `link::beat`.
impl LinkRead for WebSocket {
    async fn read_frame(&mut self) -> Frame {
        match self.recv().await {
            Some(Ok(Message::Text(t))) => Frame::Text(t.to_string()),
            Some(Ok(Message::Close(_))) | None => {
                Frame::Closed("closed by the other side".to_string())
            }
            Some(Ok(_)) => Frame::Other,
            Some(Err(e)) => Frame::Closed(e.to_string()),
        }
    }
}

/// The life of one link (**the side a machine dialed**).
async fn on_socket(fleet: Arc<Fleet>, bridge_id: String, at: String, mut socket: WebSocket) {
    fleet.entrance.lock().await.insert(bridge_id.clone(), at);
    let (conn, mut rx) = fleet.attach(&bridge_id).await;

    // Don't split the socket (don't use futures_util's Sink side). **After the handshake the machine sends
    // nothing**, so this one loop can both "send" and "wait for close".
    //
    // **Don't hold on to a machine that vanished silently.** After the handshake the machine sends nothing,
    // so silence is normal on this connection. Without Ping probes it's indistinguishable from half-open: a missing machine
    // keeps showing as "connected" in `status`, and deliveries to it vanish into thin air
    let mut watch = IdleWatch::default();
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
            // **Receive only through `beat`.** Waiting on `recv()` directly hangs forever on a half-open socket
            incoming = beat(&mut socket, &mut watch) => match incoming {
                Beat::Text(t) => fleet.on_machine_text(&bridge_id, &t).await,
                Beat::Alive => {}
                Beat::Ping => {
                    if socket.send(Message::Ping(Default::default())).await.is_err() {
                        break;
                    }
                }
                Beat::Gone(why) => {
                    rlog("info", &format!("{bridge_id}: link closed ({why})"));
                    break;
                }
            },
        }
    }
    // axum's WebSocket closes on drop
    fleet.detach(&bridge_id, &conn).await;
}

/// Asks the running gateway "who is connected right now". **Only the live process knows.**
/// It binds to loopback only, but a front proxy might forward every path, so it's guarded by the key.
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
    let tunnels = fleet.tunnels.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let entrance = fleet.entrance.lock().await.clone();
    axum::Json(serde_json::json!({
        "connected": fleet.links.connected(),
        "tunnels": tunnels,
        "entrances": entrance,
    }))
    .into_response()
}

/// Open the endpoint that accepts machines. **Never returns.**
///
/// A bind failure doesn't stop the Bridge — its own agents keep running. But no machine can
/// connect, so it's logged loudly as an error (silence would leave the cause of "can't connect" nowhere).
pub async fn serve_children(fleet: Arc<Fleet>, addr: std::net::SocketAddr) {
    let Some(listener) = bind_link_port(addr, "children").await else {
        return;
    };
    rlog("info", &format!("listening on {addr}/bridge/<id>"));
    serve_children_on(fleet, listener).await;
}

/// Open the link port. **Failing to open it doesn't stop the Bridge** — its own agents keep running.
/// But no peer can connect, so it's logged loudly as an error (silence would leave the cause nowhere).
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

/// **Only the three link routes live on this port.** The hook intake (`bridge.rs`) and MCP are on
/// **a different port**, guarded by loopback + token. This one is exposed through a front proxy, so putting
/// them on the same port would let hooks and MCP be hit from outside.
async fn serve_children_on(fleet: Arc<Fleet>, listener: tokio::net::TcpListener) {
    // **Which of our addresses this listener is.** There is one listener per address, so a machine
    // that arrives here arrived on this one — nothing has to be inferred later
    let at = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_default();
    let app = Router::new()
        .route("/status", get(on_status))
        .route(link::PROBE_PATH, get(on_upgrade))
        .route("/bridge/{id}", get(on_upgrade))
        .layer(axum::Extension(Entered(at)))
        .with_state(fleet);
    if let Err(e) = axum::serve(listener, app).await {
        rlog("error", &format!("the link server stopped: {e}"));
    }
}

/// The port on **the machine's loopback side** when using a tunnel.
pub const TUNNEL_PORT: u16 = 8799;

/// The ssh arguments the gateway keeps running. **-N: no command is run.**
///
/// `ExitOnForwardFailure=yes` is required — without it ssh survives even when forwarding fails,
/// leaving a "connected but nothing arrives" state.
pub fn tunnel_ssh_args(target: &str, remote_port: u16, parent_addr: &str) -> Vec<String> {
    tunnel_ssh_args_with(target, remote_port, parent_addr, crate::setup::ssh::gateway_key().exists())
}

/// `key` = the gateway has its own ssh key (left on the machine by `add-machine -p`). **Offer it
/// explicitly**: with only a password on the machine, nothing else gets in, and a tunnel can't ask.
pub fn tunnel_ssh_args_with(
    target: &str,
    remote_port: u16,
    parent_addr: &str,
    key: bool,
) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    if key {
        args.push("-i".into());
        args.push(crate::setup::ssh::gateway_key().to_string_lossy().to_string());
    }
    args.extend(
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
    .map(|s| s.to_string()),
    );
    args
}

/// Keep one machine's ssh tunnel open from inside the gateway's agentgw.
///
/// **Not a separate service.** It only needs watching while agentgw runs, so it's held as a child process (in the OS sense).
///
/// **ssh multiplexing (`ControlMaster auto` + `ControlPersist`) is used as is.** In that case ssh hands the
/// forward to the existing master and exits right away **with 0** (2026-09-18, on a real machine). That's not a failure —
/// the forward lives on inside the master. So on exit 0 we just wait and ask again (if the master is gone,
/// the next ssh becomes the new master and holds the forward). Without multiplexing,
/// ssh stays in the foreground and goes away together with agentgw via `kill_on_drop`.
pub async fn keep_tunnel(
    fleet: Arc<Fleet>,
    child: String,
    target: String,
    parent_addr: String,
) {
    use crate::log::LogCtx;
    let args = tunnel_ssh_args(&target, TUNNEL_PORT, &parent_addr);
    // Log only on state changes (so the once-a-minute re-request doesn't flood plugin-debug.log)
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
            // Keep why it exited. Silently reopening forever hides whether the key is missing or the peer is gone
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
            // Keep the current state on the gateway so `status` can show the route
            fleet.tunnels.lock().unwrap_or_else(|e| e.into_inner()).insert(
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

// ── Section 8: the link watch (both ends read the link through it) ─────────────────

pub const RECONNECT_MIN_MS: u64 = 1_000;
pub const RECONNECT_MAX_MS: u64 = 30_000;

/// After this much silence, send one Ping. If the next window is silent too, treat it as dead and disconnect.
pub const LINK_IDLE_MS: u64 = 30_000;

/// What to do when a silent window closes.
#[derive(Debug, PartialEq, Eq)]
pub enum Idle {
    /// Check that it's alive.
    Ping,
    /// The previous Ping got no answer. Disconnect and reconnect.
    Dead,
}

/// The watchman checking the link isn't dead. **The only way to detect half-open.**
///
/// When the peer vanishes without a FIN (network loss, suspend, a dropped NAT entry), the socket stays
/// `ESTAB` and receiving **never returns**. No Close and no Err arrive, so no receive loop can
/// exit, and the reconnect beyond it is never reached (2026-08-03: a machine went silent for 74 minutes).
/// For WebSocket Ping, both tungstenite and axum **only answer received Pings with Pong** and never
/// send their own. TCP keepalive is off by default too. So the only way is to poke from our side.
#[derive(Default)]
pub struct IdleWatch {
    pinged: bool,
}

impl IdleWatch {
    /// Something arrived / was sent. Alive.
    pub fn on_traffic(&mut self) {
        self.pinged = false;
    }

    /// The window closed with no traffic.
    pub fn on_idle(&mut self) -> Idle {
        if self.pinged {
            Idle::Dead
        } else {
            self.pinged = true;
            Idle::Ping
        }
    }
}

/// The result of reading one frame from the link. Returned by the implementor of [`LinkRead`].
pub enum Frame {
    /// A payload.
    Text(String),
    /// Not a payload (Pong etc.). Its content is irrelevant, but **the fact it arrived** proves liveness.
    Other,
    /// The peer closed / broke. The reason is a line for humans.
    Closed(String),
}

/// A reader of one frame. **No timeout here** — the watchman ([`beat`]) wraps it from outside.
///
/// axum and tungstenite differ only in the name of their read method, so implementations just bridge that.
/// **The watch clock and state machine exist only once, in [`beat`]** (to prevent a repeat of when they were copied to 4 places).
pub trait LinkRead {
    fn read_frame(&mut self) -> impl std::future::Future<Output = Frame> + Send;
}

/// The result of a watched receive.
///
/// **Don't swallow `Ping`.** Ignoring it means never noticing a link whose peer silently vanished
/// (half-open). Exhaustive matching enforces that.
pub enum Beat {
    /// A payload arrived.
    Text(String),
    /// Something other than a payload arrived. Alive.
    Alive,
    /// Silence continued. **The caller sends one Ping and waits for the next.**
    Ping,
    /// Disconnected, with a reason. The caller leaves the loop.
    Gone(String),
}

/// **Every link receive goes through this.** Waiting on a bare `next()` / `recv()` directly
/// never returns when the peer vanishes without a FIN, since neither Close nor Err arrives
/// (2026-08-03: a machine went silent for 74 minutes).
pub async fn beat<S: LinkRead>(socket: &mut S, watch: &mut IdleWatch) -> Beat {
    beat_within(socket, watch, LINK_IDLE_MS).await
}

/// The body of [`beat`]. The window size is passable **only for tests** — callers use `beat`.
async fn beat_within<S: LinkRead>(socket: &mut S, watch: &mut IdleWatch, window_ms: u64) -> Beat {
    match tokio::time::timeout(std::time::Duration::from_millis(window_ms), socket.read_frame()).await {
        Ok(Frame::Text(t)) => {
            watch.on_traffic();
            Beat::Text(t)
        }
        Ok(Frame::Other) => {
            watch.on_traffic();
            Beat::Alive
        }
        Ok(Frame::Closed(why)) => Beat::Gone(why),
        Err(_) => match watch.on_idle() {
            Idle::Ping => Beat::Ping,
            Idle::Dead => Beat::Gone(format!("no reply to ping within {window_ms}ms")),
        },
    }
}

impl<S> LinkRead for tokio_tungstenite::WebSocketStream<S>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send,
{
    async fn read_frame(&mut self) -> Frame {
        use futures_util::StreamExt;
        use tokio_tungstenite::tungstenite::protocol::Message as M;
        match self.next().await {
            Some(Ok(M::Text(t))) => Frame::Text(t.to_string()),
            Some(Ok(M::Close(_))) | None => Frame::Closed("closed by the other side".to_string()),
            Some(Ok(_)) => Frame::Other, // tungstenite handles ping/pong and the like
            Some(Err(e)) => Frame::Closed(e.to_string()),
        }
    }
}

// ── Section 9: CLI (the fleet section of `status`) ─────────────────────────────────

/// The port machines link on, when nothing says otherwise.
pub(crate) const DEFAULT_PORT: &str = "8787";

/// The one address to ask on, out of everything the gateway accepts. **The first is its own way in**
/// (loopback, written first) — and the whole comma-separated list is not a host to put in a URL.
pub(crate) fn first_addr(listen: &str) -> &str {
    listen.split(',').next().unwrap_or(listen).trim()
}

/// **Loopback**, not `0.0.0.0`: a gateway opens a wider address only when `add-machine` measures that a
/// machine needs it.
pub(crate) const DEFAULT_LISTEN: &str = "127.0.0.1:8787";

/// The fleet section of `status`. **The only strings shown to people are here.**
pub struct Cli;

impl Cli {
    pub(crate) fn env_of(dir: &StateDir) -> HashMap<String, String> {
        dir.load_env().unwrap_or_default().into_iter().collect()
    }

    pub(crate) fn write_env(dir: &StateDir, pairs: &[(&str, String)]) -> std::io::Result<()> {
        let path = dir.path().join(".env");
        let before = std::fs::read_to_string(&path).unwrap_or_default();
        let after = crate::state_dir::set_env_keys(&before, pairs);
        crate::state_dir::write_atomic_mode(&path, &after, Some(0o600))
    }

    /// Mint the key machines present. The generator lives in [`crate::state_dir::mint_secret`] — every
    /// secret this Bridge makes comes from the same place.
    fn mint_token() -> String {
        crate::state_dir::mint_secret()
    }

    /// The key shown to machines. **Once made, never changed** — remaking it would lock out every
    /// connected machine at once. The returned `bool` is "just made it".
    pub(crate) fn key_for_invite(existing: Option<&str>) -> (String, bool) {
        match existing {
            Some(t) if !t.trim().is_empty() => (t.trim().to_string(), false),
            _ => (Self::mint_token(), true),
        }
    }

    /// The open ports. Agents connect back here, so it's the first number to check when they can't connect.
    ///
    /// **Nothing is allocated here** — it only reads what's recorded (status adding ports would defeat
    /// the point, and would disagree with the numbers the Bridge holds).
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
            // All we know is there's no record — can't tell "just started" from "not running"
            crate::t!(
                "Local ports: none yet (agentgw just started, or isn't running)",
                "ローカルのポート: まだありません(起動した直後か、動いていません)"
            )
        } else {
            let open = open.join(" / ");
            crate::t!("Local ports: {open}", "ローカルのポート: {open}")
        }
    }

    /// The fleet section of `status` (after the role line, which `main.rs` prints from
    /// [`machine::role_line`](crate::bridge::machine::role_line)). The table is only shown when this host is
    /// set up to accept machines — a lone Bridge doesn't get an empty table.
    pub async fn print_fleet(dir: &StateDir) {
        let env = Self::env_of(dir);
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

    /// Hit the status endpoint. `None` if it doesn't answer (= this endpoint didn't answer, nothing more).
    ///
    /// **Tries 3 times.** `install.sh` shows status right after a restart, so a single try would
    /// misread "the endpoint isn't open yet" as "not running".
    pub(crate) async fn ask_connected(listen: &str, token: &str) -> Option<Vec<String>> {
        Self::ask_status(listen, token).await.map(|r| r.0)
    }

    /// The addresses of ours that machines have actually arrived on. `None` = it didn't answer, which
    /// is **not** the same as "none" — with no answer, nothing may be closed.
    pub(crate) async fn ask_entrances(listen: &str, token: &str) -> Option<Vec<String>> {
        Self::ask_status(listen, token).await.map(|r| r.2)
    }

    /// The answer from the running gateway's `status` endpoint — connected machines, the tunnels the
    /// gateway keeps open, and the addresses of ours machines have come in on.
    async fn ask_status(listen: &str, token: &str) -> Option<(Vec<String>, Tunnels, Vec<String>)> {
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

    async fn ask_status_once(listen: &str, token: &str) -> Option<(Vec<String>, Tunnels, Vec<String>)> {
        let listen = first_addr(listen);
        // Ask curl to avoid a dependency (no HTTP client declared for this one-off job)
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
        // An older gateway doesn't return tunnels — then read it as "all direct"
        let tunnels = v
            .get("tunnels")
            .and_then(|t| serde_json::from_value(t.clone()).ok())
            .unwrap_or_default();
        let mut entrances: Vec<String> = v
            .get("entrances")
            .and_then(|e| e.as_object())
            .map(|e| e.values().filter_map(|a| a.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        entrances.sort();
        entrances.dedup();
        Some((connected, tunnels, entrances))
    }

    /// id → readable name (best-effort). Anything that can't be looked up is shown as the raw id.
    async fn names_of(env: &HashMap<String, String>, view: &FleetView) -> HashMap<String, String> {
        let mut names = HashMap::new();
        let Some(bot) = env.get("SLACK_BOT_TOKEN") else {
            return names;
        };
        let Ok(api) = crate::chat::slack::Api::new(bot) else {
            return names;
        };
        // home is a channel like the routes. Only owner is a person, so it's looked up via users.info
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
            Some(TOKEN),                      // no Bearer
            Some("bearer the-shared-secret"), // spelling is case-sensitive, per the spec
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
            Some("sclink.1"), // before `pwd <machine>:<path>` — would drop SetProject and never answer
            Some("agentgw.0"),
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

    /// A browser-style comma-separated list passes if one of them matches.
    #[test]
    fn a_list_of_subprotocols_is_accepted_when_one_matches() {
        let got = Admit::of(
            "/bridge/desktop",
            Some(&format!("Bearer {TOKEN}")),
            Some("something-else, agentgw.1"),
            TOKEN,
        );
        assert_eq!(got, Admit::Ok("desktop".into()));
    }

    /// The reachability probe **passes but gives no name**. It still has to pass authentication like the rest.
    #[test]
    fn the_probe_path_is_admitted_without_a_name() {
        assert_eq!(ok_admit(link::PROBE_PATH), Admit::Probe);
        assert_eq!(ok_admit(link::PROBE_PATH).status(), None);
        // Authentication isn't skipped
        assert_eq!(
            Admit::of(
                link::PROBE_PATH,
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

    /// **The check order is the spec.** A different version is refused before looking at the token — reporting a version
    /// mismatch as "wrong token" sends people on an investigation that can't fix anything.
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

    /// The name is trusted **only after the token passes**. A peer that hasn't passed doesn't even get its path read.
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

    // ── Connection book ──────────────────────────────────────────────────────────────

    fn conn() -> (Conn, tokio::sync::mpsc::UnboundedReceiver<String>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (Conn::new(tx), rx)
    }

    fn ready() -> link::LinkFrame {
        link::LinkFrame::Ready {
            bot_token: "xoxb-1".into(),
            home: None,
            gateway: Some("vps".into()),
        }
    }

    /// **Regression**: reconnecting under the same name keeps the new one. If this breaks, the machine
    /// goes missing on every reconnect.
    #[test]
    fn a_reconnect_keeps_the_newest_link() {
        let s = LinkServer::new();
        let (old, mut old_rx) = conn();
        let (new, mut new_rx) = conn();
        assert_eq!(s.register("desktop", old.clone()).0, Joined::New);

        let (joined, displaced) = s.register("desktop", new.clone());
        assert_eq!(joined, Joined::Reconnected); // not a new join = not shown in presence
        assert!(displaced.unwrap().is(&old));

        // Even when the displaced old link closes **later**, the new registration stays
        assert!(!s.unregister("desktop", &old));
        assert_eq!(s.connected(), vec!["desktop".to_string()]);

        // Only the new one receives
        assert!(s.send_to("desktop", &ready()));
        assert!(new_rx.try_recv().is_ok());
        assert!(old_rx.try_recv().is_err());
    }

    #[test]
    fn a_real_close_is_reported_once() {
        let s = LinkServer::new();
        let (c, _rx) = conn();
        s.register("desktop", c.clone());
        assert!(s.unregister("desktop", &c)); // really gone
        assert!(!s.unregister("desktop", &c)); // the second time does nothing
        assert!(s.connected().is_empty());
        assert!(!s.is_connected("desktop"));
    }

    #[test]
    fn sending_to_a_machine_that_is_not_here_fails_rather_than_pretending() {
        let s = LinkServer::new();
        assert!(!s.send_to("nobody", &ready()));

        // A link whose receiver is gone doesn't say "delivered" either
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

    // ── Delivery decision ──────────────────────────────────────────────────────────

    fn routes(pairs: &[(&str, &str)]) -> Routes {
        pairs
            .iter()
            .map(|(c, b)| (c.to_string(), b.to_string()))
            .collect()
    }

    /// The same pairs as an `Access` (what the channels table reads). `channel:machine` or
    /// `channel:machine:/folder`.
    fn access_of(pairs: &[(&str, &str)]) -> Access {
        let mut a = Access::default();
        for (ch, spec) in pairs {
            let (machine, path) = match spec.split_once(':') {
                Some((m, p)) => (m, Some(p.to_string())),
                None => (*spec, None),
            };
            let r = a.routes.entry(ch.to_string()).or_default();
            r.bridge = Some(machine.to_string());
            r.repo_path = path;
        }
        a
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
        // Unknown kinds and wrong shapes are "couldn't read"
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
        // A route naming ourselves (`pwd <own id>`) is handled here
        assert_eq!(d(Some("C3")), Delivery::Local);
        // **No route means local**. This is the default for a lone Bridge
        assert_eq!(d(Some("C_NEW")), Delivery::Local);
        assert_eq!(d(None), Delivery::UnknownChannel);
    }

/// A Bridge with no machines has an empty table — **everything is local**. The heart of the regression.
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

    // ── Gateway endpoints, key, order ──────────────────────────

    fn a_fleet() -> (Arc<Fleet>, tokio::sync::mpsc::Receiver<InboundMsg>) {
        let (msg_tx, msg_rx) = tokio::sync::mpsc::channel(4);
        let (click_tx, _click_rx) = tokio::sync::mpsc::channel(4);
        let (reload, _reload_rx) = tokio::sync::mpsc::channel(4);
        let dir = StateDir::at(
            std::env::temp_dir().join(format!("slack-relay-test-{}", std::process::id())),
        );
        let fleet = Arc::new(Fleet {
            homes: Default::default(),
            hosts: Default::default(),
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
            entrance: Default::default(),
        });
        (fleet, msg_rx)
    }

    /// Send one raw HTTP request and read only the status line.
    /// (No HTTP client declared for this one-off job — dependencies stay at seven)
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

    /// **Only the three link routes live on the machines' port.**
    /// The hook intake and MCP are on a different port (loopback + token), while this one is **exposed** through
    /// a front proxy — the day they share a port, hooks and MCP can be hit from outside.
    #[tokio::test]
    async fn the_children_port_carries_the_link_and_nothing_else() {
        let (fleet, _rx) = a_fleet();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve_children_on(fleet, listener));

        // The Bridge's other endpoints aren't here
        for path in ["/hook", "/mcp", "/"] {
            assert!(
                status_line(addr, path).await.contains("404"),
                "{path} がこの口に居る"
            );
        }
        // What's here is status (guarded by the key) and the two link routes
        assert!(status_line(addr, "/status").await.contains("401"));
        assert!(!status_line(addr, "/bridge/desktop").await.contains("404"));
    }

    /// **What we answer is checked before delivery.** Forwarding `pwd <machine>` would send the instruction to change the
    /// destination to the old destination (returning after answering is the caller's structure).
    #[test]
    fn only_a_persons_channel_message_is_a_command_candidate() {
        let owner = Some("U_OWNER");
        let msg = serde_json::json!({"channel": "C1", "user": "U_OWNER", "text": "route"});
        assert!(Event::new("message", &msg).is_a_command_candidate(owner));
        // If the channel can't be read, there's no way to answer
        let nowhere = serde_json::json!({"user": "U_OWNER", "text": "route"});
        assert!(!Event::new("message", &nowhere).is_a_command_candidate(owner));
        // Reactions and joins can't be commands
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
        // Our own posts are ignored (the Owner's Web API posts are the only exception — speaks_as_a_bot)
        let from_bot = serde_json::json!({"channel": "C1", "user": "U_BOT", "bot_id": "B1"});
        assert!(!Event::new("message", &from_bot).is_a_command_candidate(owner));
    }

    /// **The key is never changed once made.** Remaking it locks out every connected machine at once.
    #[test]
    fn the_key_is_minted_once_and_then_kept() {
        let (fresh, minted) = Cli::key_for_invite(None);
        assert!(minted);
        assert_eq!(fresh.len(), 64, "32バイトを16進で: {fresh}");
        assert!(fresh.chars().all(|c| c.is_ascii_hexdigit()));
        // Empty or whitespace-only is the same as "none"
        assert!(Cli::key_for_invite(Some("   ")).1);
        // An existing one comes back **as is**
        assert_eq!(
            Cli::key_for_invite(Some(" kept ")),
            ("kept".to_string(), false)
        );
        // Different every time (never an all-zero secret)
        assert_ne!(Cli::mint_token(), Cli::mint_token());
    }

    /// The same rule is needed **at the command entry too** — `Access::gate` lets the Owner's
    /// Web API posts through as a person, so if we rejected them as a bot here, `pwd <machine>` couldn't be typed.
    #[test]
    fn only_the_owners_web_api_post_counts_as_a_person() {
        let bot_post = |user: &str| serde_json::json!({"user": user, "bot_id": "B1"});
        let owner = Some("U_OWNER".to_string());
        // The Owner themself = treated as a person
        assert!(!Event::new("message", &bot_post("U_OWNER")).speaks_as_a_bot(owner.as_deref()));
        // Any other bot (including our own posts) is dropped
        assert!(Event::new("message", &bot_post("U_BOT")).speaks_as_a_bot(owner.as_deref()));
        let no_user = serde_json::json!({"bot_id": "B1"});
        assert!(Event::new("message", &no_user).speaks_as_a_bot(owner.as_deref()));
        // With no Owner set, everything is a bot (fail-closed)
        assert!(Event::new("message", &bot_post("U_OWNER")).speaks_as_a_bot(None));
        // A person's post stays a person
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

    // ── Notice rate limit ──────────────────────────────────────────────────────────

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
        // The one-minute boundary
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
        // Once reported, the count starts over
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
        assert!(c.take("C2", 0).say); // other channels aren't affected
        assert!(!c.take("C1", 100).say);
    }

    /// Delivery ends the complaint — the first failure after recovery is said right away.
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

    // ── Whom we may refuse ────────────────────────────────────────────────────

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
        // Until we know our own id, nothing in a channel is addressed to us
        assert!(!Event::new("message", &said("<@U_BOT> hi")).may_answer(None));
    }

    /// **Without this, a channel with no machine assigned keeps answering its own refusals with refusals.**
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

    /// A deletion that can't be delivered is dropped silently — there's nothing to resend.
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
        let got = CommandCtx::route(&ctx("C1", "<@U_BOT> pwd desktop", Some(OWNER)), &here());
        // With no folder named, that machine's home — so `pwd` shows `desktop:/home/…`, not "not set"
        assert_eq!(
            got,
            RouteOutcome::SetProject { bridge_id: "desktop".into(), path: "~".into() }
        );
        let reply = handover_reply("C1", &routes(&[]), "desktop");
        assert!(reply.contains("This channel is now handled by *desktop*."));
        assert!(!reply.contains("before")); // no note the first time
    }

    /// **Changing** the machine is said honestly — the conversation can be reread, but the details of the work are gone.
    #[test]
    fn changing_the_owner_of_a_channel_says_what_is_lost() {
        let reply = handover_reply("C1", &routes(&[("C1", "desktop")]), "vps");
        assert!(reply.contains("It was *desktop* before"));
        assert!(reply.contains("it can't see what desktop actually did"));
    }

    /// **Nobody else can use it.** Without this, a shared channel could be hijacked.
    #[test]
    fn only_the_owner_may_route() {
        for user in [Some("U_STRANGER"), None] {
            let got = CommandCtx::route(&ctx("C1", "<@U_BOT> pwd desktop", user), &here());
            assert!(
                matches!(got, RouteOutcome::Refused(_)),
                "{user:?} → {got:?}"
            );
        }
    }

    /// Can't point at a machine that isn't connected — a typo and one not started are treated the same.
    #[test]
    fn a_route_to_a_machine_that_is_not_here_is_refused() {
        let got = CommandCtx::route(&ctx("C1", "<@U_BOT> pwd laptop", Some(OWNER)), &here());
        match got {
            RouteOutcome::UnknownBridge(reply) => {
                assert!(reply.contains("No machine named `laptop` is connected."), "{reply}");
                assert!(reply.contains("Online machines: `desktop`, `vps`"), "{reply}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_bare_route_says_who_handles_this_channel_first() {
        // here() = ["desktop", "vps"] (connected). laptop is named only in routes
        assert_eq!(
            CommandCtx::route(&ctx("C1", "<@U_BOT> channels", Some(OWNER)), &here()),
            RouteOutcome::List
        );
        let reply = route_table(
            "C1",
            &access_of(&[("C1", "desktop"), ("C2", "laptop")]),
            &here(),
            "vps",
            &Default::default(),
            &[],
        );
        // The channel it was typed in comes first
        assert!(
            reply.starts_with("*This channel is mapped to*\n<#C1> → *desktop*\n"),
            "{reply}"
        );
        // Other channels. A channel whose machine isn't here says offline
        assert!(
            reply.contains("- <#C2> → *laptop* 🔴 offline"),
            "{reply}"
        );
        assert!(
            !reply.contains("• <#C1>"),
            "ここは「ほか」に重ねて出さない: {reply}"
        );
        // Machines: the gateway's mark, and machines only in routes (= not here) are shown too
        assert!(
            reply.contains("*Machines*: 🟢 desktop · 🔴 laptop · 🟢 vps (gateway)"),
            "{reply}"
        );
    }

    #[test]
    fn a_channel_without_a_route_says_the_parent_takes_it() {
        let reply = route_table(
            "C9",
            &access_of(&[("C1", "desktop")]),
            &here(),
            "vps",
            &Default::default(),
            &[],
        );
        assert!(
            reply.starts_with("*This channel is mapped to*\n<#C9> → *vps*\n"),
            "{reply}"
        );
        assert!(reply.contains("- <#C1> → *desktop*\n"), "{reply}");
        // Channel rows carry no 🟢 — only the machines line does
        assert_eq!(reply.matches('🟢').count(), 2, "{reply}");
    }

    #[test]
    fn with_no_routes_at_all_everything_goes_to_the_parent() {
        let reply = route_table("C1", &access_of(&[]), &[], "vps", &Default::default(), &[]);
        assert!(reply.contains("the gateway handles every channel"), "{reply}");
    }

    /// **A `pwd <machine>` without a mention isn't even refused.** That would be barging into a conversation the bot isn't part of.
    #[test]
    fn an_unaddressed_route_falls_through_without_a_refusal() {
        let got = CommandCtx::route(&ctx("C1", "pwd desktop", Some(OWNER)), &here());
        assert_eq!(got, RouteOutcome::NotACommand);
    }

    #[test]
    fn a_route_inside_a_sentence_is_a_sentence() {
        for text in [
            "<@U_BOT> please pwd desktop",
            "<@U_BOT> pwd desktop and also vps",
        ] {
            let got = CommandCtx::route(&ctx("C1", text, Some(OWNER)), &here());
            assert_eq!(got, RouteOutcome::NotACommand, "{text}");
        }
    }

    /// In a DM no mention is needed.
    #[test]
    fn in_a_dm_route_needs_no_mention() {
        let got = CommandCtx::route(&ctx("D1", "pwd desktop", Some(OWNER)), &here());
        assert!(matches!(got, RouteOutcome::SetProject { .. }), "{got:?}");
    }

    /// Every row says where the work happens: the channel's own folder, or the machine's home (which it
    /// told us when it connected). A machine we've never heard a home from is named alone.
    #[test]
    fn the_table_shows_a_folder_for_every_channel() {
        let homes: HashMap<String, String> = [("desktop".to_string(), "/home/me".to_string())]
            .into_iter()
            .collect();
        let table = route_table(
            "C1",
            &access_of(&[("C1", "desktop:/srv/app"), ("C2", "desktop"), ("C3", "laptop")]),
            &here(),
            "vps",
            &homes,
            &[],
        );
        assert!(table.contains("<#C1> → `desktop:/srv/app`"), "{table}");
        assert!(table.contains("- <#C2> → `desktop:/home/me`"), "{table}");
        assert!(table.contains("- <#C3> → *laptop* 🔴 offline"), "{table}");
    }

    /// The tunnel is reopened unattended, so it can only get in with a key. When `add-machine -p` left
    /// one behind, offer it by name.
    #[test]
    fn the_tunnel_offers_the_gateways_key_when_there_is_one() {
        let without = tunnel_ssh_args_with("user@build-box", 8799, "127.0.0.1:8787", false);
        assert_eq!(without.first().map(String::as_str), Some("-N"));
        assert!(!without.iter().any(|a| a == "-i"), "{without:?}");

        let with = tunnel_ssh_args_with("user@build-box", 8799, "127.0.0.1:8787", true);
        assert_eq!(with.first().map(String::as_str), Some("-i"));
        assert!(with[1].ends_with("agentgw_ed25519"), "{with:?}");
        assert!(with.contains(&"-N".to_string()) && with.last().unwrap() == "user@build-box", "{with:?}");
    }

    /// A channel the bot is in that nobody was assigned is the gateway's — and the assignments say
    /// nothing about it, so the table takes it from Slack.
    #[test]
    fn the_table_includes_channels_the_bot_is_in() {
        let table = route_table(
            "C1",
            &access_of(&[("C2", "laptop")]),
            &here(),
            "vps",
            &Default::default(),
            &["C1".to_string(), "C_ADMIN".to_string(), "C2".to_string()],
        );
        assert!(table.contains("<#C1> → *vps*"), "{table}");
        assert!(table.contains("- <#C_ADMIN> → *vps*"), "{table}");
        // Not twice, and a channel handed to a machine keeps its machine
        assert_eq!(table.matches("<#C2>").count(), 1, "{table}");
        assert!(table.contains("- <#C2> → *laptop* 🔴 offline"), "{table}");
    }

    /// `pwd <machine>:<path>` asks the machine first — only it can see its folders.
    #[test]
    fn pwd_with_a_machine_and_a_path_asks_that_machine() {
        let got = CommandCtx::route(&ctx("C1", "<@U_BOT> pwd desktop:~/dev/x", Some(OWNER)), &here());
        assert_eq!(
            got,
            RouteOutcome::SetProject { bridge_id: "desktop".into(), path: "~/dev/x".into() }
        );
    }

    /// A path alone goes to the machine running the channel (NotACommand here = forwarded).
    #[test]
    fn pwd_with_only_a_path_is_the_machines_business() {
        for text in ["<@U_BOT> pwd ~/x", "<@U_BOT> pwd /srv/x", "<@U_BOT> pwd ./x", "<@U_BOT> pwd"] {
            let got = CommandCtx::route(&ctx("C1", text, Some(OWNER)), &here());
            assert_eq!(got, RouteOutcome::NotACommand, "{text}");
        }
    }

    /// `channels` and `channel` list; `route` is no command any more (a sentence for the agent).
    #[test]
    fn channels_lists_and_route_is_gone() {
        for text in ["<@U_BOT> channels", "<@U_BOT> channel"] {
            let got = CommandCtx::route(&ctx("C1", text, Some(OWNER)), &here());
            assert_eq!(got, RouteOutcome::List, "{text}");
        }
        for text in ["<@U_BOT> route", "<@U_BOT> route desktop"] {
            let got = CommandCtx::route(&ctx("C1", text, Some(OWNER)), &here());
            assert_eq!(got, RouteOutcome::NotACommand, "{text}");
        }
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

    /// Until the bot knows its own id, no channel command can succeed.
    #[test]
    fn no_channel_command_is_recognised_before_the_bot_knows_its_own_id() {
        let c = CommandCtx {
            channel_id: "C1",
            user_id: Some(OWNER),
            text: "<@U_BOT> pwd desktop",
            owner_user_id: Some(OWNER),
            bot_user_id: None,
        };
        assert_eq!(
            CommandCtx::route(&c, &here()),
            RouteOutcome::NotACommand
        );
    }

    // ── DM name-claim ─────────────────────────────────────────────────────────

    fn conn_string() -> String {
        link::encode_connection(&link::Invite {
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

    /// **With an Owner already there, no re-claiming.** And the secret isn't passed on.
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
        let other = link::encode_connection(&link::Invite {
            url: "wss://relay.example".into(),
            api_token: "a-different-secret".into(),
        });
        assert!(matches!(
            DmOnboardingCtx::decide(&dm(&other, &here(), None)),
            DmOnboarding::BadToken(_)
        ));
        // A torn paste is treated the same
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

    /// A DM with an unknown author (a bot) can't become the Owner.
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
        // **Saying "connected" is the machine's own `online` notice** (with version, pid and warm pool).
        // Don't say the same thing in two places (2026-09-18: fixed after three lines piled up in admin)
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
        assert!(p.due(9_999).is_empty()); // not said twice
    }

    /// **A blink**: back within the grace period says nothing — from the Owner's view it never left.
    #[test]
    fn a_flap_says_nothing_at_all() {
        let mut p = Presence::new();
        p.on_connect("desktop");
        p.on_disconnect("desktop", 1_000);
        p.on_connect("desktop"); // it came back
        assert!(p.due(60_000).is_empty()); // the held 🔴 is gone
    }

    #[test]
    fn a_machine_that_was_never_up_never_drops() {
        let mut p = Presence::new();
        p.on_disconnect("ghost", 0);
        assert!(p.due(60_000).is_empty());
    }

    // ── status display ───────────────────────────────────────────────────────

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
        assert!(out.contains("C2 → laptop  ○ offline"), "{out}"); // the raw id if the name can't be looked up
    }

    #[test]
    fn a_front_is_a_way_in_that_no_address_of_ours_shows() {
        let serve = r#"{
          "Web": {
            "dock.example.ts.net:443": {
              "Handlers": {"/": {"Proxy": "http://127.0.0.1:8787"}}
            },
            "other.example.ts.net:443": {
              "Handlers": {"/": {"Proxy": "http://127.0.0.1:3000"}}
            }
          },
          "Services": {
            "svc:elsewhere": {
              "Web": {
                "elsewhere.example.ts.net:443": {
                  "Handlers": {"/": {"Proxy": "http://127.0.0.1:8787"}}
                }
              }
            }
          }
        }"#;
        let got = entrances("127.0.0.1:8787,203.0.113.10:8787", Some(serve));
        assert_eq!(
            got,
            vec![
                // What we hold comes first — loopback is our own way in, and what a front hands to
                Entrance { at: "127.0.0.1:8787".into(), front: None },
                Entrance { at: "203.0.113.10:8787".into(), front: None },
                // Forwards to an address we hold, so it is a way in to us
                Entrance {
                    at: "dock.example.ts.net:443".into(),
                    front: Some("tailscale serve".into()),
                },
            ],
            // `other` forwards somewhere else, and `Services` is another machine's
        );
        // No tailscale, or nothing served: only what we hold
        assert_eq!(
            entrances("127.0.0.1:8787", None),
            vec![Entrance { at: "127.0.0.1:8787".into(), front: None }]
        );
        assert_eq!(entrances("", None), vec![]);
    }

    #[test]
    fn the_machines_table_keeps_a_broken_tunnel_out_of_the_columns() {
        let row = |id: &str, host: &str, ip: &str, link: &str, online, down: Option<&str>| {
            MachineRow {
                id: id.into(),
                host: host.into(),
                ip: ip.into(),
                link: link.into(),
                online,
                down: down.map(str::to_string),
                gateway: link.starts_with("gateway"),
            }
        };
        let out = machines_md(&[
            row("dock", "dock.lan", "127.0.0.1:8787", "gateway", true, None),
            row("", "dock.example.ts.net", "100.64.0.1:443", "gateway (tailscale serve)", true, None),
            row("pve", "pve.example.ts.net", "100.64.0.9", "direct", true, None),
            row("mac", "", "", "ssh tunnel (me@mac)", false, Some("connection refused")),
        ]);
        // The gateway takes a row per way in, and only the first of them carries the name
        assert!(out.contains("| `dock` | `dock.lan` | `127.0.0.1:8787` | gateway | 🟢 |"), "{out}");
        assert!(
            out.contains("|  | `dock.example.ts.net` | `100.64.0.1:443` | gateway (tailscale serve) | 🟢 |"),
            "{out}"
        );
        assert!(
            out.contains("| `pve` | `pve.example.ts.net` | `100.64.0.9` | direct | 🟢 |"),
            "{out}"
        );
        // Nothing known about where it is, and the reason it is down stays below the table
        assert!(
            out.contains("| `mac` | (not known here) |  | ssh tunnel (me@mac) | 🔴 |"),
            "{out}"
        );
        assert!(!out.contains("| ssh tunnel (me@mac) — "), "{out}");
        assert!(
            out.ends_with("`mac` — the ssh tunnel is down: connection refused"),
            "{out}"
        );
        // Several gateway rows and no machine still means "none yet"
        let alone = machines_md(&[
            row("dock", "dock.lan", "127.0.0.1:8787", "gateway", true, None),
            row("", "dock.example.ts.net", "100.64.0.1:443", "gateway (tailscale serve)", true, None),
        ]);
        assert!(alone.contains("agentgw add-machine user@host"), "{alone}");
    }

    /// Don't mix up **not running** with **not configured** — the route table is shown either way.
    #[test]
    fn an_offline_bridge_still_shows_what_is_configured() {
        let out = format_fleet(&a_view(), None, &Tunnels::new(), &HashMap::new());
        assert!(out.contains("(not answering)"), "{out}");
        assert!(!out.contains("Machines connected"), "{out}");
        assert!(out.contains("● Channels assigned — 2"), "{out}");
        assert!(out.contains("C1 → desktop"), "{out}");
        // No marks when alive/dead is unknown
        assert!(!out.contains("● online"), "{out}");
        assert!(!out.contains("○ offline"), "{out}");
    }

    #[test]
    fn each_child_says_how_it_reaches_the_parent() {
        // The gateway opens the tunnels itself, so it knows which machines use one.
        // Machines not listed connect directly
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

    /// Both come from a real machine: `getent` on the gateway, `host` where there is no getent.
    #[test]
    fn a_reverse_lookup_gives_the_name_this_resolver_uses() {
        assert_eq!(
            name_from_getent("192.0.2.20   build-box.lan build-box\n"),
            Some("build-box.lan".to_string())
        );
        assert_eq!(name_from_getent(""), None);
        assert_eq!(
            name_from_host("20.2.0.192.in-addr.arpa domain name pointer build-box.lan.\n"),
            Some("build-box.lan".to_string())
        );
        // Nothing answered: no name to show, so the machine's own stands
        assert_eq!(name_from_host("Host 192.0.2.20 not found: 3(NXDOMAIN)\n"), None);
    }

    /// A tunnelled machine dials its own loopback, so that is the interface it names — and `127.0.0.1`
    /// tells nobody where it is. The gateway's ssh target does.
    #[test]
    fn an_ssh_target_names_the_host() {
        assert_eq!(ssh_host_of("user@build-box.lan"), "build-box.lan");
        assert_eq!(ssh_host_of("build-box"), "build-box");
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
        assert!(out.contains("● Channels assigned — none"), "{out}");
    }

    #[test]
    fn after_a_real_drop_the_next_drop_is_announced_again() {
        // 🔴 once per disconnect. After it comes back, it can be said again
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

    // ── the link watch ──

    /// Two silent windows mean dead. **Don't disconnect on the first** — cutting a link
    /// that is merely quiet every 30 seconds causes a reconnect storm.
    #[test]
    fn two_silent_windows_mean_the_link_is_dead() {
        let mut w = IdleWatch::default();
        assert_eq!(w.on_idle(), Idle::Ping); // first: poke to check
        assert_eq!(w.on_idle(), Idle::Dead); // second: no answer = dead
    }

    /// A Pong or a delivery — **anything arriving means alive**. The watchman restarts its count there.
    #[test]
    fn any_traffic_clears_the_watch() {
        let mut w = IdleWatch::default();
        assert_eq!(w.on_idle(), Idle::Ping);
        w.on_traffic(); // a Pong came back
        assert_eq!(w.on_idle(), Idle::Ping); // back to the first one
        w.on_traffic();
        w.on_traffic();
        assert_eq!(w.on_idle(), Idle::Ping);
        assert_eq!(w.on_idle(), Idle::Dead);
    }

    /// A socket that returns nothing = the peer silently vanished (half-open).
    struct SilentSocket;
    impl LinkRead for SilentSocket {
        async fn read_frame(&mut self) -> Frame {
            std::future::pending().await // never returns — this is what the 74-minute silence was
        }
    }

    /// A socket that returns a payload only on the second read and is silent otherwise (silent → arrives → silent).
    struct SilentThenText(u32);
    impl LinkRead for SilentThenText {
        async fn read_frame(&mut self) -> Frame {
            self.0 += 1;
            if self.0 == 2 {
                Frame::Text("hello".to_string())
            } else {
                std::future::pending().await
            }
        }
    }

    /// **`beat` never waits forever.** If it stays silent, it asks for a Ping, then says to disconnect in the next window.
    #[tokio::test]
    async fn a_silent_socket_gets_pinged_then_declared_gone() {
        let mut s = SilentSocket;
        let mut w = IdleWatch::default();
        assert!(matches!(beat_within(&mut s, &mut w, 10).await, Beat::Ping));
        let Beat::Gone(why) = beat_within(&mut s, &mut w, 10).await else {
            panic!("2窓目は Gone のはず");
        };
        assert!(why.contains("no reply to ping"), "{why}");
    }

    /// When something arrives, return the payload and **restart the watchman's count** — the key to not cutting a live link.
    /// Without the reset, a single silent window right after a Ping would disconnect.
    #[tokio::test]
    async fn a_frame_arrives_and_resets_the_watch() {
        let mut s = SilentThenText(0);
        let mut w = IdleWatch::default();
        // First window is silent → ask for a Ping
        assert!(matches!(beat_within(&mut s, &mut w, 10).await, Beat::Ping));
        // Then a payload arrives (a Pong works the same) → alive, so restart the count
        match beat_within(&mut s, &mut w, 10).await {
            Beat::Text(t) => assert_eq!(t, "hello"),
            _ => panic!("本文が来るはず"),
        }
        // **The count was reset, so it's Ping again.** Gone here would mean the reset didn't happen
        assert!(matches!(beat_within(&mut s, &mut w, 10).await, Beat::Ping));
        assert!(matches!(
            beat_within(&mut s, &mut w, 10).await,
            Beat::Gone(_)
        ));
    }
}
