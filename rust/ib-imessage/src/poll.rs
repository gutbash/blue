//! Native iOS 26 Messages polls: decoding inbound polls and votes, and
//! building outbound ones (`build_poll`, `build_vote`).
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
use rustpush::{
    Balloon, BalloonLayout, ExtensionApp, IndexedMessagePart, Message, MessageInst, MessagePart,
    MessageParts, MessageType, NSDictionaryClass, NormalMessage, ReactMessage, ReactMessageType,
};
use serde_json::{json, Value as Json};

pub const POLLS_BUNDLE_SUFFIX: &str = "com.apple.messages.Polls";

/// Option id -> text, per poll guid, so a vote can name the option. Votes on a
/// poll that arrived before this helper started carry only the option id.
static OPTIONS: LazyLock<Mutex<HashMap<String, Vec<(String, String)>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Poll guid (uppercase) -> balloon session id. A vote must carry its poll's
/// session; the capture shows every vote reusing the poll's.
static SESSIONS: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Poll guid (uppercase) -> newest full poll JSON (creation or option update),
/// the base an added option extends.
static PAYLOADS: LazyLock<Mutex<HashMap<String, Json>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn payload_of(poll_guid: &str) -> Option<Json> {
    PAYLOADS.lock().ok()?.get(&poll_guid.to_uppercase()).cloned()
}

pub fn session_of(poll_guid: &str) -> Option<String> {
    SESSIONS.lock().ok()?.get(&poll_guid.to_uppercase()).cloned()
}

fn remember(poll_guid: &str, session: Option<&str>, options: Option<Vec<(String, String)>>) {
    if let (Some(s), Ok(mut m)) = (session, SESSIONS.lock()) {
        m.entry(poll_guid.to_uppercase()).or_insert_with(|| s.to_string());
    }
    if let (Some(o), Ok(mut m)) = (options, OPTIONS.lock()) {
        m.insert(poll_guid.to_string(), o);
    }
}

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
        // The rest of the envelope, so one capture establishes how iOS sends
        // a poll update (associated type, layout, icon, capabilities, body).
        let (assoc_type, to_part, body) = match &inst.message {
            Message::React(r) => match &r.reaction {
                ReactMessageType::Extension { is_meta, body, .. } => (
                    Some(if *is_meta { 4000 } else { 2 }),
                    r.to_part,
                    serde_json::to_string(body).unwrap_or_default(),
                ),
                _ => (None, r.to_part, String::new()),
            },
            Message::Message(m) => (None, None, format!("xml={:?}", m.xml)),
            _ => (None, None, String::new()),
        };
        log::info!(
            "extension envelope on {}: assoc_type={:?} to_part={:?} layout={} icon_bytes={:?} \
             caps_b64={:?} send_as_text={:?} ld_text={:?} adam_id={:?} body={}",
            inst.id,
            assoc_type,
            to_part,
            b.layout
                .as_ref()
                .map(|l| plist_debug(l))
                .unwrap_or_else(|| "none".into()),
            b.icon.as_ref().map(|i| i.len()),
            b.required_capabilities.as_ref().map(|c| general_purpose::STANDARD.encode(c)),
            b.send_as_text,
            b.ld_text,
            app.app_id,
            body.chars().take(2048).collect::<String>(),
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
    remember(assoc_guid.unwrap_or(&inst.id), balloon.session.as_deref(), None);
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
        if let Ok(mut m) = PAYLOADS.lock() {
            m.insert(poll_guid.to_uppercase(), payload.clone());
        }
        return vec![json!({
            "event": "poll",
            "guid": poll_guid,
            "sender": sender,
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
            "voter": sender, "sender": sender, "option": null, "option_id": null, "selected": [],
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
                "sender": sender,
                "option": text_of(id).unwrap_or_else(|| id.to_string()),
                "option_id": id,
                "option_known": text_of(id).is_some(),
                "selected": selected,
            })
        })
        .collect()
}

// ---------------------------------------------------------------- outbound
//
// Ported from sbaresearch/transcript-consistency (main):
// - code/imessage/src/imessage/messages.rs: constants L56-61, payload structs
//   L76-124, build_poll_payload L126-162, build_poll_extension_with_meta
//   L164-202, build_poll_vote_extension L271-310, NormalMessage::poll_with_meta
//   L1311-1341.
// - code/imessage/imessage-group/src/common.rs: send_poll_message L2119-2198,
//   send_poll_vote_message L2331-2417.
// The fork carries the question only as `item.title`; it sends no separate
// caption message.

