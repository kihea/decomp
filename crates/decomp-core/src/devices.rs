//! Proofs that a device belongs to an account, which a job can check (spec
//! section 1, "Account identity in jobs").
//!
//! Every action in an app is signed by a device key, but an app may know its
//! users by account ID. The control plane keeps a Merkle tree of every
//! device that has not been revoked and the account it belongs to, and posts
//! the tree's root to the settlement log, signed like every entry there
//! (`Posted::Devices`). A device asks for its own proof: its account, that
//! signed entry, and the hashes beside its path in the tree. Checking a
//! proof needs the control plane's public key and one signature check, so a
//! job can do it, with no call to the control plane.
//!
//! A proof is only as fresh as the entry it rests on. It is taken for
//! actions within `DEVICE_PROOF_LIFETIME_SECS` of that entry, so a revoked
//! device is still taken as its account's for at most that long. The root is
//! posted when the devices change and at least every
//! `DEVICE_ROOT_REFRESH_SECS`, so a device that is still its account's can
//! always get a proof that is good.
//!
//! The tree is a binary trie on the hash of each device key, in which a
//! subtree holding one device is that device's leaf, so a path is about as
//! long as the base-2 logarithm of the number of devices. Only the device
//! itself is given its proof, so the tree reveals no account's devices.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::settlement::{LogEntry, Posted};
use crate::{AccountId, ContentHash, Envelope, PublicKey};

/// How far apart a proof's entry and the action it is used for may be, in
/// seconds: the longest a revoked device is still taken as its account's.
pub const DEVICE_PROOF_LIFETIME_SECS: u64 = 60 * 60;

/// How often the devices root is posted even when no device changed, in
/// seconds, so that a proof posted no longer ago than this can be had.
pub const DEVICE_ROOT_REFRESH_SECS: u64 = DEVICE_PROOF_LIFETIME_SECS / 2;

/// The deepest a path can be: one level for each bit of a device's hash.
const MAX_DEPTH: usize = 256;

const LEAF: u8 = 0;
const NODE: u8 = 1;

/// The hash of an empty subtree.
const EMPTY: ContentHash = ContentHash::from_bytes([0; 32]);

/// Where a device sits in the tree: the hash of its key.
fn path(device: &PublicKey) -> [u8; 32] {
    *ContentHash::of(device.as_bytes()).as_bytes()
}

fn bit(path: &[u8; 32], depth: usize) -> bool {
    path[depth / 8] >> (7 - depth % 8) & 1 == 1
}

fn leaf(device: &PublicKey, account: &AccountId) -> ContentHash {
    let mut hasher = Sha256::new();
    hasher.update([LEAF]);
    hasher.update(device.as_bytes());
    hasher.update(account.as_bytes());
    ContentHash::from_bytes(hasher.finalize().into())
}

fn node(left: &ContentHash, right: &ContentHash) -> ContentHash {
    let mut hasher = Sha256::new();
    hasher.update([NODE]);
    hasher.update(left.as_bytes());
    hasher.update(right.as_bytes());
    ContentHash::from_bytes(hasher.finalize().into())
}

/// Every device that has not been revoked, and its account.
#[derive(Clone, Debug)]
pub struct DeviceTree {
    /// Each device's path and leaf, in order of path.
    leaves: Vec<([u8; 32], PublicKey, AccountId)>,
    root: ContentHash,
}

impl DeviceTree {
    pub fn new(devices: impl IntoIterator<Item = (PublicKey, AccountId)>) -> Self {
        let mut leaves: Vec<_> = devices
            .into_iter()
            .map(|(device, account)| (path(&device), device, account))
            .collect();
        leaves.sort_by_key(|leaf| leaf.0);
        let root = subtree(&leaves, 0);
        DeviceTree { leaves, root }
    }

    pub fn root(&self) -> ContentHash {
        self.root
    }

    /// `device`'s account and the hashes beside its path, from the root
    /// down. `None` if the device is not in the tree.
    pub fn prove(&self, device: &PublicKey) -> Option<(AccountId, Vec<ContentHash>)> {
        let wanted = path(device);
        let place = self
            .leaves
            .binary_search_by(|(path, _, _)| path.cmp(&wanted))
            .ok()?;
        let mut slice = &self.leaves[..];
        let mut siblings = Vec::new();
        let mut depth = 0;
        while slice.len() > 1 {
            let (left, right) = split(slice, depth);
            if bit(&wanted, depth) {
                siblings.push(subtree(left, depth + 1));
                slice = right;
            } else {
                siblings.push(subtree(right, depth + 1));
                slice = left;
            }
            depth += 1;
        }
        Some((self.leaves[place].2, siblings))
    }
}

/// The leaves under a node at `depth`, split by the next bit of their paths.
type Leaves<'a> = &'a [([u8; 32], PublicKey, AccountId)];

fn split(leaves: Leaves<'_>, depth: usize) -> (Leaves<'_>, Leaves<'_>) {
    let middle = leaves.partition_point(|(path, _, _)| !bit(path, depth));
    leaves.split_at(middle)
}

fn subtree(leaves: Leaves<'_>, depth: usize) -> ContentHash {
    match leaves {
        [] => EMPTY,
        [(_, device, account)] => leaf(device, account),
        _ => {
            let (left, right) = split(leaves, depth);
            node(&subtree(left, depth + 1), &subtree(right, depth + 1))
        }
    }
}

/// Whether `siblings` show `device` to belong to `account` in the tree whose
/// root is `root`.
pub fn holds(
    root: &ContentHash,
    device: &PublicKey,
    account: &AccountId,
    siblings: &[ContentHash],
) -> bool {
    if siblings.len() > MAX_DEPTH {
        return false;
    }
    let path = path(device);
    let mut hash = leaf(device, account);
    for (depth, sibling) in siblings.iter().enumerate().rev() {
        hash = if bit(&path, depth) {
            node(sibling, &hash)
        } else {
            node(&hash, sibling)
        };
    }
    hash == *root
}

/// What a device hands an app to show which account it belongs to.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceProof {
    pub account: AccountId,
    /// The settlement log's entry posting the devices root, as the control
    /// plane signed it.
    pub entry: Envelope,
    /// The hashes beside the device's path in the tree, from the root down.
    pub siblings: Vec<ContentHash>,
}

impl DeviceProof {
    /// The account `device` belongs to, if the proof shows it under the
    /// control plane `control`, for an action at time `at` (in seconds since
    /// 1970). `at` must be a time the checker trusts, such as a game tick's,
    /// not one the device chose.
    pub fn account_of(
        &self,
        device: &PublicKey,
        control: &PublicKey,
        at: u64,
    ) -> Option<AccountId> {
        if self.entry.signer != *control {
            return None;
        }
        let LogEntry {
            at: posted_at,
            posted,
            ..
        } = self.entry.open().ok()?;
        let Posted::Devices { root } = posted else {
            return None;
        };
        if at.abs_diff(posted_at) > DEVICE_PROOF_LIFETIME_SECS {
            return None;
        }
        holds(&root, device, &self.account, &self.siblings).then_some(self.account)
    }
}
