//! Chat backgrounds (iOS 26): rustpush `Message::SetTranscriptBackground`,
//! IDS command 138.
//!
//! Wire shape (rustpush `SetTranscriptBackgroundMessage`, untagged plist):
//! - set: `traba` (1), `tbid` (sequence number), `cid` (chat id),
//!   `traboid`/`trabar`/`trabas`/`trabak`/`trabafs` (the MMCS object id, URL,
//!   signature, key with a leading 0x00, size), `trabapv` (payload version,
//!   1), `trabaid` (a fresh background UUID).
//! - clear: `traba`, `tbid`, `cid`, `trababr: true`.
//!
//! `tbid` is the version number, the counterpart of a group photo's
//! `group_version`. rustpush calls it "sequence number".
//!
//! The MMCS file is **not an image**. rustpush reads it with
//! `SimplifiedTranscriptPoster::parse_payload`: a zip holding
//! `transcriptBackground/watchBackground` (a plist with a small rendered
//! image) and `transcriptBackground/poster` (a PosterKit archive). A plain
//! JPEG uploaded the way `set_icon` uploads a group photo would not be a
//! valid background. Building a PosterKit photo poster from an image needs
//! device layout values (resolution, frames, title style) that are not
//! established here, so `set_background` sends only a payload that already
//! parses as a transcript background — typically one saved from an incoming
//! change with `download_background`. [VERIFY on a device]
//!
//! rustpush delivers the inbound message with no conversation
//! (`to_message(None, …)`), so the only chat reference is `cid`. That `cid`
//! equals the group's chat guid (`sender_guid`) is unverified.

use std::collections::{HashMap, VecDeque};
use std::sync::{LazyLock, Mutex};

use anyhow::{bail, Context, Result};
use rustpush::{posterkit::SimplifiedTranscriptPoster, MMCSFile, Message, MessageInst, SetTranscriptBackgroundMessage};
use serde_json::{json, Value as Json};

/// Latest version seen or sent per chat id.
static VERSIONS: LazyLock<Mutex<HashMap<String, u64>>> = LazyLock::new(Default::default);
/// Set messages by guid, for `download_background`.
static RECENT: LazyLock<Mutex<(HashMap<String, SetTranscriptBackgroundMessage>, VecDeque<String>)>> =
    LazyLock::new(Default::default);
const RECENT_LIMIT: usize = 64;

pub fn note_version(chat_id: Option<&str>, v: u64) {
    if let (Some(c), Ok(mut m)) = (chat_id, VERSIONS.lock()) {
        let e = m.entry(c.to_ascii_lowercase()).or_insert(0);
        *e = (*e).max(v);
    }
}

pub fn last_version(chat_id: &str) -> Option<u64> {
    VERSIONS.lock().ok()?.get(&chat_id.to_ascii_lowercase()).copied()
}

/// The version to send: the caller's, else one past the newest seen, else 1.
/// [VERIFY what iOS does with a stale or repeated version]
pub fn next_version(chat_id: Option<&str>, given: Option<u64>) -> u64 {
    given.unwrap_or_else(|| chat_id.and_then(last_version).map_or(1, |v| v + 1))
}

pub fn recent(guid: &str) -> Option<SetTranscriptBackgroundMessage> {
    RECENT.lock().ok()?.0.get(&guid.to_uppercase()).cloned()
}

fn fields(m: &SetTranscriptBackgroundMessage) -> (u64, Option<&String>) {
    match m {
        SetTranscriptBackgroundMessage::Remove { bid, chat_id, .. }
        | SetTranscriptBackgroundMessage::Set { bid, chat_id, .. } => (*bid, chat_id.as_ref()),
    }
}

pub fn event_for(inst: &MessageInst) -> Option<Json> {
    let Message::SetTranscriptBackground(b) = &inst.message else { return None };
    let (v, cid) = fields(b);
    note_version(cid.map(String::as_str), v);
    if matches!(b, SetTranscriptBackgroundMessage::Set { .. }) {
        if let Ok(mut g) = RECENT.lock() {
            let (map, order) = &mut *g;
            if order.len() >= RECENT_LIMIT {
                if let Some(old) = order.pop_front() {
                    map.remove(&old);
                }
            }
            order.push_back(inst.id.to_uppercase());
            map.insert(inst.id.to_uppercase(), b.clone());
        }
    }
    Some(from_message(b, &inst.id, inst.sender.as_deref()))
}

/// Pure part of `event_for`. `sender` is the Apple-verified envelope sender.
pub fn from_message(b: &SetTranscriptBackgroundMessage, guid: &str, sender: Option<&str>) -> Json {
    let (version, cid) = fields(b);
    let mut ev = json!({
        "event": "background",
        "guid": guid,
        "chat": cid,
        "sender": sender,
        "cleared": matches!(b, SetTranscriptBackgroundMessage::Remove { .. }),
        "version": version,
    });
    if let SetTranscriptBackgroundMessage::Set { background_id, payload_version, file_size, .. } = b {
        ev["background_id"] = json!(background_id);
        ev["payload_version"] = json!(payload_version);
        ev["bytes"] = json!(file_size);
    }
    ev
}