/// Full balloon bundle id, as the fork sends and as iOS stores it.
pub const POLLS_BUNDLE_ID: &str =
    "com.apple.messages.MSMessageExtensionBalloonPlugin:0000000000:com.apple.messages.Polls";
const POLLS_APP_NAME: &str = "Polls";
const POLLS_BREADCRUMB: &str = "Sent a poll";
/// Polls app icon (JPEG), verbatim from the fork.
const POLLS_ICON_B64: &str = "/9j/4AAQSkZJRgABAQAAkACQAAD/4QCMRXhpZgAATU0AKgAAAAgABQESAAMAAAABAAEAAAEaAAUAAAABAAAASgEbAAUAAAABAAAAUgEoAAMAAAABAAIAAIdpAAQAAAABAAAAWgAAAAAAAACQAAAAAQAAAJAAAAABAAOgAQADAAAAAQABAACgAgAEAAAAAQAAAGygAwAEAAAAAQAAAGwAAAAA/+0AOFBob3Rvc2hvcCAzLjAAOEJJTQQEAAAAAAAAOEJJTQQlAAAAAAAQ1B2M2Y8AsgTpgAmY7PhCfv/AABEIAGwAbAMBIgACEQEDEQH/xAAfAAABBQEBAQEBAQAAAAAAAAAAAQIDBAUGBwgJCgv/xAC1EAACAQMDAgQDBQUEBAAAAX0BAgMABBEFEiExQQYTUWEHInEUMoGRoQgjQrHBFVLR8CQzYnKCCQoWFxgZGiUmJygpKjQ1Njc4OTpDREVGR0hJSlNUVVZXWFlaY2RlZmdoaWpzdHV2d3h5eoOEhYaHiImKkpOUlZaXmJmaoqOkpaanqKmqsrO0tba3uLm6wsPExcbHyMnK0tPU1dbX2Nna4eLj5OXm5+jp6vHy8/T19vf4+fr/xAAfAQADAQEBAQEBAQEBAAAAAAAAAQIDBAUGBwgJCgv/xAC1EQACAQIEBAMEBwUEBAABAncAAQIDEQQFITEGEkFRB2FxEyIygQgUQpGhscEJIzNS8BVictEKFiQ04SXxFxgZGiYnKCkqNTY3ODk6Q0RFRkdISUpTVFVWV1hZWmNkZWZnaGlqc3R1dnd4eXqCg4SFhoeIiYqSk5SVlpeYmZqio6Slpqeoqaqys7S1tre4ubrCw8TFxsfIycrS09TV1tfY2dri4+Tl5ufo6ery8/T19vf4+fr/2wBDABYWFhYWFiYWFiY2JiYmNkk2NjY2SVxJSUlJSVxvXFxcXFxcb29vb29vb2+GhoaGhoacnJycnK+vr6+vr6+vr6//2wBDARsdHS0pLUwpKUy3fGZ8t7e3t7e3t7e3t7e3t7e3t7e3t7e3t7e3t7e3t7e3t7e3t7e3t7e3t7e3t7e3t7e3t7f/3QAEAAf/2gAMAwEAAhEDEQA/AOuooooAKKKKACiiigAooooAKKKKACiiigAooooA/9DrqKKKACiiqk038Cfiaic1FXZUYtuyJZJ0TjqarNcSHpxUFJXHKtJnQqaRJ5kn94/nSiaQd6ioqOZ9yuVFxLns4/KrSsrDKnNZVOR2Q5WtoV2viM5U10NSimI4ddwp9dad9Uc7QUUUUwP/0euooooAhnk2Jx1NZ9T3DZkx6VXrgrSvI6qash6RtIcLU32WT1FMhl8snIyDVj7VH6GqgoNe8xScr6EX2WT1FH2WT1FS/ao/Q0fao/Q1fLS7k3n2IWt5FGeD9Kgq410uPlBz71SrKoor4S4N/aJoZNj+x61o1k1pRNujBrXDy+yZ1V1JKKKK6jE//9LrqKKKAMyT/WN9aZUsoxIairzZbs7FsTQRrITu7Va+zxf3f1NUULA5TrU2+59/yrWnKKWqM5J30Y6aFFTcvGKqVM3nvwwJ/CmFHAyQRUVNXeKKjorNjaKKWsyxKvW3+r/GqVX7cYj+tb0PiMqmxNRRRXac5//T66iiigCpcr0f8Kq1qMoYFT3rPdCjYNcdaFnzG9OWliS3ZVY7uM1b8xP7w/Os2ilCq4q1hygm7ml5if3h+dNaSMKckGs+iq9u+wvZISloornNRQCTgd60lG0BR2qvBHj52/CrNdlGFldnPUld2CiiitzM/9TrqKKKACmuiuMGnUUmr6MCg8TJ7j1qOtOo2iRuornlQ/lNlU7lCirn2dPelEEY96j2Eh+0RTAJOBzVmODHzP8AlVgKF4AxS1rCilqyJVG9gooorczCiiigD//V66iiigAooooAKKKKACiiigAooooAKKKKACiiigD/2Q==";
/// NSKeyedArchiver bplist of `["supports-polls"]`, verbatim from the fork.
const POLLS_REQUIRED_CAPABILITIES_B64: &str = "YnBsaXN0MDDUAQIDBAUGBwpYJHZlcnNpb25ZJGFyY2hpdmVyVCR0b3BYJG9iamVjdHMSAAGGoF8QD05TS2V5ZWRBcmNoaXZlctEICVRyb290gAGkCwwSE1UkbnVsbNINDg8RWk5TLm9iamVjdHNWJGNsYXNzoRCAAoADXnN1cHBvcnRzLXBvbGxz0hQVFhdaJGNsYXNzbmFtZVgkY2xhc3Nlc1dOU0FycmF5ohYYWE5TT2JqZWN0CBEaJCkyN0lMUVNYXmNudXd5e4qPmqOrrgAAAAAAAAEBAAAAAAAAABkAAAAAAAAAAAAAAAAAAAC3";

