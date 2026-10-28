//! "Share Name and Photo" — attaching our iMessage profile to outgoing
//! messages so recipients see the name and photo set in OpenBubbles.
//!
//! What iMessage actually carries is a `ShareProfileMessage`: the id of an
//! encrypted CloudKit record in `com.apple.messages.profiles` and the 16-byte
//! key that decrypts it. rustpush's `ProfilesClient::set_record`
//! (`name_photo_sharing.rs`) generates that key at random and hands it back to
//! the caller only; it is never stored in CloudKit, and
//! `ProfilesClient::get_my_record` can find our record id but not the key. So
//! the helper cannot rebuild the profile from the account — it needs the copy
//! OpenBubbles kept when the profile was set.
//!
//! That copy lands in `share_profile.json` in the helper's state dir, either
//! through `--import-from` (see `import`) or written by hand:
//!
//!     {"cloud_kit_record_key":"<record id>",
//!      "cloud_kit_decryption_record_key":"<base64 or hex, 16 bytes>",
//!      "display_name":"Group G"}
//!
//! `display_name` is informational: it is what `status` reports, not what
//! recipients see. Recipients see the name stored in the CloudKit record.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{bail, Context, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use rustpush::ShareProfileMessage;

pub const FILE: &str = "share_profile.json";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ShareMode {
    /// Never attach the profile (the behaviour before this existed).
    Never,
    /// Attach it on the first message to each conversation since the
    /// helper started.
    First,
    /// Attach it to every message.
    Always,
}

impl ShareMode {
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "never" => Self::Never,
            "first" => Self::First,
            "always" => Self::Always,
            other => bail!("--share-profile must be always, first or never, not {other:?}"),
        })
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Never => "never",
            Self::First => "first",
            Self::Always => "always",
        }
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Stored {
    pub(crate) cloud_kit_record_key: String,
    /// base64 or hex.
    pub(crate) cloud_kit_decryption_record_key: String,
    #[serde(default)]
    pub(crate) display_name: Option<String>,
    /// Where it came from, for `status`.
    #[serde(default)]
    pub(crate) source: Option<String>,
    /// Contact Poster tags, base64, when the profile has a poster. Carried
    /// so an adopted profile embeds exactly what our own device sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) poster: Option<StoredPoster>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct StoredPoster {
    low_res_wallpaper_tag: String,
    wallpaper_tag: String,
    message_tag: String,
}

fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(b)
}

/// Write `share_profile.json` atomically (temp file + rename). Returns the
/// path written.
pub fn save(
    state_dir: &Path,
    m: &ShareProfileMessage,
    display_name: Option<&str>,
    source: &str,
) -> Result<String> {
    let out = state_dir.join(FILE);
    let stored = Stored {
        cloud_kit_record_key: m.cloud_kit_record_key.clone(),
        cloud_kit_decryption_record_key: b64(&m.cloud_kit_decryption_record_key),
        display_name: display_name.map(str::to_string),
        source: Some(source.to_string()),
        poster: m.poster.as_ref().map(|p| StoredPoster {
            low_res_wallpaper_tag: b64(&p.low_res_wallpaper_tag),
            wallpaper_tag: b64(&p.wallpaper_tag),
            message_tag: b64(&p.message_tag),
        }),
    };
    let tmp = out.with_extension("json.part");
    std::fs::write(&tmp, serde_json::to_string_pretty(&stored)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &out).with_context(|| format!("renaming into {}", out.display()))?;
    Ok(out.display().to_string())
}

