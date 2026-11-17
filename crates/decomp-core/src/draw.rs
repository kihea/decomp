//! Drawing the nodes for a job from public randomness, and the record of each
//! draw that the operator publishes.
//!
//! Spec section 4, "Public randomness": "The random draws for assignment and
//! spot checks come from a public source, such as the drand beacon or ICP's
//! verifiable randomness. Each draw is published, so anyone can recheck that
//! nodes were not hand-picked." Section 6: "Each operator publishes its draws,
//! so steering jobs to friendly nodes is detectable."
//!
//! The source is the drand beacon. A draw is worked out from one round of it,
//! the job's ID, and the nodes that could have been drawn, by a rule with no
//! other input. The published record holds all three and who was drawn, so
//! anyone can work the draw out again and compare.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::job::UNPROVEN_REDUNDANCY;
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

/// The share of jobs given to one proven node that a second node is also asked
/// to run, without either being told. A starting value.
pub const SPOT_CHECK_RATE: f64 = 0.10;

/// What a node's weight in a draw is counted in: millionths. A draw records
/// weights as whole numbers so that whoever works it out again starts from
/// exactly the numbers it was made with, which a fraction written out as text
/// and read back does not promise.
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

/// A node that could have been drawn for a job: idle, online, allowed the
/// job's tier, and not of an account the job is kept from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Eligible {
    pub node: PublicKey,
    /// Nodes of one account carry the same number here, and a job takes at
    /// most one of them. It is a count within this draw and names no account.
    pub owner: u32,
    /// The node's share of the draw: its reliability times its availability,
    /// in millionths.
    pub weight: u64,
    /// Whether the node is past probation.
    pub proven: bool,
}

/// The record of one draw, which the operator signs and publishes once the
/// job has closed. Until then it would tell a node whether its job is being
/// spot-checked.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Draw {
    pub job_id: JobId,
    /// When the draw was made, in seconds since 1970.
    pub at: u64,
    pub beacon: Beacon,
    /// How many nodes the job's submitter asked for, if it said.
    pub redundancy: Option<u32>,
    /// Every node that could have been drawn, in order of node key.
    pub eligible: Vec<Eligible>,
    /// The nodes drawn to run the job, in the order they were drawn.
    pub nodes: Vec<PublicKey>,
    /// The node drawn to run it as well, as a spot check, if one was.
    pub checker: Option<PublicKey>,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum DrawError {
    #[error("the nodes that could be drawn are not listed once each, in order of key")]
    NotInOrder,
    #[error("the beacon round was not the newest when the draw was made")]
    StaleRound,
    #[error("the rule draws other nodes from this round than the record names")]
    NotTheDraw,
}

impl Draw {
    /// Makes the draw for a job, or `None` if too few nodes could be drawn.
    /// `eligible` must be in order of node key.
    pub fn make(
        job_id: JobId,
        at: u64,
        beacon: Beacon,
        redundancy: Option<u32>,
        eligible: Vec<Eligible>,
    ) -> Option<Self> {
        let (nodes, checker) = drawn(&job_id, &beacon, redundancy, &eligible)?;
        Some(Self {
            job_id,
            at,
            beacon,
            redundancy,
            eligible,
            nodes,
            checker,
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
        let again = drawn(&self.job_id, &self.beacon, self.redundancy, &self.eligible);
        if again != Some((self.nodes.clone(), self.checker)) {
            return Err(DrawError::NotTheDraw);
        }
        Ok(())
    }
}

/// The rule: the nodes that run the job, then the spot checker if there is one.
///
/// A proven node runs a job alone; a node on probation is always paired. A job
/// left to one proven node is given to a second, as a spot check, with chance
/// `SPOT_CHECK_RATE`.
fn drawn(
    job_id: &JobId,
    beacon: &Beacon,
    redundancy: Option<u32>,
    eligible: &[Eligible],
) -> Option<(Vec<PublicKey>, Option<PublicKey>)> {
    let mut numbers = Numbers::new(job_id, beacon, eligible);
    let first = pick(eligible, &[], &mut numbers)?;
    let wanted = redundancy.unwrap_or(if eligible[first].proven {
        1
    } else {
        UNPROVEN_REDUNDANCY
    });
    let mut picked = vec![first];
    while picked.len() < wanted as usize {
        picked.push(pick(eligible, &picked, &mut numbers)?);
    }
    let spot_check = redundancy.is_none() && wanted == 1;
    let checker = (spot_check && numbers.next() < SPOT_CHECK_RATE)
        .then(|| pick(eligible, &picked, &mut numbers))
        .flatten();
    let node = |index: usize| eligible[index].node;
    Some((picked.into_iter().map(node).collect(), checker.map(node)))
}

/// Picks one node, with chances in proportion to weight, from those whose
/// owner has no node among `picked`.
fn pick(eligible: &[Eligible], picked: &[usize], numbers: &mut Numbers) -> Option<usize> {
    let free: Vec<usize> = (0..eligible.len())
        .filter(|&index| {
            let taken = |&other: &usize| eligible[other].owner == eligible[index].owner;
            !picked.iter().any(taken)
        })
        .collect();
    let total: u64 = free.iter().map(|&index| eligible[index].weight).sum();
    // A place along the weights laid end to end; the node it falls in is picked.
    let mut point = (numbers.next() * total as f64) as u64;
    for &index in &free {
        if point < eligible[index].weight {
            return Some(index);
        }
        point -= eligible[index].weight;
    }
    // Rounding can put `point` at the very end, past the last node.
    free.last().copied()
}

/// The numbers a draw is made with: each is a hash of the draw's seed and a
/// count, read as a fraction from 0 up to 1.
///
/// The seed is a hash of the round's randomness, the job's ID, and the nodes
/// that could be drawn. The last is there so that a node cannot work out
/// whether its own job is spot-checked: it knows the job's ID and can fetch
/// the round, but not the other nodes' weights.
struct Numbers {
    seed: [u8; 32],
    count: u64,
}

impl Numbers {
    fn new(job_id: &JobId, beacon: &Beacon, eligible: &[Eligible]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(b"decomp/draw/v1\n");
        hasher.update(beacon.randomness.as_bytes());
        hasher.update(job_id.as_bytes());
        for node in eligible {
            hasher.update(node.node.as_bytes());
            hasher.update(node.owner.to_le_bytes());
            hasher.update(node.weight.to_le_bytes());
            hasher.update([u8::from(node.proven)]);
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