/// Check a local file is a transcript-background payload before uploading.
/// Returns the poster's extension kind for the reply.
pub fn validate_payload(path: &str) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {path}"))?;
    let lower = path.to_ascii_lowercase();
    if [".jpg", ".jpeg", ".png", ".heic", ".gif"].iter().any(|e| lower.ends_with(e)) {
        bail!(
            "{path}: an iOS chat background is a PosterKit archive, not an image; \
             building one from a photo is not supported yet. Pass a payload saved \
             with download_background."
        );
    }
    let parsed = SimplifiedTranscriptPoster::parse_payload(&bytes)
        .map_err(|e| anyhow::anyhow!("{path} is not a transcript-background payload: {e}"))?;
    Ok(poster_kind(&parsed))
}

fn poster_kind(p: &SimplifiedTranscriptPoster) -> String {
    use rustpush::posterkit::PosterType::*;
    match &p.poster.r#type {
        Photo { .. } => "photo",
        Monogram { .. } => "monogram",
        Memoji { .. } => "memoji",
        TranscriptDynamic { .. } => "dynamic",
        TranscriptGradient { .. } => "gradient",
    }
    .to_string()
}

pub fn build(file: Option<MMCSFile>, version: u64, chat_id: Option<String>) -> SetTranscriptBackgroundMessage {
    SetTranscriptBackgroundMessage::from_mmcs(file, version, chat_id)
}

/// Write the downloaded payload's watch preview image next to it, if any.
/// Returns (preview path, poster kind).
pub fn extract_preview(payload: &[u8], dest: &std::path::Path) -> (Option<String>, Option<String>) {
    let Ok(p) = SimplifiedTranscriptPoster::parse_payload(payload) else { return (None, None) };
    let kind = Some(poster_kind(&p));
    let data = &p.watch.background_image_data;
    let ext = match image::guess_format(data) {
        Ok(image::ImageFormat::Png) => "png",
        Ok(image::ImageFormat::Jpeg) => "jpg",
        _ => "bin",
    };
    let out = dest.with_extension(format!("preview.{ext}"));
    match std::fs::write(&out, data) {
        Ok(()) => (Some(out.display().to_string()), kind),
        Err(e) => {
            log::warn!("writing background preview {}: {e}", out.display());
            (None, kind)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire_round_trip(m: &SetTranscriptBackgroundMessage) -> SetTranscriptBackgroundMessage {
        let mut bin = Vec::new();
        plist::to_writer_binary(&mut bin, m).unwrap();
        plist::from_bytes(&bin).unwrap()
    }

    fn mmcs() -> MMCSFile {
        MMCSFile {
            signature: vec![1, 2, 3],
            object: "OBJ".into(),
            url: "https://mmcs.example/x".into(),
            key: vec![9; 32],
            size: 1234,
        }
    }

    #[test]
    fn clear_round_trip() {
        let m = wire_round_trip(&build(None, 4, Some("chat-guid".into())));
        let ev = from_message(&m, "G", Some("tel:+12155550100"));
        assert_eq!(ev["cleared"], true);
        assert_eq!(ev["version"], 4);
        assert_eq!(ev["chat"], "chat-guid");
        assert_eq!(ev["sender"], "tel:+12155550100");
        assert!(ev.get("background_id").is_none());
    }

    #[test]
    fn set_round_trip_keeps_mmcs() {
        let m = wire_round_trip(&build(Some(mmcs()), 7, Some("c".into())));
        let ev = from_message(&m, "G", None);
        assert_eq!(ev["cleared"], false);
        assert_eq!(ev["version"], 7);
        assert_eq!(ev["payload_version"], 1);
        assert_eq!(ev["bytes"], 1234);
        let f = m.to_mmcs().unwrap();
        assert_eq!(f.key, vec![9; 32]);
        assert_eq!(f.signature, vec![1, 2, 3]);
        assert_eq!(f.object, "OBJ");
    }

    #[test]
    fn wire_keys() {
        let v = plist::to_value(&build(None, 2, None)).unwrap();
        let d = v.as_dictionary().unwrap();
        assert_eq!(d.get("trababr").and_then(|b| b.as_boolean()), Some(true));
        assert_eq!(d.get("tbid").and_then(|b| b.as_unsigned_integer()), Some(2));
        let v = plist::to_value(&build(Some(mmcs()), 3, Some("c".into()))).unwrap();
        let d = v.as_dictionary().unwrap();
        for k in ["traba", "tbid", "cid", "traboid", "trabapv", "trabaid", "trabar", "trabas", "trabak", "trabafs"] {
            assert!(d.contains_key(k), "{k}");
        }
        assert!(!d.contains_key("trababr"));
    }

    #[test]
    fn versions() {
        assert_eq!(next_version(Some("UNSEEN-X"), None), 1);
        note_version(Some("Chat-V"), 5);
        note_version(Some("chat-v"), 3);
        assert_eq!(next_version(Some("CHAT-V"), None), 6);
        assert_eq!(next_version(Some("CHAT-V"), Some(2)), 2);
    }

    #[test]
    fn images_refused() {
        let dir = std::env::temp_dir().join("ib-bg-test.png");
        std::fs::write(&dir, b"x").unwrap();
        let e = validate_payload(dir.to_str().unwrap()).unwrap_err().to_string();
        assert!(e.contains("PosterKit"));
        let bad = std::env::temp_dir().join("ib-bg-test.bin");
        std::fs::write(&bad, b"not a zip").unwrap();
        assert!(validate_payload(bad.to_str().unwrap()).is_err());
    }
}
