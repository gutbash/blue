//! `set_profile`: create our "Share Name and Photo" record ourselves.
//!
//! OpenBubbles' own "Updating profile" step never completed on this machine,
//! so the helper creates the record directly, the way rustpush's test harness
//! does (`rustpush/src/test.rs`):
//!
//! 1. Sign in to GSA as the Apple ID, with the username and SRP-hashed
//!    password OpenBubbles keeps in `gsa.plist` (copied by `--import-from`).
//! 2. Build a `CloudKitClient` on that account.
//! 3. Call `ProfilesClient::set_record`, which encrypts the name and photo
//!    with a fresh random key, saves the record in the public
//!    `com.apple.messages.profiles` zone, and returns the `ShareProfileMessage`
//!    (record id + key) that outgoing messages embed.
//!
//! Signing in can require 2FA. Then the first call answers with a
//! `2fa_required:` error, the half-signed-in account is kept in memory, and a
//! second call carrying `tfa_code` finishes. A restart in between loses it.

use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

use rustpush::cloudkit::{CloudKitClient, CloudKitState};
use rustpush::name_photo_sharing::{IMessageNameRecord, IMessageNicknameRecord, ProfilesClient};
use rustpush::{
    default_provider, APSConnectionResource, AppleAccount, DebugMutex, DebugRwLock,
    DefaultAnisetteProvider, LoginState, OSConfig, ShareProfileMessage, TokenProvider,
};

use crate::profile::Profile;

type Account = AppleAccount<DefaultAnisetteProvider>;

/// Sign-in state kept across `set_profile` calls: an account waiting for a
/// 2FA code, or a signed-in profiles client reused for later updates.
#[derive(Default)]
pub struct Auth {
    pending: Option<Account>,
    client: Option<Arc<ProfilesClient<DefaultAnisetteProvider>>>,
}

/// OpenBubbles' `gsa.plist`.
#[derive(Deserialize)]
struct Gsa {
    username: String,
    password: plist::Data,
}

pub enum Outcome {
    Done(Profile),
    /// Apple wants a 2FA code; call again with `tfa_code`. The string says
    /// where the code went.
    Needs2fa(String),
}

/// Square-crop, resize to 512x512 and re-encode as JPEG.
///
/// rustpush uploads the photo as raw bytes under the name "NickNameImage" and
/// does no conversion. iOS shows it as a circular contact image, so a square
/// JPEG is the safe form. 512 px is our choice, not a documented Apple size.
/// JPEG and PNG input only; HEIC must be converted first.
pub fn prepare_photo(path: &str) -> Result<Vec<u8>> {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if matches!(ext.as_str(), "heic" | "heif") {
        bail!("{path}: HEIC is not supported here; convert it to JPEG or PNG first");
    }
    let img = image::open(path).with_context(|| format!("decoding {path} (JPEG or PNG)"))?;
    let side = img.width().min(img.height());
    if side < 64 {
        bail!("{path}: {}x{} is too small for a profile photo", img.width(), img.height());
    }
    let x = (img.width() - side) / 2;
    let y = (img.height() - side) / 2;
    let square = img
        .crop_imm(x, y, side, side)
        .resize_exact(512, 512, image::imageops::FilterType::Lanczos3)
        .to_rgb8();
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90)
        .encode_image(&square)
        .context("encoding the profile photo as JPEG")?;
    Ok(out)
}

