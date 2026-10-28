//! Loading the iMessage identity that OpenBubbles registered for us.
//!
//! We deliberately do not register with Apple ourselves. Registration needs
//! "validation data", which is produced by Apple's NAC crypto — implemented
//! upstream in `open-absinthe`, a *closed-source* crate that ships here only
//! as a non-functional stub. So `rustpush::register()` cannot work in this
//! build, and any code path reaching it will fail.
//!
//! What we can do is adopt a registration somebody else completed. OpenBubbles
//! stores its rustpush state as two plists, and they are exactly our types:
//!
//!     hw_info.plist   /os_config  → MacOSConfig   (the rented Mac's identity)
//!                     /push       → APSState      (APNs token + client cert)
//!                     /identity   → IDSNGMIdentity(device/pre/legacy keys)
//!     id.plist        (root)      → Vec<IDSUser>  (Apple ID + per-service
//!                                                  IDS certs and handles)
//!
//! `IDSUser::registration` is what makes this work: once OpenBubbles has
//! registered, that map holds a live `id_keypair` per service, and sending
//! only needs those — never `register()`.
//!
//! The catch is expiry. Registrations carry `registered_at_s` and lapse after
//! roughly a month, and renewing means registering again, which lands back on
//! the validation-data problem. So renewal stays OpenBubbles' job: run it,
//! let it re-register, and re-import. `registration_age` exists to warn about
//! that before sends start failing.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use plist::Value;
use rustpush::macos::MacOSConfig;
use rustpush::{APSState, IDSNGMIdentity, IDSUser};

/// Everything needed to talk to Apple as this Apple ID.
pub struct LoadedState {
    pub os_config: MacOSConfig,
    pub aps: APSState,
    pub identity: IDSNGMIdentity,
    pub users: Vec<IDSUser>,
    /// Where we persist refreshed IDS keys back to. Never the OpenBubbles
    /// copy — see `StateDir::import_from`.
    pub dir: PathBuf,
}

/// The helper's own state directory, seeded from OpenBubbles but separate.
///
/// Keeping our own copy matters for two reasons. Writing into the app's data
/// directory while it may be running risks a torn plist, and rustpush hands
/// us refreshed IDS keys through a callback that we must persist — clobbering
/// OpenBubbles' file with those would leave the user no way back if this
/// helper turns out to be broken.
pub struct StateDir(pub PathBuf);

/// The four files that are the registration. `hw_info` and `id` are the two
/// that matter; the rest are carried along so a restore is a straight copy.
const STATE_FILES: &[&str] = &["hw_info.plist", "id.plist", "gsa.plist", "id_cache.plist"];

impl StateDir {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self(dir.into())
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    /// True once `import_from` has run at least once.
    pub fn is_seeded(&self) -> bool {
        self.path("hw_info.plist").exists() && self.path("id.plist").exists()
    }

    /// Copy the registration out of an OpenBubbles data directory.
    ///
    /// Read-only with respect to `src`: we never write there, so OpenBubbles
    /// stays usable and stays the thing that can re-register.
    pub fn import_from(&self, src: &Path) -> Result<()> {
        std::fs::create_dir_all(&self.0)
            .with_context(|| format!("creating state dir {}", self.0.display()))?;
        for name in STATE_FILES {
            let from = src.join(name);
            if !from.exists() {
                // Only hw_info and id are load-bearing; the others are
                // absent on a fresh install and that's fine.
                if matches!(*name, "hw_info.plist" | "id.plist") {
                    bail!("{} has no {} — is OpenBubbles registered?", src.display(), name);
                }
                continue;
            }
            std::fs::copy(&from, self.path(name))
                .with_context(|| format!("copying {}", from.display()))?;
        }
        // Identity keys and an Apple ID auth cert. Owner-only, always.
        restrict_permissions(&self.0)?;
        Ok(())
    }

