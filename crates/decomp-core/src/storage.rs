//! Storage as node work (spec section 2, the data class of jobs, and section 4,
//! "Storage challenge").
//!
//! An account pays to have a blob held for a number of days. The blob goes to a
//! few nodes that other devices can connect to, each from a different account.
//! Now and then each is challenged to produce one randomly chosen chunk, which is
//! checked against the blob's hash-tree root, so the coordinator never needs the
//! data itself. A node is paid for the time up to each challenge it passes.
//!
//! What must last (who holds what, and what has been paid) changes only by
//! applying entries, so a log of entries is a complete copy. Which challenges
//! are awaiting answers is not kept.

use std::collections::HashMap;

use rand::seq::SliceRandom;
use rand::{Rng, RngExt};
use serde::{Deserialize, Serialize};

use crate::hosts::Host;
use crate::keys::{AccountId, ContentHash, PublicKey};
use crate::ledger::{Credits, LedgerEntry};
use crate::merkle;

/// How many nodes hold each blob, when that many can be found.
pub const TARGET_REPLICAS: usize = 3;
/// The fewest nodes a blob may be placed on.
pub const MIN_REPLICAS: usize = 2;

/// What one node is paid, before the fee split, to hold one gibibyte for one day.
/// A starting value. The spec prices compute, at 1.5 cents a slot-hour, and not
/// storage; at that rate a credit is worth about 0.0004 cents, and cloud storage
/// costs about 0.08 cents a gigabyte-day, which comes to this many credits.
pub const CREDITS_PER_GIB_DAY: u64 = 180;

/// The longest a blob can be paid for at once.
pub const MAX_DAYS: u32 = 365;

/// How long a node has to answer a storage challenge.
pub const CHALLENGE_ANSWER_SECS: u64 = 60;

/// Challenges in a row a node may fail before its copy is counted as lost.
const FAILURES_BEFORE_LOST: u32 = 3;

const SECS_PER_DAY: u64 = 24 * 3600;
const BYTES_PER_GIB: u64 = 1 << 30;

/// What one node is paid, before the fee split, to hold `size` bytes for `secs`
/// seconds.
pub fn storage_fee(size: u64, secs: u64) -> Credits {
    let micros_per_gib_day = u128::from(Credits::whole(CREDITS_PER_GIB_DAY).micros());
    let work = u128::from(size) * u128::from(secs);
    let span = u128::from(BYTES_PER_GIB) * u128::from(SECS_PER_DAY);
    Credits::from_micros((work * micros_per_gib_day).div_ceil(span) as u64)
}

/// A change to what is stored where.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StorageEntry {
    /// A blob was accepted for storage and these nodes were asked to hold it.
    Placed {
        root: ContentHash,
        size: u64,
        owner: AccountId,
        replicas: Vec<Host>,
        until: u64,
    },
    /// A node received the blob. It is paid from this moment.
    Confirmed {
        root: ContentHash,
        node: PublicKey,
        at: u64,
    },
    /// A node passed a challenge, and is paid for the time since its last one.
    Passed {
        root: ContentHash,
        node: PublicKey,
        at: u64,
    },
    /// A node failed a challenge or did not answer it. That time is not paid for.
    Failed {
        root: ContentHash,
        node: PublicKey,
        at: u64,
    },
    /// The time paid for ran out. What was not paid to nodes returns to the owner.
    Ended { root: ContentHash },
}

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("this blob is already stored")]
    AlreadyStored,
    #[error("a blob is stored for between 1 and {MAX_DAYS} days")]
    BadDuration,
    #[error(
        "fewer than {MIN_REPLICAS} nodes that can hold data are online, each from a different account"
    )]
    NotEnoughNodes,
    #[error("no such stored blob")]
    UnknownBlob,
    #[error("this node was not asked to hold the blob")]
    NotAsked,
    #[error("this node has no challenge awaiting that answer")]
    NotChallenged,
}

/// A blob an account asks to have held.
pub struct Storing {
    pub root: ContentHash,
    pub size: u64,
    /// The account that pays.
    pub owner: AccountId,
    pub days: u32,
}

/// What a node is asked to hold.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoreOrder {
    pub root: ContentHash,
    pub size: u64,
    /// When the node may delete the blob, in seconds since 1970.
    pub until: u64,
}

/// A demand that a node produce one chunk of a blob it holds.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Challenge {
    pub root: ContentHash,
    pub chunk: u64,
}

/// Where a stored blob can be fetched from.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Located {
    pub size: u64,
    pub addresses: Vec<String>,
}

struct Deal {
    size: u64,
    until: u64,
    holders: Vec<Holder>,
}