/// Delete `share_profile.json`, for when our own device deletes the profile.
pub fn remove(state_dir: &Path) -> Result<()> {
    match std::fs::remove_file(state_dir.join(FILE)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

pub struct Profile {
    pub message: ShareProfileMessage,
    pub display_name: Option<String>,
    pub source: Option<String>,
}

fn decode_key(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    let hex = s.len() % 2 == 0
        && s.chars().all(|c| c.is_ascii_hexdigit())
        && s.len() == 32;
    if hex {
        return (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
            .collect();
    }
    base64::engine::general_purpose::STANDARD.decode(s).ok()
}

/// Load `share_profile.json` from the state dir. `Ok(None)` if absent.
pub fn load(state_dir: &Path) -> Result<Option<Profile>> {
    let path = state_dir.join(FILE);
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let stored: Stored =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    let key = decode_key(&stored.cloud_kit_decryption_record_key)
        .with_context(|| format!("{}: decryption key is not base64 or hex", path.display()))?;
    // rustpush's own records use 16 bytes; a key adopted from an Apple
    // device is taken as sent, so only emptiness is rejected.
    if key.is_empty() {
        bail!("{}: decryption key is empty", path.display());
    }
    if stored.cloud_kit_record_key.is_empty() {
        bail!("{}: cloud_kit_record_key is empty", path.display());
    }
    let poster = match &stored.poster {
        None => None,
        Some(p) => {
            let d = |s: &str| {
                base64::engine::general_purpose::STANDARD
                    .decode(s)
                    .with_context(|| format!("{}: poster tag is not base64", path.display()))
            };
            Some(rustpush::SharedPoster {
                low_res_wallpaper_tag: d(&p.low_res_wallpaper_tag)?,
                wallpaper_tag: d(&p.wallpaper_tag)?,
                message_tag: d(&p.message_tag)?,
            })
        }
    };
    Ok(Some(Profile {
        message: ShareProfileMessage {
            cloud_kit_decryption_record_key: key,
            cloud_kit_record_key: stored.cloud_kit_record_key,
            poster,
        },
        display_name: stored.display_name,
        source: stored.source,
    }))
}

/// Field names a stored `ShareProfileMessage` may use: rustpush's own
/// snake_case (serde) and the camelCase a Dart model would use.
const RECORD_NAMES: &[&str] = &["cloud_kit_record_key", "cloudKitRecordKey", "recordKey"];
const KEY_NAMES: &[&str] = &[
    "cloud_kit_decryption_record_key",
    "cloudKitDecryptionRecordKey",
    "decryptionKey",
];

/// The decryption key as stored: a string (base64/hex) or a byte array.
fn key_as_string(v: &Json) -> Option<String> {
    match v {
        Json::String(s) => decode_key(s).map(|_| s.clone()),
        Json::Array(a) => {
            let bytes: Option<Vec<u8>> =
                a.iter().map(|n| n.as_u64().and_then(|n| u8::try_from(n).ok())).collect();
            bytes.map(|b| base64::engine::general_purpose::STANDARD.encode(b))
        }
        _ => None,
    }
}

/// Depth-first search for an object carrying a record key and a decryption
/// key. String values are re-parsed as JSON, since Flutter's
/// shared_preferences stores structured values as encoded strings.
fn find(v: &Json, depth: usize) -> Option<(String, String)> {
    if depth > 6 {
        return None;
    }
    match v {
        Json::Object(map) => {
            let rec = RECORD_NAMES.iter().find_map(|n| map.get(*n).and_then(|v| v.as_str()));
            let key = KEY_NAMES.iter().find_map(|n| map.get(*n).and_then(key_as_string));
            if let (Some(r), Some(k)) = (rec, key) {
                return Some((r.to_string(), k));
            }
            map.values().find_map(|v| find(v, depth + 1))
        }
        Json::Array(a) => a.iter().find_map(|v| find(v, depth + 1)),
        Json::String(s) if s.starts_with('{') || s.starts_with('[') => {
            serde_json::from_str::<Json>(s).ok().and_then(|v| find(&v, depth + 1))
        }
        _ => None,
    }
}

/// Copy the shared profile out of an OpenBubbles data dir, if it has one.
///
/// Looks in `shared_preferences.json` for a stored `ShareProfileMessage`.
/// Returns what happened, for the log; never fails the import, because the
/// registration import must not depend on a profile existing. A hand-written
/// `share_profile.json` is left alone when nothing is found.
pub fn import(src: &Path, state_dir: &Path) -> String {
    let prefs_path = src.join("shared_preferences.json");
    let prefs: Json = match std::fs::read_to_string(&prefs_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
    {
        Some(p) => p,
        None => return format!("no readable {}; profile not imported", prefs_path.display()),
    };
    let sharing_on = prefs.get("flutter.nameAndPhotoSharing").and_then(|v| v.as_bool());
    let Some((record, key)) = find(&prefs, 0) else {
        return format!(
            "no shared-profile record in {} (flutter.nameAndPhotoSharing = {}); \
             set Name and Photo in OpenBubbles, then re-import",
            prefs_path.display(),
            sharing_on.map(|b| b.to_string()).unwrap_or_else(|| "absent".into())
        );
    };
    let stored = Stored {
        cloud_kit_record_key: record,
        cloud_kit_decryption_record_key: key,
        // OpenBubbles' local name field; the authoritative name is inside the
        // encrypted CloudKit record.
        display_name: prefs
            .get("flutter.userName")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        source: Some(prefs_path.display().to_string()),
        poster: None,
    };
    let out = state_dir.join(FILE);
    match serde_json::to_string_pretty(&stored)
        .map_err(anyhow::Error::from)
        .and_then(|s| std::fs::write(&out, s).map_err(Into::into))
    {
        Ok(()) => format!("imported shared profile into {}", out.display()),
        Err(e) => format!("found a shared profile but could not write {}: {e}", out.display()),
    }
}

/// Copy OpenBubbles' anisette provisioning (`anisette_test/`) to
/// `STATE_DIR/anisette`, which `set_profile` signs in with. Reusing it makes
/// the sign-in come from the device Apple already knows rather than a newly
/// provisioned one. Without it, rustpush provisions fresh. Never fails the
/// import.
pub fn import_anisette(src: &Path, state_dir: &Path) -> String {
    let from = src.join("anisette_test");
    let to = state_dir.join("anisette");
    let Ok(entries) = std::fs::read_dir(&from) else {
        return format!("no {}; set_profile will provision anisette fresh", from.display());
    };
    if let Err(e) = std::fs::create_dir_all(&to) {
        return format!("could not create {}: {e}", to.display());
    }
    let mut n = 0;
    for e in entries.flatten() {
        if e.file_type().map(|t| t.is_file()).unwrap_or(false)
            && std::fs::copy(e.path(), to.join(e.file_name())).is_ok()
        {
            n += 1;
        }
    }
    format!("copied {n} anisette file(s) into {}", to.display())
}

/// Per-process memory of which conversations already got the profile, for
/// `ShareMode::First`.
#[derive(Default)]
pub struct Sent(HashSet<String>);

impl Sent {
    /// Conversation key: the chat guid when known, else the sorted
    /// participant list.
    pub fn first_time(&mut self, guid: Option<&str>, participants: &[String]) -> bool {
        let key = match guid {
            Some(g) => format!("g:{g}"),
            None => {
                let mut p = participants.to_vec();
                p.sort();
                format!("p:{}", p.join(","))
            }
        };
        self.0.insert(key)
    }
}

#[cfg(test)]
mod roundtrip {
    #[test]
    fn save_then_load_keeps_keys_and_poster() {
        let dir = std::env::temp_dir().join(format!("ibim-prof-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let m = rustpush::ShareProfileMessage {
            cloud_kit_decryption_record_key: (0u8..16).collect(),
            cloud_kit_record_key: "+1ZvgjukQfNbTOQ4KJfjvA==".into(),
            poster: Some(rustpush::SharedPoster {
                low_res_wallpaper_tag: vec![1, 2, 3],
                wallpaper_tag: vec![4, 5],
                message_tag: vec![6],
            }),
        };
        super::save(&dir, &m, Some("Group G"), "own device").unwrap();
        let p = super::load(&dir).unwrap().unwrap();
        assert_eq!(p.message.cloud_kit_record_key, m.cloud_kit_record_key);
        assert_eq!(p.message.cloud_kit_decryption_record_key, m.cloud_kit_decryption_record_key);
        let poster = p.message.poster.unwrap();
        assert_eq!(poster.wallpaper_tag, vec![4, 5]);
        assert_eq!(p.display_name.as_deref(), Some("Group G"));
        assert_eq!(p.source.as_deref(), Some("own device"));
        assert!(!dir.join("share_profile.json.part").exists());
        super::remove(&dir).unwrap();
        assert!(super::load(&dir).unwrap().is_none());
        std::fs::remove_dir_all(&dir).ok();
    }
}