/// Sign in to GSA. `Ok(Err(why))` means a 2FA code is needed; the
/// half-signed-in account is parked in `auth.pending`.
async fn sign_in(
    os_config: &Arc<dyn OSConfig>,
    aps: &APSConnectionResource,
    state_dir: &Path,
    auth: &mut Auth,
    tfa_code: Option<&str>,
) -> Result<std::result::Result<Account, String>> {
    let gsa_path = state_dir.join("gsa.plist");
    let gsa: Gsa = plist::from_file(&gsa_path).with_context(|| {
        format!("reading {} (re-run --import-from to copy it)", gsa_path.display())
    })?;
    let pass: Vec<u8> = gsa.password.into();

    let (mut account, mut state) = match (auth.pending.take(), tfa_code) {
        (Some(mut acct), Some(code)) => {
            let st = acct
                .verify_2fa(code.trim().to_string())
                .await
                .map_err(|e| anyhow!("2FA code rejected: {e:?}"))?;
            (acct, st)
        }
        _ => {
            let info = os_config.get_gsa_config(&*aps.state.read().await, false);
            let anisette = default_provider(info.clone(), state_dir.join("anisette"));
            let mut acct = AppleAccount::new_with_anisette(info, anisette, None, Box::new(|_| {}))
                .map_err(|e| anyhow!("creating the Apple ID client: {e:?}"))?;
            let st = acct
                .login_email_pass(&gsa.username, &pass)
                .await
                .map_err(|e| anyhow!("Apple ID sign-in: {e:?}"))?;
            (acct, st)
        }
    };
    // Same state machine as icloud_auth's `login_with_anisette`, except that
    // the code is asked of the caller instead of read from stdin.
    for _ in 0..4 {
        state = match state {
            LoginState::LoggedIn => return Ok(Ok(account)),
            LoginState::NeedsExtraStep(step) => {
                if account.get_pet().is_some() {
                    return Ok(Ok(account));
                }
                bail!("Apple asked for an extra sign-in step ({step}); finish it in OpenBubbles");
            }
            LoginState::NeedsLogin => account
                .login_email_pass(&gsa.username, &pass)
                .await
                .map_err(|e| anyhow!("Apple ID sign-in: {e:?}"))?,
            LoginState::NeedsDevice2FA | LoginState::NeedsSMS2FA => {
                account
                    .send_2fa_to_devices()
                    .await
                    .map_err(|e| anyhow!("asking Apple to send a 2FA code: {e:?}"))?;
                auth.pending = Some(account);
                return Ok(Err("a code was pushed to the Apple ID's trusted devices".into()));
            }
            LoginState::Needs2FAVerification => {
                auth.pending = Some(account);
                return Ok(Err("Apple is waiting for the code shown on a trusted device".into()));
            }
            LoginState::NeedsSMS2FAVerification(_) => {
                bail!("Apple asked for SMS 2FA, which set_profile does not support")
            }
        };
    }
    bail!("Apple ID sign-in did not settle after four steps")
}

/// Create (or replace) our profile record, persist it, and return it.
#[allow(clippy::too_many_arguments)]
pub async fn set_profile(
    os_config: &Arc<dyn OSConfig>,
    aps: &APSConnectionResource,
    state_dir: &Path,
    auth: &mut Auth,
    current: Option<ShareProfileMessage>,
    name: IMessageNameRecord,
    photo: Option<Vec<u8>>,
    tfa_code: Option<&str>,
) -> Result<Outcome> {
    let client = match auth.client.clone() {
        Some(c) => c,
        None => {
            let account = match sign_in(os_config, aps, state_dir, auth, tfa_code).await? {
                Ok(a) => a,
                Err(why) => return Ok(Outcome::Needs2fa(why)),
            };
            let dsid = account
                .persisted
                .as_ref()
                .map(|p| p.dsid)
                .context("signed in but Apple returned no dsid")?;
            let anisette = account.anisette.clone();
            let account = Arc::new(DebugMutex::new(account));
            let token_provider =
                TokenProvider::new(account, os_config.clone(), Default::default(), Box::new(|_| {}));
            let cloudkit = Arc::new(CloudKitClient {
                anisette,
                state: DebugRwLock::new(
                    CloudKitState::new(dsid.to_string()).context("CloudKit state")?,
                ),
                config: os_config.clone(),
                token_provider,
            });
            let c = Arc::new(ProfilesClient::new(cloudkit));
            auth.client = Some(c.clone());
            c
        }
    };
    let display = name.name.clone();
    // Passing the current record lets `set_record` delete it first, so a
    // second call replaces the profile instead of leaving an orphan.
    let mut existing = current;
    client
        .set_record(IMessageNicknameRecord { name, image: photo, poster: None }, &mut existing)
        .await
        .map_err(|e| anyhow!("creating the CloudKit profile record: {e}"))?;
    let message = existing.context("set_record returned no record")?;
    crate::profile::save(state_dir, &message, Some(&display), "set_profile")?;
    Ok(Outcome::Done(Profile {
        message,
        display_name: Some(display),
        source: Some("set_profile".to_string()),
    }))
}

#[cfg(test)]
mod tests {
    #[test]
    fn photo_is_square_512_jpeg() {
        let dir = std::env::temp_dir().join(format!("ibim-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("wide.png");
        image::RgbImage::from_pixel(900, 300, image::Rgb([200, 30, 30])).save(&src).unwrap();
        let out = super::prepare_photo(src.to_str().unwrap()).unwrap();
        assert_eq!(&out[..2], &[0xFF, 0xD8], "not a JPEG");
        let back = image::load_from_memory(&out).unwrap();
        assert_eq!((back.width(), back.height()), (512, 512));
        assert!(super::prepare_photo("C:\nope.heic").is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
