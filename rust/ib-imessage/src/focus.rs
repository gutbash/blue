//! Focus status ("has notifications silenced"), read-only, via rustpush's
//! StatusKit client (`rustpush/src/statuskit.rs`).
//!
//! How Apple's Focus sharing works, as rustpush implements it:
//!
//! 1. **Key exchange is push, not pull.** Someone who shares their Focus
//!    status with us sends an IDS message (command 227) on
//!    `com.apple.private.alloy.status.keysharing`, a sub-service of
//!    `com.apple.private.alloy.multiplex1`. It carries their StatusKit
//!    channel id, an AES ratchet key, an EC signing key, and their personal
//!    config (which Focus modes are allowed to reach us). rustpush stores it in
//!    `StatusKitState.keys`. There is no request we can send to obtain it: it
//!    exists only if the other person's device chose to share with us. That
//!    is the iPhone's "Share Focus Status" setting (Settings > Focus > Focus
//!    Status, plus Messages' per-app permission); which conversations iOS
//!    shares with, and when, is the device's decision. [VERIFY on a device]
//! 2. **Status arrives on an APNs channel.** With a key, we subscribe to
//!    their channel (`com.apple.icloud.presence.mode.status`) and each publish
//!    is decrypted and verified with the shared keys. No Apple ID sign-in and
//!    no GSA token are needed for this: rustpush only asks the token provider
//!    in `ensure_channel` / `share_status`, which are the *publishing* side.
//! 3. **We never publish.** This module never calls `ensure_channel`,
//!    `share_status` or `invite_to_channel`, so the Apple ID has no StatusKit
//!    channel of its own and shares no status with anyone. Receiving does not
//!    require it.
//!
//! The registration already lists `multiplex1` (OpenBubbles registered it),
//! but the helper's identity manager only knew `com.apple.madrid`. `--focus`
//! adds `multiplex1`. rustpush re-registers any service whose `data_hash`
//! differs from the stored one, and this build cannot re-register, so
//! `--focus` is refused at startup unless the hash matches.
//!
//! Status semantics (rustpush `StatusKitStatus`): `a` (active) true means no
//! Focus is on; false means a Focus is on, with `i` the mode id. `allowed`
//! is rustpush's "would our message break through": active, or the mode is
//! in the allowed list the sharer sent us.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use rustpush::statuskit::{ChannelInterestToken, StatusKitClient, StatusKitMessage, StatusKitState};
use rustpush::{
    default_provider, APSConnection, APSMessage, AppleAccount, DebugMutex, DefaultAnisetteProvider,
    IDSUser, IdentityManager, OSConfig, TokenProvider,
};
use serde_json::{json, Value as Json};
use tokio::sync::{broadcast, Mutex};

pub const MUX: &str = "com.apple.private.alloy.multiplex1";
pub const STATE_FILE: &str = "statuskit.plist";

/// `Ok` when every user's stored `multiplex1` registration hash equals what
/// rustpush would compute, i.e. adding the service will not re-register.
pub fn registration_ok(users: &[IDSUser]) -> std::result::Result<(), String> {
    let want = rustpush::findmy::MULTIPLEX_SERVICE.hash_data();
    for u in users {
        match u.registration.get(MUX) {
            None => return Err(format!("{} has no {MUX} registration", u.user_id)),
            Some(r) if r.data_hash != want => {
                return Err(format!(
                    "{MUX} registration hash {} differs from this build's {want}; \
                     enabling it would re-register",
                    r.data_hash
                ))
            }
            Some(_) => {}
        }
    }
    Ok(())
}

/// One status, as `focus_status` and the `focus` event report it.
pub fn status_json(handle: &str, msg: &StatusKitMessage, observed_s: f64) -> Json {
    let StatusKitMessage::StatusChanged { user: _, mode, allowed, active, created_s } = msg;
    json!({
        "handle": handle,
        "focused": !*active,
        "mode": mode,
        "allowed": allowed,
        // When the sharer's device created this status: "since when".
        "at": epoch_to_rfc3339(*created_s),
        "at_epoch": created_s,
        "observed_at": epoch_to_rfc3339(observed_s),
    })
}

