//! Drawing which asking node is granted a lease, from public randomness, and
//! the record of each draw that the job board publishes.
//!
//! Spec section 4, "Public randomness": "The random draws for assignment and
//! spot checks come from a public source, such as the drand beacon or ICP's
//! verifiable randomness. Each draw is published, so anyone can recheck that
//! nodes were not hand-picked."
//!
//! A draw is worked out from one round of the beacon, the job's ID, which
//! pickup it is, and the nodes that could have been granted the lease, by a
//! rule with no other input. The published record holds all of them and who
//! was drawn, so anyone can work the draw out again and compare.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::keys::{ContentHash, JobId, PublicKey};

/// The drand network draws are made from: its "quicknet" chain, by the hash
/// that names it, when its first round was, and how long a round lasts.
pub const BEACON_CHAIN: &str = "52db9ba70e0cc0f6eaf7803dd07447a1f5477735fd3f661792ba94600c84e971";
const BEACON_GENESIS_SECS: u64 = 1_692_803_367;
pub const BEACON_PERIOD_SECS: u64 = 3;

/// How many rounds behind the newest a draw's round may be. It allows for the
/// time it takes to fetch a round. Any older, and the randomness of a draw
/// would have been public for long enough to choose a moment by.
pub const BEACON_MAX_LAG_ROUNDS: u64 = 3;

/// The share of unconfirmed results that are audited by a spot check. A
/// starting value.
pub const SPOT_CHECK_RATE: f64 = 0.10;

/// What a node's weight in a draw is counted in: millionths. A draw records
/// weights as whole numbers so that whoever works it out again starts from
/// exactly the numbers it was made with.
pub const WEIGHT_SCALE: f64 = 1_000_000.0;

/// One round of the beacon: its number and the 32 random bytes it gave.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Beacon {
    pub round: u64,
    pub randomness: ContentHash,
}

impl Beacon {
    /// Whether this round was the beacon's newest at `time`, or close behind it.
    pub fn is_current(&self, time: u64) -> bool {
        let newest = round_at(time);
        self.round <= newest && newest - self.round <= BEACON_MAX_LAG_ROUNDS
    }
}

/// The number of the beacon's newest round at `time`, in seconds since 1970.
pub fn round_at(time: u64) -> u64 {
    time.saturating_sub(BEACON_GENESIS_SECS) / BEACON_PERIOD_SECS + 1
}

/// Which lease on a job a draw grants.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Pickup {
    /// The first lease.
    First,
    /// A second lease, whose result confirms or disputes the first.
    Second,
    /// A spot check of a result accepted unconfirmed.
    Audit,
}

/// A node that could have been granted the lease: it asked for work, may take
/// the job's class and tier, accepts it under its owner's preferences, is
/// under its account's share cap, and is not of an account the job is kept
/// from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Eligible {
    pub node: PublicKey,
    /// The node's share of the draw: reliability times availability times
    /// hardware fit, in millionths.
    pub weight: u64,
    /// The node's account also spends credits in spaces, so its request is
    /// served first (spec section 5, "Balanced users first").
    pub balanced: bool,
}

/// The record of one draw, which the job board publishes once the job has
/// closed. Until then it would tell a node whether its job is being checked.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Draw {
    pub job_id: JobId,
    pub pickup: Pickup,
    /// When the draw was made, in seconds since 1970.
    pub at: u64,
    pub beacon: Beacon,
    /// Every node that could have been drawn, in order of node key.
    pub eligible: Vec<Eligible>,
    /// The node granted the lease.
    pub node: PublicKey,
    /// The bid the lease filled, if the job board filled one. A filled bid is
    /// chosen by price, so its draw only breaks ties among the lowest bids.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bid: Option<ContentHash>,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum DrawError {
    #[error("the nodes that could be drawn are not listed once each, in order of key")]
    NotInOrder,
    #[error("the beacon round was not the newest when the draw was made")]
    StaleRound,
    #[error("the rule draws another node from this round than the record names")]
    NotTheDraw,
}

