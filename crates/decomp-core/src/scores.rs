//! How far each node is trusted (spec section 1 "Node profile" and section 4
//! "Earning spot checks").
//!
//! Scores change only by applying events, so a log of events is a complete copy.
//! Two of the spec's four scores are kept here. Bandwidth and hardware fit need
//! measurements that nothing takes yet.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::job::MAX_TIER;
use crate::keys::PublicKey;
use crate::ledger::Credits;

/// Matching results a node needs before it leaves probation.
pub const PROBATION_RESULTS: u32 = 20;

/// The score every node starts with.
const STARTING_SCORE: f64 = 0.5;
/// How far one result moves a score toward 1 (good) or 0 (bad).
const SCORE_STEP: f64 = 0.05;
/// What a wrong result multiplies reliability by. The spec asks for a sharp drop.
const WRONG_RESULT_FACTOR: f64 = 0.5;
/// Wrong results, since last leaving probation, that send a node back to it.
const WRONG_RESULTS_BEFORE_PROBATION: u32 = 2;
/// Every score drifts back toward the network average, halving its distance in
/// this time, so an idle or penalized node is eventually tried again.
const HEALING_HALF_LIFE_SECS: f64 = 7.0 * 24.0 * 3600.0;

/// The lowest reliability a node may have to take jobs of each tier above 1.
const TIER_2_RELIABILITY: f64 = 0.95;
const TIER_3_RELIABILITY: f64 = 0.98;
const TOP_TIER_RELIABILITY: f64 = 0.99;
/// The lowest tier whose jobs need stake behind the node.
pub const FIRST_STAKED_TIER: u32 = 3;

/// The chance of being drawn that even the lowest-scoring node keeps.
const LOWEST_WEIGHT: f64 = 0.01;

/// Something a node did that changes its scores.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct ScoreEvent {
    pub node: PublicKey,
    /// When it happened, in seconds since 1970.
    pub at: u64,
    pub what: Conduct,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Conduct {
    /// Its result was confirmed.
    Matched,
    /// Its result was shown to be wrong.
    Wrong,
    /// It got a job whose answer the coordinator already knew, and got it wrong.
    FailedKnownAnswer,
    /// It reported a result before the deadline.
    Finished,
    /// It was handed a job and did not report before the deadline.
    Missed,
}

/// What applying an event did beyond changing scores.
#[derive(Debug, PartialEq, Eq)]
pub enum Penalty {
    None,
    /// The node is back on probation, which also costs it part of its stake.
    ReturnedToProbation,
}

#[derive(Clone, Copy)]
struct NodeScore {
    reliability: f64,
    availability: f64,
    /// Results confirmed since the node last entered probation.
    matched: u32,
    /// Results shown wrong since the node last left probation.
    wrong: u32,
    updated_at: u64,
}

/// What a node may be handed, and how likely it is to be drawn.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Standing {
    pub reliability: f64,
    pub availability: f64,
    /// Past probation: its jobs are spot-checked instead of always duplicated.
    pub proven: bool,
    /// The highest tier of job it may take, before stake is considered.
    pub max_tier: u32,
    /// Credits its account has staked. Jobs of tier 3 and above need stake that
    /// covers the job.
    pub stake: Credits,
}

impl Standing {
    /// Its share of the draw when nodes are picked for a job.
    pub fn weight(&self) -> f64 {
        (self.reliability * self.availability).max(LOWEST_WEIGHT)
    }
}

/// The scores of every node that has done anything.
#[derive(Default)]
pub struct Scores {
    nodes: HashMap<PublicKey, NodeScore>,
}

impl Scores {
    pub fn apply(&mut self, event: &ScoreEvent) -> Penalty {
        let average = self.average();
        let score = self.nodes.entry(event.node).or_insert(NodeScore {
            reliability: STARTING_SCORE,
            availability: STARTING_SCORE,
            matched: 0,
            wrong: 0,
            updated_at: event.at,
        });

        // First let the time since the node's last event pull it toward the average.
        let elapsed = event.at.saturating_sub(score.updated_at) as f64;
        let kept = 0.5_f64.powf(elapsed / HEALING_HALF_LIFE_SECS);
        score.reliability = average.0 + (score.reliability - average.0) * kept;
        score.availability = average.1 + (score.availability - average.1) * kept;
        score.updated_at = event.at;

        match event.what {
            Conduct::Matched => {
                score.reliability += SCORE_STEP * (1.0 - score.reliability);
                score.matched += 1;
            }
            Conduct::Finished => score.availability += SCORE_STEP * (1.0 - score.availability),
            Conduct::Missed => score.availability -= SCORE_STEP * score.availability,
            Conduct::Wrong | Conduct::FailedKnownAnswer => {
                score.reliability *= WRONG_RESULT_FACTOR;
                score.wrong += 1;
                let was_proven = score.matched >= PROBATION_RESULTS;
                let repeated = score.wrong >= WRONG_RESULTS_BEFORE_PROBATION;
                if was_proven && (repeated || event.what == Conduct::FailedKnownAnswer) {
                    score.matched = 0;
                    score.wrong = 0;
                    return Penalty::ReturnedToProbation;
                }
            }
        }
        Penalty::None
    }

    pub fn standing(&self, node: &PublicKey, stake: Credits) -> Standing {
        let (reliability, availability, matched) = match self.nodes.get(node) {
            Some(score) => (score.reliability, score.availability, score.matched),
            None => (STARTING_SCORE, STARTING_SCORE, 0),
        };
        let proven = matched >= PROBATION_RESULTS;
        let max_tier = match reliability {
            _ if !proven => 0,
            r if r >= TOP_TIER_RELIABILITY => MAX_TIER,
            r if r >= TIER_3_RELIABILITY => FIRST_STAKED_TIER,
            r if r >= TIER_2_RELIABILITY => 2,
            _ => 1,
        };
        Standing {
            reliability,
            availability,
            proven,
            max_tier,
            stake,
        }
    }

    /// The network's average reliability and availability.
    fn average(&self) -> (f64, f64) {
        if self.nodes.is_empty() {
            return (STARTING_SCORE, STARTING_SCORE);
        }
        let count = self.nodes.len() as f64;
        let reliability: f64 = self.nodes.values().map(|score| score.reliability).sum();
        let availability: f64 = self.nodes.values().map(|score| score.availability).sum();
        (reliability / count, availability / count)
    }
}
