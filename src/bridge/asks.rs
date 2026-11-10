//! Asks between channels: an agent in one thread asks the agent in another channel for something.
//!
//! The gateway decides everything (whether a follow-up may go through, who to tell when a person is
//! needed); this module holds those decisions as pure functions so they test without Slack or links.

pub use crate::bridge::state::{AskState, AskedFrom};

pub use crate::chat::DeliveryKind;

/// One message from an agent for an agent, as the gateway hands it to the machine that runs the thread.
///
/// `channel` / `thread_ts` / `ts` are where it lands and the real Slack post it was shown as, so the
/// ledger and dedup work on it as on any message. `origin_*` is the other end of the exchange.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AgentDelivery {
    pub channel: String,
    pub thread_ts: String,
    pub ts: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_channel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_thread: Option<String>,
    pub kind: DeliveryKind,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub needs_human: bool,
}

impl From<AgentDelivery> for crate::chat::InboundMsg {
    fn from(d: AgentDelivery) -> Self {
        crate::chat::InboundMsg {
            channel: d.channel,
            channel_kind: crate::chat::ChannelKind::Channel,
            ts: d.ts,
            thread_ts: Some(d.thread_ts),
            user: None,
            is_bot: false,
            bot_id: None,
            text: d.text,
            files: Vec::new(),
            file_paths: Vec::new(),
            file_errors: Vec::new(),
            reaction: None,
            deleted_ts: None,
            edited: None,
            session_title: None,
            from_agent: Some(crate::chat::AgentOrigin {
                channel: d.origin_channel,
                thread_ts: d.origin_thread,
                kind: d.kind,
                needs_human: d.needs_human,
            }),
        }
    }
}

/// Whether a follow-up from `from_channel`/`from_thread` may go into the asked thread whose record is
/// `row`. The refusals are for the asking agent (a tool error), not shown to people.
pub fn check_follow_up(
    row: Option<&AskedFrom>,
    from_channel: &str,
    from_thread: &str,
    target_online: bool,
) -> Result<(), String> {
    let Some(row) = row.filter(|r| r.channel == from_channel && r.thread_ts == from_thread) else {
        return Err("no ask from this thread goes to that thread".into());
    };
    match row.state {
        AskState::Pending => Err("still waiting for the Owner to press Run".into()),
        AskState::Ended => Err("a person ended this exchange; ask again in a new thread".into()),
        AskState::Approved if !target_online => Err("the machine for that channel is offline".into()),
        AskState::Approved => Ok(()),
    }
}

/// Whether a new ask may go to `to_channel`.
pub fn check_new(
    to_channel: &str,
    from_channel: &str,
    from_thread: &str,
    to_thread: Option<&str>,
) -> Result<(), String> {
    if to_channel.starts_with('D') {
        return Err("asks cannot go to a DM".into());
    }
    if to_channel == from_channel && to_thread == Some(from_thread) {
        return Err("cannot ask your own thread".into());
    }
    Ok(())
}

/// Where to post "a person is needed" for the asked thread `thread_ts`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotifyTarget {
    pub channel: String,
    pub thread_ts: String,
    pub mention_owner: bool,
}

/// The thread that asked, and the top of the chain (where a person gave the first instruction).
/// One target when they are the same. Only the top mentions the Owner.
///
/// `rows` maps an asked thread's ts to (its channel, its record).
pub fn notify_targets(
    rows: &std::collections::HashMap<String, (String, AskedFrom)>,
    thread_ts: &str,
) -> Vec<NotifyTarget> {
    let Some((_, first)) = rows.get(thread_ts) else {
        return Vec::new();
    };
    let asker = (first.channel.clone(), first.thread_ts.clone());
    // Each thread's record is written once, when it is asked, so the chain cannot loop; the visited
    // set only keeps a broken record from spinning
    let mut top = asker.clone();
    let mut seen = std::collections::HashSet::from([thread_ts.to_string()]);
    while let Some((_, up)) = rows.get(&top.1) {
        if !seen.insert(top.1.clone()) {
            break;
        }
        top = (up.channel.clone(), up.thread_ts.clone());
    }
    let mut out = Vec::new();
    if asker != top {
        out.push(NotifyTarget {
            channel: asker.0,
            thread_ts: asker.1,
            mention_owner: false,
        });
    }
    out.push(NotifyTarget {
        channel: top.0,
        thread_ts: top.1,
        mention_owner: true,
    });
    out
}

/// What the Run / Reject buttons carry, so a press works across a restart with no table in memory.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AskOffer {
    pub from_channel: String,
    pub from_thread: String,
    pub to_channel: String,
    /// The first post of the request (the asked thread's root).
    pub request_ts: String,
}

impl AskOffer {
    pub fn encode(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn decode(value: &str) -> Option<Self> {
        serde_json::from_str(value).ok()
    }
}

#[cfg(test)]
mod frame_tests {
    use crate::bridge::gateway::link::{decode, encode, LinkFrame};