/// Handles inside the payload are bare: `+1…` or an email (see the capture).
/// The fork strips only `mailto:`; iOS also writes phone numbers bare.
pub fn bare_handle(h: &str) -> String {
    h.strip_prefix("tel:").or_else(|| h.strip_prefix("mailto:")).unwrap_or(h).to_string()
}

fn b64(s: &str) -> Vec<u8> {
    general_purpose::STANDARD.decode(s).expect("embedded base64")
}

/// The creation JSON. Keys follow the fork (L76-98) and the captured iPhone
/// payload: `item.{orderedPollOptions,title,creatorHandle}`, `version: 1`.
pub fn poll_payload(creator: &str, title: &str, options: &[(String, String)]) -> Json {
    let creator = bare_handle(creator);
    json!({
        "item": {
            "orderedPollOptions": options.iter().map(|(id, text)| json!({
                "text": text,
                "canBeEdited": false,
                "optionIdentifier": id,
                "creatorHandle": creator,
                "attributedText": text,
            })).collect::<Vec<_>>(),
            "title": title,
            "creatorHandle": creator,
        },
        "version": 1,
    })
}

/// The vote JSON: the voter's whole selection. An empty list withdraws, as
/// the iPhone did in the capture. `serverVoteTime` is omitted, as the fork
/// omits it and as the iPhone's first vote did.
pub fn vote_payload(voter: &str, option_ids: &[String]) -> Json {
    let voter = bare_handle(voter);
    json!({
        "item": {
            "votes": option_ids.iter().map(|id| json!({
                "participantHandle": voter,
                "voteOptionIdentifier": id,
            })).collect::<Vec<_>>(),
        },
        "version": 1,
    })
}

fn data_url(payload: &Json, suffix: &str) -> String {
    format!("data:,{}{}", general_purpose::STANDARD.encode(payload.to_string()), suffix)
}

pub struct BuiltPoll {
    pub message: NormalMessage,
    pub payload: Json,
    pub session: String,
    /// (option id, text), in order.
    pub options: Vec<(String, String)>,
}