    pub fn load(&self) -> Result<LoadedState> {
        // Must happen before any key is touched: rustpush resolves private
        // keys through a process-wide keystore, and the migration below
        // imports into it.
        crate::migrate::init(&self.path("keystore.plist"))?;

        let hw_path = self.path("hw_info.plist");
        let mut hw_val: Value = plist::from_file(&hw_path)
            .with_context(|| format!("reading {}", hw_path.display()))?;
        // OpenBubbles stores RSA private keys inline; rustpush `main` wants
        // keystore aliases. See `migrate` for why and how.
        let n = crate::migrate::externalize_keys(&mut hw_val)
            .context("migrating keys in hw_info.plist")?;
        if n > 0 {
            log::debug!("imported {n} key(s) from hw_info.plist into the keystore");
        }
        let hw = hw_val
            .as_dictionary()
            .context("hw_info.plist is not a dictionary")?;

        // `os_config` is stored tagged (`type: MacOS`). We only support the
        // MacOS variant — an iOS-relay registration has different hardware
        // fields and would need a different OSConfig entirely.
        let os_config_val = hw
            .get("os_config")
            .context("hw_info.plist has no os_config")?;
        let tag = os_config_val
            .as_dictionary()
            .and_then(|d| d.get("type"))
            .and_then(|t| t.as_string())
            .unwrap_or("");
        if tag != "MacOS" {
            bail!(
                "os_config type is {tag:?}, expected \"MacOS\" — this helper \
                 only understands a Mac-derived registration"
            );
        }
        let os_config: MacOSConfig = plist::from_value(os_config_val)
            .context("os_config does not match rustpush's MacOSConfig")?;

        let aps: APSState = match hw.get("push") {
            Some(v) => plist::from_value(v).context("push state")?,
            // No token yet means OpenBubbles never completed APNs setup.
            None => bail!("hw_info.plist has no push state"),
        };
        let identity: IDSNGMIdentity = plist::from_value(
            hw.get("identity").context("hw_info.plist has no identity")?,
        )
        .context("identity keys")?;

        let id_path = self.path("id.plist");
        let mut id_val: Value = plist::from_file(&id_path)
            .with_context(|| format!("reading {}", id_path.display()))?;
        // Each user has an auth keypair, and each registered service under
        // `registration` has its own IDS keypair — all of them inline.
        let n = crate::migrate::externalize_keys(&mut id_val)
            .context("migrating keys in id.plist")?;
        if n > 0 {
            log::debug!("imported {n} key(s) from id.plist into the keystore");
        }
        let users: Vec<IDSUser> =
            plist::from_value(&id_val).context("id.plist does not match rustpush's IDSUser")?;
        if users.is_empty() {
            bail!("id.plist has no users — OpenBubbles is not signed in");
        }

        Ok(LoadedState {
            os_config,
            aps,
            identity,
            users,
            dir: self.0.clone(),
        })
    }

    /// Persist IDS keys handed back by rustpush's `keys_updated` callback.
    ///
    /// Written to a temporary file and renamed, because a half-written
    /// id.plist is an unusable registration and we'd have no way to rebuild
    /// it without another Mac.
    pub fn save_users(&self, users: &[IDSUser]) -> Result<()> {
        let dest = self.path("id.plist");
        let tmp = self.path("id.plist.tmp");
        plist::to_file_binary(&tmp, &users.to_vec()).context("serializing id.plist")?;
        std::fs::rename(&tmp, &dest).context("replacing id.plist")?;
        restrict_permissions(&self.0)?;
        Ok(())
    }
}

/// How long ago each service was registered, in seconds.
///
/// Used to warn while there's still time to re-register, rather than
/// discovering expiry as an opaque send failure.
pub fn registration_age(users: &[IDSUser]) -> HashMap<String, u64> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut out = HashMap::new();
    for user in users {
        for (service, reg) in &user.registration {
            let age = now.saturating_sub(reg.registered_at_s);
            // Several services can share a name across users; keep the
            // oldest, since that's the one that expires first.
            out.entry(service.clone())
                .and_modify(|e: &mut u64| *e = (*e).max(age))
                .or_insert(age);
        }
    }
    out
}

#[cfg(not(unix))]
fn restrict_permissions(_dir: &Path) -> Result<()> {
    // No mode bits on Windows. The state dir inherits the ACL of its parent;
    // keep it under the user profile so only that account can read it.
    Ok(())
}

#[cfg(unix)]
fn restrict_permissions(dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            std::fs::set_permissions(entry.path(), std::fs::Permissions::from_mode(0o600))?;
        }
    }
    Ok(())
}