pub fn now_s() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// Seconds since the epoch -> `YYYY-MM-DDTHH:MM:SSZ` (UTC), no extra crate.
pub fn epoch_to_rfc3339(s: f64) -> String {
    let secs = s.max(0.0) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

pub struct Focus {
    client: Arc<StatusKitClient<DefaultAnisetteProvider>>,
    /// Held for the helper's lifetime; dropping one unsubscribes.
    tokens: Mutex<Vec<ChannelInterestToken>>,
    subscribed: Mutex<HashSet<String>>,
    last: Mutex<HashMap<String, Json>>,
}

fn save_state(path: &Path, state: &StatusKitState) {
    let tmp = path.with_extension("plist.tmp");
    let r = plist::to_file_binary(&tmp, state).map_err(|e| e.to_string())
        .and_then(|_| std::fs::rename(&tmp, path).map_err(|e| e.to_string()));
    if let Err(e) = r {
        log::error!("saving {}: {e}", path.display());
    }
}

impl Focus {
    pub async fn start(
        conn: APSConnection,
        os_config: Arc<dyn OSConfig>,
        identity: IdentityManager,
        state_dir: &Path,
    ) -> Result<Arc<Self>> {
        let path: PathBuf = state_dir.join(STATE_FILE);
        let state: StatusKitState = match plist::from_file(&path) {
            Ok(s) => s,
            Err(e) if path.exists() => return Err(anyhow!("reading {}: {e}", path.display())),
            Err(_) => StatusKitState::default(),
        };
        // An account that is never signed in. StatusKitClient wants a token
        // provider, but only its publishing calls use it, and we make none.
        let info = os_config.get_gsa_config(&*conn.state.read().await, false);
        let anisette = default_provider(info.clone(), state_dir.join("anisette"));
        let account = AppleAccount::new_with_anisette(info, anisette, None, Box::new(|_| {}))
            .map_err(|e| anyhow!("creating the (unsigned) Apple ID client: {e:?}"))?;
        let tokens = TokenProvider::new(
            Arc::new(DebugMutex::new(account)),
            os_config.clone(),
            Default::default(),
            Box::new(|_| {}),
        );
        let save_path = path.clone();
        let client = StatusKitClient::new(
            state,
            Box::new(move |s| save_state(&save_path, s)),
            tokens,
            conn,
            os_config,
            identity,
        )
        .await;
        let me = Arc::new(Self {
            client,
            tokens: Mutex::new(Vec::new()),
            subscribed: Mutex::new(HashSet::new()),
            last: Mutex::new(HashMap::new()),
        });
        me.subscribe_all().await;
        Ok(me)
    }

    /// Handles that have shared their Focus status with us.
    pub async fn sharers(&self) -> Vec<String> {
        let st = self.client.state.read().await;
        let mut v: Vec<String> = st.keys.values().map(|d| d.from.clone()).collect();
        v.sort();
        v.dedup();
        v
    }

    /// Subscribe to every sharer's channel not yet subscribed.
    pub async fn subscribe_all(&self) -> Vec<String> {
        let mut subscribed = self.subscribed.lock().await;
        let new: Vec<String> = self.sharers().await.into_iter().filter(|h| !subscribed.contains(h)).collect();
        if !new.is_empty() {
            log::info!("focus: subscribing to {}", new.join(", "));
            let token = self.client.request_handles(&new).await;
            self.tokens.lock().await.push(token);
            subscribed.extend(new.iter().cloned());
        }
        new
    }

    /// Feed one APNs message. Run on its own task: rustpush's handler can
    /// still panic on malformed input, and that must not take down the pump.
    pub fn handle(self: &Arc<Self>, msg: APSMessage, events: broadcast::Sender<String>) {
        let me = self.clone();
        tokio::spawn(async move {
            let before = me.sharers().await.len();
            match me.client.handle(msg).await {
                Ok(Some(m)) => {
                    let StatusKitMessage::StatusChanged { user, .. } = &m;
                    let ev = status_json(user, &m, now_s());
                    me.last.lock().await.insert(user.clone(), ev.clone());
                    let mut line = ev;
                    line["event"] = json!("focus");
                    log::info!("focus event: {line}");
                    let _ = events.send(line.to_string());
                }
                Ok(None) => {}
                Err(e) => log::warn!("statuskit: {e}"),
            }
            if me.sharers().await.len() != before {
                for h in me.subscribe_all().await {
                    let _ = events.send(json!({"event": "focus_shared", "handle": h}).to_string());
                }
            }
        });
    }

    pub async fn query(&self, handles: &[String]) -> Json {
        let sharers: HashSet<String> = self.sharers().await.into_iter().collect();
        let subscribed = self.subscribed.lock().await.clone();
        let last = self.last.lock().await;
        let out: Vec<Json> = handles
            .iter()
            .map(|h| {
                let shared = sharers.contains(h);
                match last.get(h) {
                    Some(s) => {
                        let mut s = s.clone();
                        s["shared"] = json!(true);
                        s["subscribed"] = json!(subscribed.contains(h));
                        s["known"] = json!(true);
                        s
                    }
                    None => json!({
                        "handle": h, "shared": shared, "subscribed": subscribed.contains(h),
                        "known": false, "focused": null, "mode": null, "allowed": null, "at": null,
                    }),
                }
            })
            .collect();
        json!({ "statuses": out, "sharers": sharers.into_iter().collect::<Vec<_>>() })
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(epoch_to_rfc3339(0.0), "1970-01-01T00:00:00Z");
        assert_eq!(epoch_to_rfc3339(1_798_416_000.0), "2026-12-28T00:00:00Z");
        assert_eq!(epoch_to_rfc3339(951_782_400.0 + 3661.0), "2000-02-29T01:01:01Z");
    }

    #[test]
    fn status_mapping() {
        let away = StatusKitMessage::StatusChanged {
            user: "tel:+12155550100".into(),
            mode: Some("com.apple.donotdisturb.mode.default".into()),
            allowed: false,
            active: false,
            created_s: 1_798_416_000.0,
        };
        let j = status_json("tel:+12155550100", &away, 1_798_416_060.0);
        assert_eq!(j["focused"], true);
        assert_eq!(j["allowed"], false);
        assert_eq!(j["mode"], "com.apple.donotdisturb.mode.default");
        assert_eq!(j["at"], "2026-12-28T00:00:00Z");
        assert_eq!(j["observed_at"], "2026-12-28T00:01:00Z");
        let avail = StatusKitMessage::StatusChanged {
            user: "x".into(), mode: None, allowed: true, active: true, created_s: 0.0,
        };
        assert_eq!(status_json("x", &avail, 0.0)["focused"], false);
    }

    #[test]
    fn mux_hash_of_this_build() {
        // Measured 2026-10-28: the firm's registration stores
        // 13329886844215203963 for multiplex1, so registration_ok refuses
        // --focus on it. madrid matches (5591053237194338696).
        assert_eq!(rustpush::findmy::MULTIPLEX_SERVICE.hash_data(), 10279869897339162389);
        assert_eq!(rustpush::MADRID_SERVICE.hash_data(), 5591053237194338696);
    }
}