/// A poll message, as `NormalMessage::poll_with_meta` in the fork.
pub fn build_poll(creator: &str, question: &str, options: &[String]) -> anyhow::Result<BuiltPoll> {
    if options.len() < 2 {
        anyhow::bail!("a poll needs at least two options");
    }
    if options.iter().any(|o| o.trim().is_empty()) {
        anyhow::bail!("poll options must not be empty");
    }
    let pairs: Vec<(String, String)> = options
        .iter()
        .map(|t| (uuid::Uuid::new_v4().to_string().to_uppercase(), t.clone()))
        .collect();
    let session = uuid::Uuid::new_v4().to_string().to_uppercase();
    // The fork always writes `?src=p&c=2`. The iPhone wrote c=2 on a
    // two-option poll and c=3 after a third option was added, so c is taken
    // here as the option count. [VERIFY]
    let suffix = format!("?src=p&c={}", pairs.len());
    let payload = poll_payload(creator, question, &pairs);
    let url = data_url(&payload, &suffix);
    let balloon = Balloon {
        url,
        session: Some(session.clone()),
        layout: Some(BalloonLayout::TemplateLayout {
            image_subtitle: String::new(),
            image_title: String::new(),
            caption: POLLS_BREADCRUMB.to_string(),
            secondary_subcaption: String::new(),
            tertiary_subcaption: String::new(),
            subcaption: String::new(),
            class: NSDictionaryClass::NSDictionary,
        }),
        ld_text: Some(POLLS_BREADCRUMB.to_string()),
        is_live: true,
        icon: Some(b64(POLLS_ICON_B64)),
        required_capabilities: Some(b64(POLLS_REQUIRED_CAPABILITIES_B64)),
        send_as_text: Some(true),
    };
    let parts = MessageParts(vec![IndexedMessagePart {
        part: MessagePart::Object(POLLS_BREADCRUMB.to_string()),
        idx: None,
        ext: None,
    }]);
    let mut message = NormalMessage::new(String::new(), MessageType::IMessage);
    message.xml = Some(parts.xml_string());
    message.parts = parts;
    message.app = Some(ExtensionApp {
        name: POLLS_APP_NAME.to_string(),
        app_id: None,
        bundle_id: POLLS_BUNDLE_ID.to_string(),
        balloon: Some(balloon),
    });
    Ok(BuiltPoll { message, payload, session, options: pairs })
}

fn plist_debug<T: serde::Serialize>(v: &T) -> String {
    plist::to_value(v).map(|p| format!("{p:?}")).unwrap_or_else(|e| format!("<{e}>"))
}

/// The poll JSON with one option appended, as the iPhone wrote it when an
/// option was added (capture line 6): the whole `item` repeated, the new
/// entry carrying the adder as `creatorHandle`, `canBeEdited: false`, and
/// `attributedText` = `text`. Returns (payload, new option id, `?src=p&c=N`).
///
/// Only the payload is established. The envelope of the update (associated
/// type 2 vs 4000, layout, icon, capabilities, body, `to_part`) was not in
/// the capture, so no `poll_add_option` command sends this yet.
pub fn extend_payload(prev: &Json, adder: &str, text: &str) -> anyhow::Result<(Json, String, String)> {
    if text.trim().is_empty() {
        anyhow::bail!("option text must not be empty");
    }
    let mut next = prev.clone();
    let opts = next["item"]["orderedPollOptions"]
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("payload has no item.orderedPollOptions"))?;
    if opts.iter().any(|o| o["text"].as_str() == Some(text)) {
        anyhow::bail!("poll already has an option {text:?}");
    }
    let id = uuid::Uuid::new_v4().to_string().to_uppercase();
    opts.push(json!({
        "text": text,
        "canBeEdited": false,
        "optionIdentifier": id,
        "creatorHandle": bare_handle(adder),
        "attributedText": text,
    }));
    let suffix = format!("?src=p&c={}", opts.len());
    Ok((next, id, suffix))
}

/// Record a poll we sent, so later votes on it resolve to option text.
pub fn register_sent(poll_guid: &str, session: &str, options: Vec<(String, String)>, payload: Json) {
    if let Ok(mut m) = PAYLOADS.lock() {
        m.insert(poll_guid.to_uppercase(), payload);
    }
    remember(poll_guid, Some(session), Some(options));
}

