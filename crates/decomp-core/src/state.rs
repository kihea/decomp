//! Application state (spec section 7, "State").
//!
//! An app's state is cut into shards, each a set of records with a name the app
//! chooses. A shard's contents are a blob like any other, named by the hash of
//! its one byte form (see `decomp_schema::records`). All that is kept here is
//! which contents each shard has now.
//!
//! A job can be handed one shard. It reads the records as they were when it was
//! handed out, and the records it leaves behind come back in its result, checked
//! like its output. Jobs on the same shard run one at a time, each starting from
//! what the one before left, so no job's changes are lost to another's.
//!
//! Which contents a shard has changes only by applying entries, so a log of
//! entries is a complete copy.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::keys::{AccountId, ContentHash};

/// The longest name a shard may have, in bytes.
pub const MAX_SHARD_NAME_BYTES: usize = 64;

/// One shard of one app's state.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShardRef {
    /// The app, by its ID.
    pub app: AccountId,
    /// The shard's name within the app.
    pub shard: String,
}

impl ShardRef {
    /// Whether the shard's name is one a shard may have.
    pub fn is_well_named(&self) -> bool {
        !self.shard.is_empty() && self.shard.len() <= MAX_SHARD_NAME_BYTES
    }
}

/// A verified job left a shard with new contents.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StateEntry {
    #[serde(flatten)]
    pub shard: ShardRef,
    pub contents: ContentHash,
}

/// The contents every shard has now. A shard no job has written to is not here,
/// and is empty.
#[derive(Default)]
pub struct States {
    contents: HashMap<ShardRef, ContentHash>,
}

impl States {
    pub fn apply(&mut self, entry: &StateEntry) {
        self.contents.insert(entry.shard.clone(), entry.contents);
    }

    /// The hash of the shard's contents, or `None` if it is empty.
    pub fn contents(&self, shard: &ShardRef) -> Option<ContentHash> {
        self.contents.get(shard).copied()
    }
}
