//! Storage as node work (spec section 2, the data class of jobs, and section 4,
//! "Storage challenge").
//!
//! An account pays to have a blob held for a number of days. The blob goes to a
//! few nodes that other devices can connect to, each from a different account,
//! chosen by hashing the blob's name against each node's key (spec, "Blobs":
//! "Any client can compute where a chunk lives, so there is no central
//! index"). Given the list of storage nodes, `placement_order` gives the same
//! answer to everyone.
//! Now and then each is challenged to produce one randomly chosen chunk, which is
//! checked against the blob's hash-tree root, so the control plane never needs the
//! data itself. A node is paid for the time up to each challenge it passes.
//!
//! What must last (who holds what, and what has been paid) changes only by
//! applying entries, so a log of entries is a complete copy. Which challenges
//! are awaiting answers is not kept.

use std::collections::HashMap;

use rand::{Rng, RngExt};
use serde::{Deserialize, Serialize};

use crate::hosts::Host;
use crate::keys::{AccountId, ContentHash, PublicKey};
use crate::ledger::{Credits, LedgerEntry};
use crate::merkle;

/// How many nodes hold each blob, when that many can be found.
pub const TARGET_REPLICAS: usize = 3;

/// How long the blobs a job produces (its output, the blobs it stored, its
/// shard's new contents, its follow-ups' inputs) are kept, paid by the job's
/// payer. The submitter fetches them in that time, or stores them again for
/// longer. A starting value.
pub const PRODUCED_RETENTION_DAYS: u32 = 7;
/// The fewest nodes a blob may be placed on.
pub const MIN_REPLICAS: usize = 2;

/// What one node is paid, before the fee split, to hold one gibibyte for one
/// day, in microcredits. A starting value: the spec rents storage by the
/// gigabyte-month and leaves the rate to testing. A credit buys about one
/// slot-hour, which the spec's economics put near 1.5 cents, and cloud storage
/// costs about 0.08 cents a gigabyte-day, which comes to about 0.057 credits a
/// gibibyte-day (1.7 credits a gibibyte-month).
pub const MICROS_PER_GIB_DAY: u64 = 57_000;

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
    let micros_per_gib_day = u128::from(MICROS_PER_GIB_DAY);
    let work = u128::from(size) * u128::from(secs);
    let span = u128::from(BYTES_PER_GIB) * u128::from(SECS_PER_DAY);
    Credits::from_micros((work * micros_per_gib_day).div_ceil(span) as u64)
}

/// The storage nodes in the order a blob named `root` is placed on them: by
/// the hash of the blob's name and each node's key, highest first. Each node's
/// place depends only on the two, so adding or losing a node moves few blobs,
/// and anyone with the list of storage nodes works out the same order.
pub fn placement_order(root: &ContentHash, hosts: Vec<Host>) -> Vec<Host> {
    let score = |host: &Host| {
        let mut bytes = b"decomp/place/v1
"
        .to_vec();
        bytes.extend_from_slice(root.as_bytes());
        bytes.extend_from_slice(host.node.as_bytes());
        ContentHash::of(&bytes)
    };
    let mut scored: Vec<(ContentHash, Host)> =
        hosts.into_iter().map(|host| (score(&host), host)).collect();
    scored.sort_by(|a, b| b.0.as_bytes().cmp(a.0.as_bytes()));
    scored.into_iter().map(|(_, host)| host).collect()
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
        /// Who the blob was left for, if it was left for a device.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        addressed: Option<Addressed>,
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
    /// The time paid for ran out, or the owner deleted the blob. What was not
    /// paid to nodes returns to the owner.
    Ended { root: ContentHash },
    /// A replica was lost, and another node is asked to copy the blob from a
    /// node that still holds it (spec, "Blobs": "When a storage challenge
    /// fails, a repair job copies the chunk to a new node"). It is paid from
    /// what the lost replica no longer earns.
    Repaired { root: ContentHash, replica: Host },
}

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("this blob is already stored")]
    AlreadyStored,
    #[error("only the account that stored a blob can delete it")]
    NotOwner,
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
    /// Who the blob is left for, if it is left for a device.
    pub addressed: Option<Addressed>,
}

/// A stored blob left by one device for another, which is told it is waiting.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Addressed {
    pub from: PublicKey,
    pub to: PublicKey,
}

/// A stored blob waiting for a device.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Waiting {
    pub root: ContentHash,
    pub size: u64,
    /// The device that left it, as the coordinator recorded. What is inside
    /// should say for itself who it is from.
    pub from: PublicKey,
}

/// What a node is asked to hold.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoreOrder {
    pub root: ContentHash,
    pub size: u64,
    /// When the node may delete the blob, in seconds since 1970.
    pub until: u64,
    /// The node fetches the blob from the nodes that hold it, instead of
    /// waiting for it to be uploaded: a lost replica is being replaced.
    #[serde(default)]
    pub repair: bool,
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
    owner: AccountId,
    size: u64,
    until: u64,
    holders: Vec<Holder>,
    addressed: Option<Addressed>,
}

struct Holder {
    replica: Host,
    state: Holding,
    /// The holder replaces a lost replica, and copies the blob from the others.
    repair: bool,
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
    /// Blobs each node should drop, because their deal ended, until the node
    /// is told.
    drops: HashMap<PublicKey, Vec<ContentHash>>,
}