/// Option ids known for a poll seen or sent since startup.
pub fn option_ids_of(poll_guid: &str) -> Option<Vec<String>> {
    OPTIONS.lock().ok()?.get(poll_guid).map(|v| v.iter().map(|(i, _)| i.clone()).collect())
}

/// A vote: `React` with `Extension { is_meta: true }` (associated type 4000)
/// on the poll guid, an empty `Object` body, no part and empty `to_text`, as
/// the fork's `send_poll_vote_message` (common.rs L2346-2370).
pub fn build_vote(
    poll_guid: &str,
    session: &str,
    voter: &str,
    option_ids: &[String],
) -> anyhow::Result<ReactMessage> {
    let session = uuid::Uuid::parse_str(session)
        .map_err(|e| anyhow::anyhow!("poll session {session:?} is not a UUID: {e}"))?
        .to_string()
        .to_uppercase();
    let balloon = Balloon {
        url: data_url(&vote_payload(voter, option_ids), ""),
        session: Some(session),
        layout: None,
        ld_text: None,
        is_live: false,
        icon: None,
        required_capabilities: None,
        send_as_text: None,
    };
    Ok(ReactMessage {
        to_uuid: poll_guid.to_string(),
        to_part: None,
        reaction: ReactMessageType::Extension {
            spec: ExtensionApp {
                name: POLLS_APP_NAME.to_string(),
                app_id: None,
                bundle_id: POLLS_BUNDLE_ID.to_string(),
                balloon: Some(balloon),
            },
            body: MessageParts(vec![IndexedMessagePart {
                part: MessagePart::Object(String::new()),
                idx: None,
                ext: None,
            }]),
            is_meta: true,
        },
        to_text: String::new(),
        embedded_profile: None,
    })
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

    // Real iPhone capture, 2026-10-28 (poll-capture.log): creation, a 1+2
    // vote with serverVoteTime, and a withdrawal.
    const CAP_CREATE: &str = "data:,eyJ2ZXJzaW9uIjoxLCJpdGVtIjp7Im9yZGVyZWRQb2xsT3B0aW9ucyI6W3sidGV4dCI6IjEiLCJjcmVhdG9ySGFuZGxlIjoiKzEyMTU1NTQxMDUwIiwiY2FuQmVFZGl0ZWQiOmZhbHNlLCJhdHRyaWJ1dGVkVGV4dCI6IjEiLCJvcHRpb25JZGVudGlmaWVyIjoiNkFERTlFOEUtMTE5MS00RUFCLUExQTAtNEIwNDBGMDI1MTc4In0seyJ0ZXh0IjoiMiIsImNyZWF0b3JIYW5kbGUiOiIrMTIxNTU1NDEwNTAiLCJjYW5CZUVkaXRlZCI6ZmFsc2UsImF0dHJpYnV0ZWRUZXh0IjoiMiIsIm9wdGlvbklkZW50aWZpZXIiOiJBMDcwMDI1OS0wODk4LTQ1RDMtQTA3Qy05RDBEODY5Q0NGRDgifV0sInRpdGxlIjoiIiwiY3JlYXRvckhhbmRsZSI6IisxMjE1NTU0MTA1MCJ9fQ==?src=p&c=2";
    const CAP_VOTE: &str = "data:,eyJ2ZXJzaW9uIjoxLCJpdGVtIjp7InZvdGVzIjpbeyJ2b3RlT3B0aW9uSWRlbnRpZmllciI6IjZBREU5RThFLTExOTEtNEVBQi1BMUEwLTRCMDQwRjAyNTE3OCIsInNlcnZlclZvdGVUaW1lIjo4MTQ5MTQ3MDUuOTIzMjIsInBhcnRpY2lwYW50SGFuZGxlIjoiKzEyMTU1NTQxMDUwIn0seyJ2b3RlT3B0aW9uSWRlbnRpZmllciI6IkEwNzAwMjU5LTA4OTgtNDVEMy1BMDdDLTlEMEQ4NjlDQ0ZEOCIsInBhcnRpY2lwYW50SGFuZGxlIjoiKzEyMTU1NTQxMDUwIn1dfX0=";
    const CAP_WITHDRAW: &str = "data:,eyJ2ZXJzaW9uIjoxLCJpdGVtIjp7InZvdGVzIjpbXX19";

    fn keys(v: &Json) -> Vec<String> {
        let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
        k.sort();
        k
    }

    fn balloon_url(app: &ExtensionApp) -> String {
        app.balloon.as_ref().unwrap().url.clone()
    }

    #[test]
    fn sent_poll_matches_capture_shape() {
        let opts = vec!["Tue".to_string(), "Wed".to_string()];
        let b = build_poll("mailto:agent@groupg.net", "Site visit?", &opts).unwrap();
        let url = balloon_url(b.message.app.as_ref().unwrap());
        assert!(url.ends_with("?src=p&c=2"));
        let ours = decode_url(&url).unwrap();
        let cap = decode_url(CAP_CREATE).unwrap();
        assert_eq!(keys(&ours), keys(&cap));
        assert_eq!(keys(&ours["item"]), keys(&cap["item"]));
        assert_eq!(
            keys(&ours["item"]["orderedPollOptions"][0]),
            keys(&cap["item"]["orderedPollOptions"][0])
        );
        assert_eq!(ours["version"], 1);
        assert_eq!(ours["item"]["creatorHandle"], "agent@groupg.net");
        assert_eq!(ours["item"]["orderedPollOptions"][1]["canBeEdited"], false);
        assert_eq!(ours["item"]["orderedPollOptions"][1]["attributedText"], "Wed");
        for (id, _) in &b.options {
            assert!(uuid::Uuid::parse_str(id).is_ok() && *id == id.to_uppercase());
        }
        assert_eq!(bare_handle("tel:+12155541050"), "+12155541050");
    }

    #[test]
    fn sent_poll_round_trips_through_our_decoder() {
        let opts = vec!["A".to_string(), "B".to_string(), "C".to_string()];
        let b = build_poll("tel:+12155550100", "Q", &opts).unwrap();
        let url = balloon_url(b.message.app.as_ref().unwrap());
        assert!(url.ends_with("c=3"));
        let ev = from_payload(&decode_url(&url).unwrap(), "SENT-1", None, Json::Null);
        assert_eq!(ev[0]["event"], "poll");
        assert_eq!(ev[0]["question"], "Q");
        assert_eq!(ev[0]["creator"], "+12155550100");
        assert_eq!(ev[0]["options"], json!(["A", "B", "C"]));
        let ids: Vec<String> = b.options.iter().map(|(i, _)| i.clone()).collect();
        assert_eq!(ev[0]["option_ids"], json!(ids));
    }

    #[test]
    fn sent_poll_envelope() {
        let b = build_poll("mailto:agent@groupg.net", "", &["1".into(), "2".into()]).unwrap();
        let m = &b.message;
        assert_eq!(m.xml.as_deref().map(|x| x.contains("Sent a poll")), Some(true));
        let app = m.app.as_ref().unwrap();
        assert_eq!(app.bundle_id, POLLS_BUNDLE_ID);
        assert_eq!(app.name, "Polls");
        let bl = app.balloon.as_ref().unwrap();
        assert!(bl.is_live);
        assert_eq!(bl.send_as_text, Some(true));
        assert_eq!(bl.ld_text.as_deref(), Some("Sent a poll"));
        let icon = bl.icon.as_ref().unwrap();
        assert_eq!(&icon[..3], &[0xFF, 0xD8, 0xFF]); // JPEG
        let caps = bl.required_capabilities.as_ref().unwrap();
        let v: plist::Value = plist::from_bytes(caps).unwrap();
        let objs = v.as_dictionary().unwrap()["$objects"].as_array().unwrap();
        assert!(objs.iter().any(|o| o.as_string() == Some("supports-polls")));
    }

    #[test]
    fn balloon_fields_survive_rustpush_wire_encoding() {
        let b = build_poll("mailto:agent@groupg.net", "Q", &["x".into(), "y".into()]).unwrap();
        let app = b.message.app.unwrap();
        let (_ati, bp) = app.to_raw(false).unwrap();
        let back = ExtensionApp::from_bp(&bp.unwrap(), POLLS_BUNDLE_ID).unwrap();
        let bl = back.balloon.unwrap();
        let orig = app.balloon.unwrap();
        assert_eq!(back.name, "Polls");
        assert_eq!(bl.url, orig.url);
        assert_eq!(bl.send_as_text, Some(true));
        assert_eq!(bl.required_capabilities, orig.required_capabilities);
        assert_eq!(bl.icon, orig.icon);
        assert!(bl.is_live);
        assert_eq!(
            bl.session.unwrap().to_uppercase(),
            orig.session.unwrap().to_uppercase()
        );
    }

    #[test]
    fn vote_matches_capture_and_round_trips() {
        let opts = vec!["1".to_string(), "2".to_string()];
        let b = build_poll("mailto:agent@groupg.net", "", &opts).unwrap();
        register_sent("POLL-SENT", &b.session, b.options.clone(), b.payload.clone());
        assert_eq!(session_of("poll-sent").as_deref(), Some(b.session.as_str()));
        let ids: Vec<String> = b.options.iter().map(|(i, _)| i.clone()).collect();
        let r = build_vote("POLL-SENT", &b.session, "tel:+12155541050", &ids).unwrap();
        assert_eq!(r.to_uuid, "POLL-SENT");
        assert_eq!(r.to_part, None);
        let ReactMessageType::Extension { spec, is_meta, .. } = &r.reaction else { panic!() };
        assert!(*is_meta);
        let bl = spec.balloon.as_ref().unwrap();
        assert!(!bl.is_live);
        assert!(bl.layout.is_none() && bl.icon.is_none() && bl.required_capabilities.is_none());
        let ours = decode_url(&bl.url).unwrap();
        let cap = decode_url(CAP_VOTE).unwrap();
        assert_eq!(keys(&ours), keys(&cap));
        assert_eq!(keys(&ours["item"]), keys(&cap["item"]));
        // The iPhone adds serverVoteTime on some entries; we omit it.
        let mut cap_entry = keys(&cap["item"]["votes"][0]);
        cap_entry.retain(|k| k != "serverVoteTime");
        assert_eq!(keys(&ours["item"]["votes"][0]), cap_entry);
        let ev = from_payload(&ours, "POLL-SENT", None, Json::Null);
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[0]["voter"], "+12155541050");
        assert_eq!(ev[0]["option"], "1");
        assert_eq!(ev[1]["option"], "2");
        assert_eq!(ev[0]["option_known"], true);
    }

    #[test]
    fn withdraw_matches_capture() {
        let s = uuid::Uuid::new_v4().to_string();
        let r = build_vote("P", &s, "mailto:agent@groupg.net", &[]).unwrap();
        let ReactMessageType::Extension { spec, .. } = &r.reaction else { panic!() };
        let ours = decode_url(&spec.balloon.as_ref().unwrap().url).unwrap();
        assert_eq!(ours, decode_url(CAP_WITHDRAW).unwrap());
        let ev = from_payload(&ours, "P", None, Json::Null);
        assert_eq!(ev[0]["selected"], json!([]));
    }

    #[test]
    fn bad_inputs_rejected() {
        assert!(build_poll("x", "q", &["only".into()]).is_err());
        assert!(build_poll("x", "q", &["a".into(), " ".into()]).is_err());
        assert!(build_vote("P", "not-a-uuid", "x", &[]).is_err());
    }

    #[test]
    fn captured_poll_decodes() {
        let ev = from_payload(&decode_url(CAP_CREATE).unwrap(), "E150", None, Json::Null);
        assert_eq!(ev[0]["options"], json!(["1", "2"]));
        let v = from_payload(&decode_url(CAP_VOTE).unwrap(), "E150", None, Json::Null);
        assert_eq!(v.len(), 2);
        assert_eq!(v[1]["option"], "2");
    }

    #[test]
    fn sender_comes_from_envelope_not_payload() {
        // CAP_CREATE names +12155541050 as creatorHandle; the envelope says otherwise.
        let ev = from_payload(&decode_url(CAP_CREATE).unwrap(), "FORGE", Some("tel:+19995550000"), Json::Null);
        assert_eq!(ev[0]["creator"], "+12155541050");
        assert_eq!(ev[0]["sender"], "tel:+19995550000");
        let v = from_payload(&decode_url(CAP_VOTE).unwrap(), "FORGE", Some("tel:+19995550000"), Json::Null);
        assert!(v.iter().all(|e| e["voter"] == "+12155541050" && e["sender"] == "tel:+19995550000"));
        let w = from_payload(&decode_url(CAP_WITHDRAW).unwrap(), "FORGE", Some("tel:+19995550000"), Json::Null);
        assert_eq!(w[0]["sender"], "tel:+19995550000");
        let n = from_payload(&decode_url(CAP_CREATE).unwrap(), "FORGE", None, Json::Null);
        assert!(n[0]["sender"].is_null());
    }

    // Capture line 6 (2026-10-28): the iPhone added "new choice" to CAP_CREATE.
    const CAP_ADDED: &str = "data:,eyJ2ZXJzaW9uIjoxLCJpdGVtIjp7ImNyZWF0b3JIYW5kbGUiOiIrMTIxNTU1NDEwNTAiLCJ0aXRsZSI6IiIsIm9yZGVyZWRQb2xsT3B0aW9ucyI6W3siY2FuQmVFZGl0ZWQiOmZhbHNlLCJvcHRpb25JZGVudGlmaWVyIjoiNkFERTlFOEUtMTE5MS00RUFCLUExQTAtNEIwNDBGMDI1MTc4IiwiY3JlYXRvckhhbmRsZSI6IisxMjE1NTU0MTA1MCIsInRleHQiOiIxIiwiYXR0cmlidXRlZFRleHQiOiIxIn0seyJhdHRyaWJ1dGVkVGV4dCI6IjIiLCJvcHRpb25JZGVudGlmaWVyIjoiQTA3MDAyNTktMDg5OC00NUQzLUEwN0MtOUQwRDg2OUNDRkQ4IiwiY3JlYXRvckhhbmRsZSI6IisxMjE1NTU0MTA1MCIsInRleHQiOiIyIiwiY2FuQmVFZGl0ZWQiOmZhbHNlfSx7ImF0dHJpYnV0ZWRUZXh0IjoibmV3IGNob2ljZSIsIm9wdGlvbklkZW50aWZpZXIiOiIzMjZBQjUyOC1DODM0LTRGNDEtQjkyNC05MjI3MTQ2REU1MDEiLCJjcmVhdG9ySGFuZGxlIjoiKzEyMTU1NTQxMDUwIiwidGV4dCI6Im5ldyBjaG9pY2UiLCJjYW5CZUVkaXRlZCI6ZmFsc2V9XX19?src=p&c=3";

    #[test]
    fn extend_matches_captured_add_option() {
        let base = decode_url(CAP_CREATE).unwrap();
        let (ours, id, suffix) = extend_payload(&base, "tel:+12155541050", "new choice").unwrap();
        assert_eq!(suffix, "?src=p&c=3");
        assert!(CAP_ADDED.ends_with(&suffix));
        let mut cap = decode_url(CAP_ADDED).unwrap();
        // Identical apart from the minted option id.
        cap["item"]["orderedPollOptions"][2]["optionIdentifier"] = json!(id);
        assert_eq!(ours, cap);
    }

    #[test]
    fn extended_payload_round_trips_and_updates_map() {
        let base = decode_url(CAP_CREATE).unwrap();
        let (ours, id, suffix) = extend_payload(&base, "mailto:agent@groupg.net", "Fri").unwrap();
        let url = data_url(&ours, &suffix);
        let ev = from_payload(&decode_url(&url).unwrap(), "ADD-1", None, Json::Null);
        assert_eq!(ev[0]["event"], "poll");
        assert_eq!(ev[0]["options"], json!(["1", "2", "Fri"]));
        assert_eq!(ev[0]["option_ids"][2], json!(id));
        assert_eq!(option_ids_of("ADD-1").unwrap().len(), 3);
        assert_eq!(payload_of("add-1").unwrap(), ours);
        assert_eq!(ours["item"]["orderedPollOptions"][2]["creatorHandle"], "agent@groupg.net");
        assert!(extend_payload(&ours, "x", "Fri").is_err());
        assert!(extend_payload(&ours, "x", " ").is_err());
        assert!(extend_payload(&json!({"item":{}}), "x", "a").is_err());
    }

    #[test]
    fn captured_add_option_decodes_as_poll() {
        let ev = from_payload(&decode_url(CAP_ADDED).unwrap(), "E150F794", None, Json::Null);
        assert_eq!(ev[0]["options"], json!(["1", "2", "new choice"]));
    }
}
