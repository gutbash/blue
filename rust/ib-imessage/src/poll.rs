//! Native iOS 26 Messages polls, inbound only.
//!
//! A poll is an `MSMessage` extension balloon from the Polls app. Its balloon
//! bundle id ends in `com.apple.messages.Polls` (Messages stores the full
//! `com.apple.messages.MSMessageExtensionBalloonPlugin:0000000000:com.apple.messages.Polls`).
//! The balloon's `URL` is `data:,<base64 JSON>[?src=p&c=<n>]`:
//!
//! - creation: `{"item":{"title":…,"creatorHandle":…,"orderedPollOptions":
//!   [{"optionIdentifier":<UUID>,"text":…,"creatorHandle":…,…}]},"version":1}`,
//!   carried by an ordinary message (`Message::Message` with `app`).
//! - vote: `{"item":{"votes":[{"voteOptionIdentifier":<UUID>,
//!   "participantHandle":…,"serverVoteTime"?:…}]},"version":1}`, carried as an
//!   associated message of type 4000 on the poll (rustpush:
//!   `ReactMessageType::Extension { is_meta: true }`, `to_uuid` = poll guid).
//!   A vote payload is the voter's full current selection, not a delta.
//! - an associated type-2 update carrying `orderedPollOptions` is a poll whose
//!   options changed (an option added); it is re-emitted as `poll` with the
//!   original poll's guid.
//!
//! Sources: ReagentX/imessage-exporter `imessage-database/src/message_types/polls.rs`
//! and its PollCreate/PollVote fixtures; openclaw/imsg `MessagePollDecoder.swift`
//! and `PollPayload.inc`; photon-hq/advanced-imessage-kit `lib/poll-utils.ts`.
//! All three read macOS chat.db / IMCore, not the APNs wire. That the APNs `bp`
//! balloon carries the same keyed archive is inferred from how rustpush
//! handles every other balloon. [VERIFY against a captured poll]

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use base64::{engine::general_purpose, Engine as _};
use rustpush::{ExtensionApp, Message, MessageInst, ReactMessageType};
use serde_json::{json, Value as Json};

pub const POLLS_BUNDLE_SUFFIX: &str = "com.apple.messages.Polls";

/// Option id -> text, per poll guid, so a vote can name the option. Votes on a
/// poll that arrived before this helper started carry only the option id.
static OPTIONS: LazyLock<Mutex<HashMap<String, Vec<(String, String)>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn is_polls_bundle(bid: &str) -> bool {
    bid == POLLS_BUNDLE_SUFFIX || bid.rsplit(':').next() == Some(POLLS_BUNDLE_SUFFIX)
}