impl Draw {
    /// Makes a draw, or `None` if no node could be drawn. `eligible` must be in
    /// order of node key.
    pub fn make(
        job_id: JobId,
        pickup: Pickup,
        at: u64,
        beacon: Beacon,
        eligible: Vec<Eligible>,
        bid: Option<ContentHash>,
    ) -> Option<Self> {
        let node = drawn(&job_id, pickup, &beacon, &eligible)?;
        Some(Self {
            job_id,
            pickup,
            at,
            beacon,
            eligible,
            node,
            bid,
        })
    }

    /// Works the draw out again and compares. This does not show that the
    /// round's randomness is the beacon's: whoever checks must fetch the round
    /// from the beacon and compare that too.
    pub fn check(&self) -> Result<(), DrawError> {
        let in_order = |pair: &[Eligible]| pair[0].node < pair[1].node;
        if !self.eligible.windows(2).all(in_order) {
            return Err(DrawError::NotInOrder);
        }
        if !self.beacon.is_current(self.at) {
            return Err(DrawError::StaleRound);
        }
        if drawn(&self.job_id, self.pickup, &self.beacon, &self.eligible) != Some(self.node) {
            return Err(DrawError::NotTheDraw);
        }
        Ok(())
    }
}

/// Whether a result accepted unconfirmed is audited, worked out from the round
/// it was accepted in and the job's ID, so anyone can recheck it once the job
/// has closed. The node does not learn of it until the audit is done.
pub fn is_audited(job_id: &JobId, beacon: &Beacon) -> bool {
    let mut numbers = Numbers::new(job_id, Pickup::Audit, beacon, &[]);
    numbers.next() < SPOT_CHECK_RATE
}

/// The rule: one node, with chances in proportion to weight, from the balanced
/// accounts' nodes if any could be drawn, else from all of them.
fn drawn(
    job_id: &JobId,
    pickup: Pickup,
    beacon: &Beacon,
    eligible: &[Eligible],
) -> Option<PublicKey> {
    let balanced: Vec<&Eligible> = eligible.iter().filter(|node| node.balanced).collect();
    let pool: Vec<&Eligible> = if balanced.is_empty() {
        eligible.iter().collect()
    } else {
        balanced
    };
    let last = pool.last()?;
    let mut numbers = Numbers::new(job_id, pickup, beacon, eligible);
    let total: u64 = pool.iter().map(|node| node.weight).sum();
    // A place along the weights laid end to end; the node it falls in is drawn.
    let mut point = (numbers.next() * total as f64) as u64;
    for node in &pool {
        if point < node.weight {
            return Some(node.node);
        }
        point -= node.weight;
    }
    // Rounding can put `point` at the very end, past the last node.
    Some(last.node)
}

/// The numbers a draw is made with: each is a hash of the draw's seed and a
/// count, read as a fraction from 0 up to 1.
///
/// The seed is a hash of the round's randomness, the job's ID, the pickup, and
/// the nodes that could be drawn. The last is there so that a node cannot work
/// out a draw ahead of time: it knows the job's ID and can fetch the round,
/// but not who else is asking.
struct Numbers {
    seed: [u8; 32],
    count: u64,
}

impl Numbers {
    fn new(job_id: &JobId, pickup: Pickup, beacon: &Beacon, eligible: &[Eligible]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(b"decomp/draw/v2\n");
        hasher.update(beacon.randomness.as_bytes());
        hasher.update(job_id.as_bytes());
        hasher.update([pickup as u8]);
        for node in eligible {
            hasher.update(node.node.as_bytes());
            hasher.update(node.weight.to_le_bytes());
            hasher.update([u8::from(node.balanced)]);
        }
        Self {
            seed: hasher.finalize().into(),
            count: 0,
        }
    }

    fn next(&mut self) -> f64 {
        let mut hasher = Sha256::new();
        hasher.update(self.seed);
        hasher.update(self.count.to_le_bytes());
        self.count += 1;
        let hash: [u8; 32] = hasher.finalize().into();
        let bits = u64::from_le_bytes(hash[..8].try_into().expect("eight bytes"));
        // The top 53 bits, which is as many as a fraction below 1 can hold.
        (bits >> 11) as f64 / (1u64 << 53) as f64
    }
}
