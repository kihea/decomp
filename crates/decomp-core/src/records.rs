//! Records: small pieces of state a user or an app keeps on the network (spec,
//! "Data management", "Records").
//!
//! "A record is a key, a version number, the hash of its value, and the
//! writer's signature. A write names the version it read and is accepted only
//! if that version is still current." The value itself is a blob; the record
//! is the pointer to it, and the check runs where the pointer is kept, so two
//! devices or jobs writing the same record cannot both win: the loser reads
//! again and retries.
//!
//! Records are kept per account. Any device of the account may write them; any
//! signer may read them, so values that must stay private are written sealed.
//! Like `Accounts`, records change only by applying entries, so a log of
//! entries is a complete copy.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::keys::{AccountId, ContentHash, PublicKey};

/// The longest key, in bytes.
pub const MAX_RECORD_KEY_BYTES: usize = 256;

/// The most records one account may keep. Each write is a call to the control
/// plane, so high-volume state belongs in fewer, larger records.
pub const MAX_RECORDS_PER_ACCOUNT: usize = 10_000;

/// The most records one listing returns.
pub const MAX_RECORD_PAGE: usize = 256;

/// One record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub owner: AccountId,
    pub key: String,
    /// Starts at 1 with the first write and rises by one with each.
    pub version: u64,
    /// The hash of the blob that holds the value.
    pub value: ContentHash,
    /// The device that wrote this version. Its signed request is kept with the
    /// record's log entry.
    pub writer: PublicKey,
    /// When this version was written, in seconds since 1970.
    pub at: u64,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum RecordError {
    #[error("a record's key is 1 to 256 bytes")]
    BadKey,
    #[error("the record is at version {current}; read it again and retry")]
    StaleVersion { current: u64 },
    #[error("an account keeps at most 10,000 records")]
    TooMany,
    #[error("no such record")]
    UnknownRecord,
}

/// Every account's records.
#[derive(Clone, Debug, Default)]
pub struct Records {
    by_owner: BTreeMap<AccountId, BTreeMap<String, Record>>,
}

impl Records {
    /// Makes the record that writing `value` to `key` would leave, if the write
    /// names the version that is current: 0 for a key never written.
    pub fn write(
        &self,
        owner: AccountId,
        key: String,
        read_version: u64,
        value: ContentHash,
        writer: PublicKey,
        at: u64,
    ) -> Result<Record, RecordError> {
        if key.is_empty() || key.len() > MAX_RECORD_KEY_BYTES {
            return Err(RecordError::BadKey);
        }
        let owned = self.by_owner.get(&owner);
        let current = owned
            .and_then(|records| records.get(&key))
            .map_or(0, |record| record.version);
        if read_version != current {
            return Err(RecordError::StaleVersion { current });
        }
        if current == 0 && owned.is_some_and(|records| records.len() >= MAX_RECORDS_PER_ACCOUNT) {
            return Err(RecordError::TooMany);
        }
        Ok(Record {
            owner,
            key,
            version: current + 1,
            value,
            writer,
            at,
        })
    }

    /// Takes in a record that `write` made.
    pub fn apply(&mut self, record: Record) {
        self.by_owner
            .entry(record.owner)
            .or_default()
            .insert(record.key.clone(), record);
    }

    pub fn get(&self, owner: &AccountId, key: &str) -> Option<&Record> {
        self.by_owner.get(owner)?.get(key)
    }

    /// An account's records whose keys start with `prefix`, in order of key,
    /// from just after `from`.
    pub fn list(&self, owner: &AccountId, prefix: &str, from: Option<&str>) -> Vec<Record> {
        let Some(records) = self.by_owner.get(owner) else {
            return Vec::new();
        };
        records
            .range(prefix.to_string()..)
            .take_while(|(key, _)| key.starts_with(prefix))
            .filter(|(key, _)| from.is_none_or(|from| key.as_str() > from))
            .take(MAX_RECORD_PAGE)
            .map(|(_, record)| record.clone())
            .collect()
    }
}
