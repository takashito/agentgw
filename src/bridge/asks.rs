//! Asks between channels: an agent in one thread asks the agent in another channel for something.
//!
//! The gateway decides everything (whether a follow-up may go through, who to tell when a person is
//! needed); this module holds those decisions as pure functions so they test without Slack or links.

pub use crate::bridge::state::{AskState, AskedFrom};

/// What one delivery between agents is: a request to the asked thread, or something going back to
/// the asking thread (its reply, or a notice that no reply is coming).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryKind {
    Ask,
    Reply,
    Notice,
}

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
