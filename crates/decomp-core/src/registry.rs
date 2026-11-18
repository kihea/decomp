//! The name registry (spec, "Planes and components": Registry; section 7,
//! "How names resolve" and "Name rules").
//!
//! A name maps to an app ID. The app's manifest, module hashes, client files,
//! and metadata are the app's own record (see `app`); the registry adds the
//! name, how long it is held, and whether the app has been reviewed. Anyone
//! can resolve a name. What an application layer does with the metadata blob
//! an app names is its own business: the registry stores only its hash.
//!
//! Name rules:
//! - First come, with a credit deposit, which makes squatting cost something.
//!   The deposit is returned when the name is released.
//! - Well-known brand names are held until ownership is proven, and only a
//!   reviewer can assign them.
//! - Unused names expire: a name not renewed within its term returns to the
//!   pool, and its deposit to its owner.
//! - Look-alike names are blocked: a name whose skeleton matches a held name's
//!   is refused.
//!
//! Like `Accounts`, the registry changes only by applying entries, so a log of
//! entries is a complete copy.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::keys::AccountId;
use crate::ledger::Credits;

/// The deposit a name is held against. A starting value.
pub const NAME_DEPOSIT: Credits = Credits::whole(10);

/// How long a name is held after it is registered or renewed, in seconds.
pub const NAME_TERM_SECS: u64 = 365 * 24 * 3600;

/// The shortest and longest names, in characters. A name is one DNS label.
pub const MIN_NAME_CHARS: usize = 3;
pub const MAX_NAME_CHARS: usize = 63;

/// The most entries one directory page holds.
pub const MAX_DIRECTORY_PAGE: usize = 100;

/// A name and what it points to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NameRecord {
    pub name: String,
    pub app: AccountId,
    pub registered_at: u64,
    pub expires_at: u64,
    /// The app was reviewed and may be listed in a directory.
    pub verified: bool,
}

/// A change to the registry.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RegistryEntry {
    /// `app` takes `name` at `at`, with the deposit set aside.
    Register {
        name: String,
        app: AccountId,
        at: u64,
    },
    /// The name's term starts again from `at`.
    Renew { name: String, at: u64 },
    /// The name returns to the pool, by its owner's wish or because its term
    /// ran out.
    Release { name: String },
    /// A reviewer marks the app a name points to as reviewed, or not.
    SetVerified { name: String, verified: bool },
    /// A brand name is held until a reviewer assigns it.
    Reserve { name: String },
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum NameError {
    #[error("a name is 3 to 63 characters of a to z, 0 to 9, and inner hyphens")]
    BadName,
    #[error("that name is taken")]
    Taken,
    #[error("that name looks too much like {0}")]
    LookAlike(String),
    #[error("that name is held for its brand's owner; ask for it to be assigned")]
    Reserved,
    #[error("no such name")]
    UnknownName,
    #[error("only the app a name points to can change it")]
    NotOwner,
    #[error("an app holds one name")]
    AlreadyNamed,
}

/// Every name, and the names held for brands.
#[derive(Clone, Debug, Default)]
pub struct Registry {
    names: BTreeMap<String, NameRecord>,
    /// Each held name's skeleton, for spotting look-alikes.
    skeletons: BTreeMap<String, String>,
    reserved: BTreeSet<String>,
}

impl Registry {
    /// Whether `name` could be registered by `app` now, without a reviewer.
    pub fn check(&self, name: &str, app: &AccountId) -> Result<(), NameError> {
        check_syntax(name)?;
        if self.names.contains_key(name) {
            return Err(NameError::Taken);
        }
        if self.reserved.contains(name) {
            return Err(NameError::Reserved);
        }
        if let Some(other) = self.skeletons.get(&skeleton(name)) {
            return Err(NameError::LookAlike(other.clone()));
        }
        if self.names.values().any(|record| record.app == *app) {
            return Err(NameError::AlreadyNamed);
        }
        Ok(())
    }

    /// Whether `app` owns `name`.
    pub fn owned(&self, name: &str, app: &AccountId) -> Result<&NameRecord, NameError> {
        let record = self.names.get(name).ok_or(NameError::UnknownName)?;
        if record.app != *app {
            return Err(NameError::NotOwner);
        }
        Ok(record)
    }

    pub fn apply(&mut self, entry: &RegistryEntry) {
        match entry {
            RegistryEntry::Register { name, app, at } => {
                self.reserved.remove(name);
                self.skeletons.insert(skeleton(name), name.clone());
                self.names.insert(
                    name.clone(),
                    NameRecord {
                        name: name.clone(),
                        app: *app,
                        registered_at: *at,
                        expires_at: at + NAME_TERM_SECS,
                        verified: false,
                    },
                );
            }
            RegistryEntry::Renew { name, at } => {
                if let Some(record) = self.names.get_mut(name) {
                    record.expires_at = at + NAME_TERM_SECS;
                }
            }
            RegistryEntry::Release { name } => {
                self.names.remove(name);
                self.skeletons.remove(&skeleton(name));
            }
            RegistryEntry::SetVerified { name, verified } => {
                if let Some(record) = self.names.get_mut(name) {
                    record.verified = *verified;
                }
            }
            RegistryEntry::Reserve { name } => {
                self.reserved.insert(name.clone());
            }
        }
    }

    pub fn resolve(&self, name: &str) -> Option<&NameRecord> {
        self.names.get(name)
    }

    /// The name an app holds, if any.
    pub fn name_of(&self, app: &AccountId) -> Option<&NameRecord> {
        self.names.values().find(|record| record.app == *app)
    }

    /// Names in order, from just after `from`, at most `limit` of them; with
    /// `query`, only names that contain it.
    pub fn page(
        &self,
        verified_only: bool,
        query: Option<&str>,
        from: Option<&str>,
        limit: usize,
    ) -> (Vec<NameRecord>, Option<String>) {
        let limit = limit.clamp(1, MAX_DIRECTORY_PAGE);
        let after = |record: &&NameRecord| from.is_none_or(|from| record.name.as_str() > from);
        let mut entries: Vec<NameRecord> = self
            .names
            .values()
            .filter(after)
            .filter(|record| !verified_only || record.verified)
            .filter(|record| query.is_none_or(|query| record.name.contains(&query.to_lowercase())))
            .take(limit + 1)
            .cloned()
            .collect();
        let next = (entries.len() > limit).then(|| {
            entries.truncate(limit);
            entries.last().map(|record| record.name.clone())
        });
        (entries, next.flatten())
    }

    /// Names whose term has run out by `now`, to be released.
    pub fn lapsed(&self, now: u64) -> Vec<NameRecord> {
        self.names
            .values()
            .filter(|record| record.expires_at <= now)
            .cloned()
            .collect()
    }
}

/// Whether `name` is one lowercase DNS label of the allowed length.
pub fn check_syntax(name: &str) -> Result<(), NameError> {
    let good_chars = name
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    let length = (MIN_NAME_CHARS..=MAX_NAME_CHARS).contains(&name.len());
    if !good_chars || !length || name.starts_with('-') || name.ends_with('-') {
        return Err(NameError::BadName);
    }
    Ok(())
}

/// What a name looks like when the characters people confuse are made the
/// same and hyphens dropped. Two names with one skeleton are look-alikes.
pub fn skeleton(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        match ch {
            '-' => {}
            '0' => out.push('o'),
            '1' | 'i' | 'l' => out.push('l'),
            '5' => out.push('s'),
            '3' => out.push('e'),
            '4' => out.push('a'),
            '8' => out.push('b'),
            _ => out.push(ch),
        }
    }
    out.replace("rn", "m").replace("vv", "w")
}