struct Holder {
    replica: Host,
    state: Holding,
}

enum Holding {
    /// Asked to hold the blob; has not yet said it received it.
    Ordered,
    Held {
        /// The node has been paid for holding the blob up to this moment.
        paid_through: u64,
        last_challenged: u64,
        failures: u32,
    },
    /// Failed too many challenges in a row. Not challenged or paid again.
    Lost,
}

struct Awaited {
    challenge: Challenge,
    asked_at: u64,
}

/// What is stored where.
#[derive(Default)]
pub struct Storage {
    deals: HashMap<ContentHash, Deal>,
    /// The one challenge each node may have awaiting an answer.
    awaited: HashMap<PublicKey, Awaited>,
}

impl Storage {
    /// Applies one entry. Entries come from this type's own methods, so they
    /// always fit the state they are applied to.
    pub fn apply(&mut self, entry: &StorageEntry) {
        match entry {
            StorageEntry::Placed {
                root,
                size,
                replicas,
                until,
                ..
            } => {
                let holders = replicas
                    .iter()
                    .map(|replica| Holder {
                        replica: replica.clone(),
                        state: Holding::Ordered,
                    })
                    .collect();
                let deal = Deal {
                    size: *size,
                    until: *until,
                    holders,
                };
                self.deals.insert(*root, deal);
            }
            StorageEntry::Confirmed { root, node, at } => {
                if let Some(holder) = self.holder(root, node) {
                    holder.state = Holding::Held {
                        paid_through: *at,
                        last_challenged: *at,
                        failures: 0,
                    };
                }
            }
            StorageEntry::Passed { root, node, at } => {
                if let Some(holder) = self.holder(root, node) {
                    holder.state = Holding::Held {
                        paid_through: *at,
                        last_challenged: *at,
                        failures: 0,
                    };
                }
            }
            StorageEntry::Failed { root, node, at } => {
                if let Some(holder) = self.holder(root, node)
                    && let Holding::Held { failures, .. } = holder.state
                {
                    holder.state = if failures + 1 >= FAILURES_BEFORE_LOST {
                        Holding::Lost
                    } else {
                        Holding::Held {
                            paid_through: *at,
                            last_challenged: *at,
                            failures: failures + 1,
                        }
                    };
                }
            }
            StorageEntry::Ended { root } => {
                self.deals.remove(root);
            }
        }
    }

    /// Chooses which of the `online` hosts hold a blob. Returns the entry that
    /// records the choice and the ledger entry that sets the whole fee aside.
    pub fn place(
        &self,
        blob: Storing,
        mut online: Vec<Host>,
        now: u64,
        rng: &mut impl Rng,
    ) -> Result<(StorageEntry, LedgerEntry), StorageError> {
        let Storing {
            root,
            size,
            owner,
            days,
        } = blob;
        if self.deals.contains_key(&root) {
            return Err(StorageError::AlreadyStored);
        }
        if days == 0 || days > MAX_DAYS {
            return Err(StorageError::BadDuration);
        }
        online.shuffle(rng);
        let mut replicas: Vec<Host> = Vec::new();
        for host in online {
            if replicas.len() < TARGET_REPLICAS
                && replicas.iter().all(|chosen| chosen.account != host.account)
            {
                replicas.push(host);
            }
        }
        if replicas.len() < MIN_REPLICAS {
            return Err(StorageError::NotEnoughNodes);
        }

        let secs = u64::from(days) * SECS_PER_DAY;
        let per_node = storage_fee(size, secs);
        let amount = (0..replicas.len()).fold(Credits::ZERO, |sum, _| sum + per_node);
        let until = now + secs;
        Ok((
            StorageEntry::Placed {
                root,
                size,
                owner,
                replicas,
                until,
            },
            LedgerEntry::DataEscrow {
                id: root,
                account: owner,
                amount,
            },
        ))
    }

    /// The blobs `node` has been asked to hold and has not yet said it received.
    pub fn orders_for(&self, node: &PublicKey) -> Vec<StoreOrder> {
        self.deals
            .iter()
            .filter(|(_, deal)| {
                deal.holders.iter().any(|holder| {
                    holder.replica.node == *node && matches!(holder.state, Holding::Ordered)
                })
            })
            .map(|(root, deal)| StoreOrder {
                root: *root,
                size: deal.size,
                until: deal.until,
            })
            .collect()
    }