    #[test]
    fn ask_frames_roundtrip() {
        for f in [
            LinkFrame::AskChannel {
                req_id: "r1".into(),
                from_channel: "C1".into(),
                from_thread: "1.1".into(),
                to_channel: "C2".into(),
                to_thread: None,
                text: "exit".into(),
            },
            LinkFrame::AskAnswer { req_id: "r1".into(), result: Err("no".into()) },
            LinkFrame::AgentReply {
                channel: "C2".into(),
                thread_ts: "2.2".into(),
                ts: "2.3".into(),
                text: "done".into(),
                needs_human: true,
                outcome: "reply".into(),
            },
        ] {
            assert_eq!(decode(&encode(&f)), Some(f));
        }
    }

    #[test]
    fn a_machine_never_takes_a_frame_meant_for_the_gateway() {
        let f = LinkFrame::NeedsHuman { channel: "C".into(), thread_ts: "1".into(), ts: "2".into() };
        assert!(crate::bridge::machine::FromRelay::of(f).is_none());
    }

    #[test]
    fn an_agent_message_reaches_the_machine_as_a_delivery() {
        let d = super::AgentDelivery {
            channel: "C2".into(),
            thread_ts: "2.1".into(),
            ts: "2.1".into(),
            text: "hi".into(),
            origin_channel: Some("C1".into()),
            origin_thread: Some("1.1".into()),
            kind: super::DeliveryKind::Ask,
            needs_human: false,
        };
        let f = LinkFrame::AgentMessage { msg: serde_json::to_value(&d).unwrap() };
        match crate::bridge::machine::FromRelay::of(f) {
            Some(crate::bridge::machine::FromRelay::AgentMessage(got)) => assert_eq!(got, d),
            _ => panic!("not delivered as an agent message"),
        }
    }
}

#[cfg(test)]
mod decision_tests {
    use super::*;
    use std::collections::HashMap;

    fn row(ch: &str, th: &str, s: AskState) -> AskedFrom {
        AskedFrom {
            channel: ch.into(),
            thread_ts: th.into(),
            state: s,
            outstanding: false,
            missed: 0,
            origin_closed: false,
            request: None,
        }
    }

    #[test]
    fn follow_ups_need_an_approved_link_from_the_same_thread() {
        let ok = row("C1", "1.1", AskState::Approved);
        assert!(check_follow_up(Some(&ok), "C1", "1.1", true).is_ok());
        assert!(check_follow_up(None, "C1", "1.1", true).is_err());
        assert!(check_follow_up(Some(&ok), "C1", "9.9", true).is_err());
        let pending = row("C1", "1.1", AskState::Pending);
        assert!(check_follow_up(Some(&pending), "C1", "1.1", true).unwrap_err().contains("Run"));
        let ended = row("C1", "1.1", AskState::Ended);
        assert!(check_follow_up(Some(&ended), "C1", "1.1", true).unwrap_err().contains("new thread"));
        assert!(check_follow_up(Some(&ok), "C1", "1.1", false).unwrap_err().contains("offline"));
    }

    #[test]
    fn new_asks_refuse_dms_and_the_same_thread() {
        assert!(check_new("D1", "C1", "1.1", None).is_err());
        assert!(check_new("C1", "C1", "1.1", Some("1.1")).is_err());
        assert!(check_new("C2", "C1", "1.1", None).is_ok());
    }

    #[test]
    fn a_three_level_chain_tells_the_asker_and_the_top_and_mentions_only_the_top() {
        // CoS(C0/0.1) → Lead(C1/1.1) → project(C2/2.1)
        let mut rows = HashMap::new();
        rows.insert("1.1".to_string(), ("C1".to_string(), row("C0", "0.1", AskState::Approved)));
        rows.insert("2.1".to_string(), ("C2".to_string(), row("C1", "1.1", AskState::Approved)));
        assert_eq!(
            notify_targets(&rows, "2.1"),
            vec![
                NotifyTarget { channel: "C1".into(), thread_ts: "1.1".into(), mention_owner: false },
                NotifyTarget { channel: "C0".into(), thread_ts: "0.1".into(), mention_owner: true },
            ]
        );
    }

    #[test]
    fn one_level_sends_one_notice_with_the_mention() {
        let mut rows = HashMap::new();
        rows.insert("1.1".to_string(), ("C1".to_string(), row("C0", "0.1", AskState::Approved)));
        assert_eq!(
            notify_targets(&rows, "1.1"),
            vec![NotifyTarget { channel: "C0".into(), thread_ts: "0.1".into(), mention_owner: true }]
        );
    }

    #[test]
    fn a_thread_nobody_asked_has_nobody_to_tell() {
        assert!(notify_targets(&HashMap::new(), "1.1").is_empty());
    }

    #[test]
    fn a_broken_record_that_loops_still_ends() {
        let mut rows = HashMap::new();
        rows.insert("1.1".to_string(), ("C1".to_string(), row("C2", "2.1", AskState::Approved)));
        rows.insert("2.1".to_string(), ("C2".to_string(), row("C1", "1.1", AskState::Approved)));
        assert!(notify_targets(&rows, "1.1").len() <= 2);
    }

    #[test]
    fn an_ask_offer_survives_the_button_value() {
        let o = AskOffer {
            from_channel: "C1".into(),
            from_thread: "1.1".into(),
            to_channel: "C2".into(),
            request_ts: "2.1".into(),
        };
        assert_eq!(AskOffer::decode(&o.encode()), Some(o.clone()));
        assert!(crate::bridge::state::FolderOffer::decode(&o.encode()).is_none(), "not mistaken for a folder offer");
    }
}