/// Decode the JSON inside a Polls `data:,` URL.
pub fn decode_url(url: &str) -> Option<Json> {
    let rest = url.strip_prefix("data:,")?;
    let b64 = rest.split_once('?').map_or(rest, |(b, _)| b);
    let b64 = percent_decode(b64);
    let engines = [
        &general_purpose::STANDARD,
        &general_purpose::URL_SAFE,
        &general_purpose::STANDARD_NO_PAD,
        &general_purpose::URL_SAFE_NO_PAD,
    ];
    let bytes = engines.iter().find_map(|e| e.decode(b64.trim()).ok())?;
    serde_json::from_slice(&bytes).ok()
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("");
            if let Ok(v) = u8::from_str_radix(hex, 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The poll events implied by one inbound message. Empty for anything that is
/// not a Polls balloon. The caller still publishes the raw `message` event.
pub fn events_for(inst: &MessageInst) -> Vec<Json> {
    let chat = serde_json::to_value(&inst.conversation).unwrap_or(Json::Null);
    let (app, assoc_guid): (&ExtensionApp, Option<&str>) = match &inst.message {
        Message::Message(m) => match &m.app {
            Some(app) => (app, None),
            None => return vec![],
        },
        Message::React(r) => match &r.reaction {
            ReactMessageType::Extension { spec, .. } => (spec, Some(r.to_uuid.as_str())),
            _ => return vec![],
        },
        _ => return vec![],
    };
    // `ExtensionApp.balloon` is `#[serde(skip)]` in rustpush, so the raw
    // `message` event never carries it. Log it here so a single real capture
    // is enough to see what arrived.
    if let Some(b) = &app.balloon {
        let url: String = b.url.chars().take(8192).collect();
        log::info!(
            "extension balloon on {} (assoc {:?}): bid={} name={} session={:?} live={} url={}",
            inst.id, assoc_guid, app.bundle_id, app.name, b.session, b.is_live, url
        );
    } else {
        log::info!("extension on {} (assoc {:?}): bid={} with no balloon", inst.id, assoc_guid, app.bundle_id);
    }
    if !is_polls_bundle(&app.bundle_id) {
        return vec![];
    }
    let Some(balloon) = &app.balloon else {
        log::warn!("poll {} has no balloon payload", inst.id);
        return vec![];
    };
    let Some(payload) = decode_url(&balloon.url) else {
        log::warn!("poll {}: could not decode balloon URL", inst.id);
        return vec![];
    };
    from_payload(
        &payload,
        assoc_guid.unwrap_or(&inst.id),
        inst.sender.as_deref(),
        chat,
    )
}

/// Pure part of `events_for`, testable without a `MessageInst`.
pub fn from_payload(payload: &Json, poll_guid: &str, sender: Option<&str>, chat: Json) -> Vec<Json> {
    let item = &payload["item"];
    if let Some(opts) = item["orderedPollOptions"].as_array() {
        let pairs: Vec<(String, String)> = opts
            .iter()
            .map(|o| {
                (
                    o["optionIdentifier"].as_str().unwrap_or_default().to_string(),
                    o["text"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect();
        if let Ok(mut m) = OPTIONS.lock() {
            m.insert(poll_guid.to_string(), pairs.clone());
        }
        return vec![json!({
            "event": "poll",
            "guid": poll_guid,
            "chat": chat,
            "question": item["title"].as_str().unwrap_or_default(),
            "creator": item["creatorHandle"].as_str().or(sender),
            "options": pairs.iter().map(|(_, t)| t.clone()).collect::<Vec<_>>(),
            "option_ids": pairs.iter().map(|(i, _)| i.clone()).collect::<Vec<_>>(),
        })];
    }
    let Some(votes) = item["votes"].as_array() else {
        return vec![];
    };
    let known = OPTIONS.lock().ok().and_then(|m| m.get(poll_guid).cloned());
    let text_of = |id: &str| -> Option<String> {
        known.as_ref()?.iter().find(|(i, _)| i == id).map(|(_, t)| t.clone())
    };
    let selected: Vec<Json> = votes
        .iter()
        .map(|v| {
            let id = v["voteOptionIdentifier"].as_str().unwrap_or_default();
            json!({"option_id": id, "option": text_of(id)})
        })
        .collect();
    if votes.is_empty() {
        return vec![json!({
            "event": "poll_vote", "poll_guid": poll_guid, "chat": chat,
            "voter": sender, "option": null, "option_id": null, "selected": [],
        })];
    }
    votes
        .iter()
        .map(|v| {
            let id = v["voteOptionIdentifier"].as_str().unwrap_or_default();
            let voter = v["participantHandle"].as_str().or(sender);
            json!({
                "event": "poll_vote",
                "poll_guid": poll_guid,
                "chat": chat,
                "voter": voter,
                "option": text_of(id).unwrap_or_else(|| id.to_string()),
                "option_id": id,
                "option_known": text_of(id).is_some(),
                "selected": selected,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // From ReagentX/imessage-exporter test_data/app_message/PollCreate.plist.
    const CREATE: &str = "data:,eyJpdGVtIjp7InRpdGxlIjoiIiwiY3JlYXRvckhhbmRsZSI6IisxMjIyMzMzNDQ0NCIsIm9yZGVyZWRQb2xsT3B0aW9ucyI6W3sib3B0aW9uSWRlbnRpZmllciI6IjYzMENEMEY4LTk1MDItNDBENC1CMkUxLUM1M0UwMkZENDM1RSIsImNyZWF0b3JIYW5kbGUiOiJzdGV2ZW5AYXBwbGVuZXQuY29tIiwidGV4dCI6Ik9wdGlvbiAxIiwiY2FuQmVFZGl0ZWQiOmZhbHNlLCJhdHRyaWJ1dGVkVGV4dCI6Ik9wdGlvbiAxIn0seyJvcHRpb25JZGVudGlmaWVyIjoiNThCNTlENzQtRUY3My00QkE3LUFBOEUtNUEzMEZCQUYzRjQ0IiwiY3JlYXRvckhhbmRsZSI6InN0ZXZlbkBhcHBsZW5ldC5jb20iLCJ0ZXh0IjoiT3B0aW9uIDIiLCJjYW5CZUVkaXRlZCI6ZmFsc2UsImF0dHJpYnV0ZWRUZXh0IjoiT3B0aW9uIDIifSx7Im9wdGlvbklkZW50aWZpZXIiOiJENDkzRERDOC02RUUyLTQwOEYtODg4Qy0zOTg2ODM4QTFGQkIiLCJjcmVhdG9ySGFuZGxlIjoic3RldmVuQGFwcGxlbmV0LmNvbSIsInRleHQiOiJPcHRpb24gMyIsImNhbkJlRWRpdGVkIjpmYWxzZSwiYXR0cmlidXRlZFRleHQiOiJPcHRpb24gMyJ9LHsib3B0aW9uSWRlbnRpZmllciI6IjU0MURDQTZELTY4NkEtNEZFQi05MDkyLTAwQkE0QTQ1RTJBNCIsImNyZWF0b3JIYW5kbGUiOiIrMTIyMjMzMzQ0NDQiLCJ0ZXh0IjoiT3B0aW9uIDQiLCJjYW5CZUVkaXRlZCI6ZmFsc2UsImF0dHJpYnV0ZWRUZXh0IjoiT3B0aW9uIDQifV19LCJ2ZXJzaW9uIjoxfQ==?src=p&c=4";
    // From PollVote.plist (a different capture; same option ids).
    const VOTE: &str = "data:,eyJpdGVtIjp7InZvdGVzIjpbeyJ2b3RlT3B0aW9uSWRlbnRpZmllciI6IjU4QjU5RDc0LUVGNzMtNEJBNy1BQThFLTVBMzBGQkFGM0Y0NCIsInBhcnRpY2lwYW50SGFuZGxlIjoiKzEyMjIzMzM0NDQ0In1dfSwidmVyc2lvbiI6MX0=";

    #[test]
    fn bundle_ids() {
        assert!(is_polls_bundle("com.apple.messages.Polls"));
        assert!(is_polls_bundle(
            "com.apple.messages.MSMessageExtensionBalloonPlugin:0000000000:com.apple.messages.Polls"
        ));
        assert!(!is_polls_bundle("com.apple.messages.URLBalloonProvider"));
    }

    #[test]
    fn creation_then_vote() {
        let ev = from_payload(&decode_url(CREATE).unwrap(), "POLL-1", Some("tel:+1"), Json::Null);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0]["event"], "poll");
        assert_eq!(ev[0]["options"], json!(["Option 1", "Option 2", "Option 3", "Option 4"]));
        assert_eq!(ev[0]["creator"], "+12223334444");

        let v = from_payload(&decode_url(VOTE).unwrap(), "POLL-1", Some("tel:+1"), Json::Null);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0]["event"], "poll_vote");
        assert_eq!(v[0]["poll_guid"], "POLL-1");
        assert_eq!(v[0]["voter"], "+12223334444");
        assert_eq!(v[0]["option"], "Option 2");
        assert_eq!(v[0]["option_known"], true);
    }

    #[test]
    fn vote_on_unknown_poll_keeps_id() {
        let v = from_payload(&decode_url(VOTE).unwrap(), "UNSEEN", None, Json::Null);
        assert_eq!(v[0]["option"], "58B59D74-EF73-4BA7-AA8E-5A30FBAF3F44");
        assert_eq!(v[0]["option_known"], false);
    }

    #[test]
    fn non_poll_url() {
        assert!(decode_url("https://example.com").is_none());
    }
}