impl Storage {
    /// Applies one entry. Entries come from this type's own methods, so they
    /// always fit the state they are applied to.
    pub fn apply(&mut self, entry: &StorageEntry) {
        match entry {
            StorageEntry::Placed {
                root,
                size,
                owner,
                replicas,
                until,
                addressed,
            } => {
                let holders = replicas
                    .iter()
                    .map(|replica| Holder {
                        replica: replica.clone(),
                        state: Holding::Ordered,
                        repair: false,
                    })
                    .collect();
                let deal = Deal {
                    owner: *owner,
                    size: *size,
                    until: *until,
                    holders,
                    addressed: *addressed,
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
            StorageEntry::Repaired { root, replica } => {
                if let Some(deal) = self.deals.get_mut(root) {
                    deal.holders.push(Holder {
                        replica: replica.clone(),
                        state: Holding::Ordered,
                        repair: true,
                    });
                }
            }
            StorageEntry::Ended { root } => {
                if let Some(deal) = self.deals.remove(root) {
                    for holder in deal.holders {
                        self.drops
                            .entry(holder.replica.node)
                            .or_default()
                            .push(*root);
                    }
                }
            }
        }
    }

    /// Ends a deal at its owner's request (spec, "Blobs": "The owner signs a
    /// delete, the pointers are removed, and nodes drop the chunks"). Time
    /// already paid stays paid; what is left of the fee returns to the owner.
    /// The nodes are told to drop the blob when they next ask for work.
    pub fn delete(
        &self,
        root: &ContentHash,
        owner: &AccountId,
    ) -> Result<(StorageEntry, LedgerEntry), StorageError> {
        let deal = self.deals.get(root).ok_or(StorageError::UnknownBlob)?;
        if deal.owner != *owner {
            return Err(StorageError::NotOwner);
        }
        Ok((
            StorageEntry::Ended { root: *root },
            LedgerEntry::DataRefund { id: *root },
        ))
    }

    /// The blobs `node` should drop, which it is now told of.
    pub fn take_drops(&mut self, node: &PublicKey) -> Vec<ContentHash> {
        self.drops.remove(node).unwrap_or_default()
    }

    /// Chooses which of the `online` hosts hold a blob. Returns the entry that
    /// records the choice and the ledger entry that sets the whole fee aside.
    pub fn place(
        &self,
        blob: Storing,
        online: Vec<Host>,
        now: u64,
    ) -> Result<(StorageEntry, LedgerEntry), StorageError> {
        let Storing {
            root,
            size,
            owner,
            days,
            addressed,
        } = blob;
        if self.deals.contains_key(&root) {
            return Err(StorageError::AlreadyStored);
        }
        if days == 0 || days > MAX_DAYS {
            return Err(StorageError::BadDuration);
        }
        let mut replicas: Vec<Host> = Vec::new();
        for host in placement_order(&root, online) {
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
                addressed,
            },
            LedgerEntry::DataEscrow {
                id: root,
                account: owner,
                amount,
            },
        ))
    }

    /// The stored blobs left for `device` that it can fetch now. A blob stays
    /// here until its paid time runs out, whether or not the device has fetched it.
    pub fn waiting_for(&self, device: &PublicKey) -> Vec<Waiting> {
        self.deals
            .iter()
            .filter(|(_, deal)| {
                deal.holders
                    .iter()
                    .any(|holder| matches!(holder.state, Holding::Held { .. }))
            })
            .filter_map(|(root, deal)| {
                let addressed = deal.addressed.filter(|addressed| addressed.to == *device)?;
                Some(Waiting {
                    root: *root,
                    size: deal.size,
                    from: addressed.from,
                })
            })
            .collect()
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
                repair: deal
                    .holders
                    .iter()
                    .any(|holder| holder.replica.node == *node && holder.repair),
            })
            .collect()
    }

    /// The repairs to make now: for each blob that has lost replicas and still
    /// has a node holding it, the next node in its placement order that is
    /// online and of an account not already holding it.
    pub fn repairs(&self, online: &[Host]) -> Vec<StorageEntry> {
        let mut repairs = Vec::new();
        for (root, deal) in &self.deals {
            let live = deal
                .holders
                .iter()
                .filter(|holder| !matches!(holder.state, Holding::Lost));
            let held = live
                .clone()
                .any(|holder| matches!(holder.state, Holding::Held { .. }));
            if !held || live.count() >= TARGET_REPLICAS {
                continue;
            }
            let taken = |host: &Host| {
                deal.holders.iter().any(|holder| {
                    holder.replica.account == host.account || holder.replica.node == host.node
                })
            };
            let next = placement_order(root, online.to_vec())
                .into_iter()
                .find(|host| !taken(host));
            if let Some(replica) = next {
                repairs.push(StorageEntry::Repaired {
                    root: *root,
                    replica,
                });
            }
        }
        repairs
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

    /// Holders that have gone a whole interval, and the time to answer, without
    /// being challenged: they stopped asking for work, so they could not be
    /// asked. Each such stretch counts as a failed challenge, so a node that
    /// left for good is counted as lost and its replica repaired.
    pub fn overdue(&self, now: u64, interval: u64) -> Vec<StorageEntry> {
        let mut failed = Vec::new();
        for (root, deal) in &self.deals {
            for holder in &deal.holders {
                let Holding::Held {
                    last_challenged, ..
                } = holder.state
                else {
                    continue;
                };
                let silent =
                    now.saturating_sub(last_challenged) >= interval + CHALLENGE_ANSWER_SECS;
                if silent && !self.awaited.contains_key(&holder.replica.node) {
                    failed.push(StorageEntry::Failed {
                        root: *root,
                        node: holder.replica.node,
                        at: now,
                    });
                }
            }
        }
        failed
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
                ..
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

    /// Whether a blob is held by at least one node, so it can be fetched.
    pub fn is_stored(&self, root: &ContentHash) -> bool {
        self.locate(root)
            .is_some_and(|located| !located.addresses.is_empty())
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
