//! Bridging OpenBubbles' on-disk format to rustpush `main`.
//!
//! rustpush changed how RSA private keys are stored. Older builds — including
//! the OpenBubbles 1.15.0 that produced our registration — wrote the key
//! material inline:
//!
//!     keypair = { cert: <data>, private: <data> }   // DER, inline
//!
//! `main` moved private keys behind a keystore abstraction and now stores only
//! a label, because on Android and iOS the key lives in hardware and can never
//! be extracted:
//!
//!     keypair = { cert: <data>, private: "some-alias" }
//!
//! `RsaKey` is a newtype over that alias, so deserialising the old shape fails
//! with "invalid value: byte array, expected a string".
//!
//! The fix is to meet it where it is. We walk the plist, and every time we see
//! a `{cert, private}` pair with inline `private` bytes, we import those bytes
//! into a software keystore under a deterministic alias and swap the inline
//! data for that alias. What comes out the far side is the shape `main`
//! expects, and deserialisation proceeds untouched.
//!
//! Aliases are derived from a digest of the key itself rather than from its
//! position in the tree. Position is not stable — services come and go from
//! `registration` across renewals — but a key's own bytes are, so re-importing
//! is idempotent and a re-import after renewal doesn't strand the old entries.
//!
//! EC keys are unaffected: `IDSNGMIdentity` still serialises those inline via
//! `CompactECKey`, so `hw_info.plist`'s `identity` block loads as-is.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use anyhow::{Context, Result};
use keystore::software::{NoEncryptor, SoftwareKeystore, SoftwareKeystoreState};
use keystore::{KeystoreAccessRules, KeystoreDigest, KeystorePadding, RsaKey};
use plist::Value;

/// Install the process-wide software keystore, persisted at `path`.
///
/// rustpush reaches the keystore through a `OnceLock` global, so this must run
/// before any key is imported or used. Calling it twice is harmless —
/// `init_keystore` ignores the second set — but the first path wins.
pub fn init(path: &Path) -> Result<()> {
    let state: SoftwareKeystoreState = if path.exists() {
        plist::from_file(path).with_context(|| format!("reading keystore {}", path.display()))?
    } else {
        SoftwareKeystoreState::default()
    };

    let save_to: PathBuf = path.to_path_buf();
    let store = SoftwareKeystore {
        state: RwLock::new(state),
        update_state: Box::new(move |state| {
            // Called on every mutation. A failure here is not fatal for the
            // running process — the keys are live in memory — but the next
            // start would have to re-import, so it's worth shouting about.
            if let Err(e) = persist(&save_to, state) {
                log::error!("could not persist keystore: {e:#}");
            }
        }),
        // The registration plists next to this file are already unencrypted
        // and equally sensitive, so encrypting only the keystore would buy
        // nothing. Both are protected by 0600 and the user's home directory.
        encryptor: NoEncryptor,
    };
    keystore::init_keystore(store);
    Ok(())
}

fn persist(path: &Path, state: &SoftwareKeystoreState) -> Result<()> {
    let tmp = path.with_extension("tmp");
    plist::to_file_binary(&tmp, state).context("serializing keystore")?;
    std::fs::rename(&tmp, path).context("replacing keystore")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Rewrite inline RSA private keys into keystore aliases, in place.
///
/// Returns how many keys were imported, purely so startup can log whether a
/// migration happened.
pub fn externalize_keys(value: &mut Value) -> Result<usize> {
    let mut imported = 0;
    walk(value, &mut imported)?;
    Ok(imported)
}

fn walk(value: &mut Value, imported: &mut usize) -> Result<()> {
    match value {
        Value::Dictionary(dict) => {
            // A keypair is the only place `cert` and `private` appear together.
            let needs_migration = matches!(dict.get("private"), Some(Value::Data(_)))
                && dict.contains_key("cert");
            if needs_migration {
                let Some(Value::Data(der)) = dict.get("private") else {
                    unreachable!("checked above");
                };
                let alias = import_key(der)?;
                dict.insert("private".into(), Value::String(alias));
                *imported += 1;
            }
            // Recurse regardless: `registration` is a dictionary of
            // per-service dictionaries, each with its own keypair.
            for (_, v) in dict.iter_mut() {
                walk(v, imported)?;
            }
        }
        Value::Array(items) => {
            for v in items.iter_mut() {
                walk(v, imported)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Import one DER-encoded RSA private key, returning its alias.
fn import_key(der: &[u8]) -> Result<String> {
    let alias = format!("openbubbles-import:{}", short_digest(der));
    // These mirror the rules rustpush itself uses when it creates IDS and
    // activation keys (see `generate_auth_csr` and `activate`): PKCS#1
    // signatures over SHA-1, signing only. The software keystore ignores the
    // rules, but a hardware-backed one would enforce them, and a key imported
    // with weaker rules than it's used under would fail later, not now.
    let rules = KeystoreAccessRules {
        signature_padding: vec![KeystorePadding::PKCS1],
        digests: vec![KeystoreDigest::Sha1],
        can_sign: true,
        ..Default::default()
    };
    // `bits` is recorded metadata; the actual modulus comes from the DER, so
    // the two can't disagree.
    match RsaKey::import(&alias, 2048, der, rules) {
        Ok(_) => {}
        // Expected, and not a problem. The alias is a digest of the key, so an
        // existing entry under it holds these exact bytes. This happens both
        // within a single load — an Apple ID's auth keypair is reused across
        // every registered service — and on every start after the first, since
        // the keystore persists.
        Err(keystore::KeystoreError::KeyAlreadyExists) => {
            log::trace!("key {alias} already in keystore");
        }
        Err(e) => return Err(anyhow::anyhow!("importing RSA key {alias}: {e}")),
    }
    Ok(alias)
}

/// A short, stable name for a key, derived from the key itself.
///
/// Deliberately not a cryptographic hash. This only has to separate a handful
/// of keys from each other stably across runs, and it names a local keystore
/// entry rather than guarding anything. A change in hasher between Rust
/// releases would cause a harmless re-import under a new alias.
fn short_digest(bytes: &[u8]) -> String {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
        .to_be_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Which aliases the keystore currently holds, for diagnostics.
pub fn imported_aliases(path: &Path) -> HashMap<String, usize> {
    let mut out = HashMap::new();
    if let Ok(Value::Dictionary(d)) = plist::from_file::<_, Value>(path) {
        if let Some(Value::Dictionary(keys)) = d.get("keys") {
            for (k, _) in keys.iter() {
                out.insert(k.clone(), 1);
            }
        }
    }
    out
}