    /// A node says it has received a blob it was asked to hold.
    pub fn confirm(
        &mut self,
        root: ContentHash,
        node: PublicKey,
        now: u64,
    ) -> Result<StorageEntry, StorageError> {
        if !self.deals.contains_key(&root) {
            return Err(StorageError::UnknownBlob);
        }
        match self.holder(&root, &node) {
            Some(Holder {
                state: Holding::Ordered,
                ..
            }) => Ok(StorageEntry::Confirmed {
                root,
                node,
                at: now,
            }),
            _ => Err(StorageError::NotAsked),
        }
    }

    /// A challenge for `node`, if one of the blobs it holds has gone `interval`
    /// seconds without one. A node has at most one challenge awaiting an answer.
    pub fn challenge_for(
        &mut self,
        node: PublicKey,
        now: u64,
        interval: u64,
        rng: &mut impl Rng,
    ) -> Option<Challenge> {
        if self.awaited.contains_key(&node) {
            return None;
        }
        let (root, deal) = self.deals.iter().find(|(_, deal)| {
            deal.holders.iter().any(|holder| {
                holder.replica.node == node
                    && matches!(holder.state, Holding::Held { last_challenged, .. }
                        if now.saturating_sub(last_challenged) >= interval)
            })
        })?;
        let chunk = rng.random_range(0..merkle::chunk_count(deal.size));
        let challenge = Challenge { root: *root, chunk };
        self.awaited.insert(
            node,
            Awaited {
                challenge,
                asked_at: now,
            },
        );
        Some(challenge)
    }

    /// Checks a node's answer to its challenge. Returns the entry recording the
    /// outcome and, if it passed, the payment for the time since its last one.
    pub fn answer(
        &mut self,
        node: PublicKey,
        answered: Challenge,
        chunk: &[u8],
        proof: &[ContentHash],
        now: u64,
    ) -> Result<(StorageEntry, Option<LedgerEntry>), StorageError> {
        let awaited = self.awaited.get(&node).ok_or(StorageError::NotChallenged)?;
        let asked = awaited.challenge;
        if asked.root != answered.root || asked.chunk != answered.chunk {
            return Err(StorageError::NotChallenged);
        }
        self.awaited.remove(&node);

        let root = asked.root;
        let deal = self.deals.get(&root).ok_or(StorageError::UnknownBlob)?;
        if !merkle::verify(&root, deal.size, asked.chunk, chunk, proof) {
            return Ok((
                StorageEntry::Failed {
                    root,
                    node,
                    at: now,
                },
                None,
            ));
        }
        let holder = deal
            .holders
            .iter()
            .find(|holder| holder.replica.node == node);
        let payment = match holder {
            Some(Holder {
                replica,
                state: Holding::Held { paid_through, .. },
            }) => {
                let held_for = now.min(deal.until).saturating_sub(*paid_through);
                let amount = storage_fee(deal.size, held_for);
                (amount > Credits::ZERO).then_some(LedgerEntry::DataPay {
                    id: root,
                    account: replica.account,
                    amount,
                })
            }
            _ => None,
        };
        Ok((
            StorageEntry::Passed {
                root,
                node,
                at: now,
            },
            payment,
        ))
    }

    /// What time has decided: challenges nobody answered have failed, and blobs
    /// whose paid time has run out are ended and what is left is refunded.
    pub fn sweep(&mut self, now: u64) -> Vec<(StorageEntry, Option<LedgerEntry>)> {
        let mut entries = Vec::new();
        self.awaited.retain(|node, awaited| {
            let overdue = now.saturating_sub(awaited.asked_at) > CHALLENGE_ANSWER_SECS;
            if overdue {
                let root = awaited.challenge.root;
                entries.push((
                    StorageEntry::Failed {
                        root,
                        node: *node,
                        at: now,
                    },
                    None,
                ));
            }
            !overdue
        });
        for (root, deal) in &self.deals {
            if deal.until <= now {
                let refund = LedgerEntry::DataRefund { id: *root };
                entries.push((StorageEntry::Ended { root: *root }, Some(refund)));
            }
        }
        entries
    }

    /// Where a stored blob can be fetched from: the nodes known to hold it.
    pub fn locate(&self, root: &ContentHash) -> Option<Located> {
        let deal = self.deals.get(root)?;
        let addresses = deal
            .holders
            .iter()
            .filter(|holder| matches!(holder.state, Holding::Held { .. }))
            .map(|holder| holder.replica.address.clone())
            .collect();
        Some(Located {
            size: deal.size,
            addresses,
        })
    }

    fn holder(&mut self, root: &ContentHash, node: &PublicKey) -> Option<&mut Holder> {
        let deal = self.deals.get_mut(root)?;
        deal.holders
            .iter_mut()
            .find(|holder| holder.replica.node == *node)
    }
}
