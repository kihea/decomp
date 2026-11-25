//! The job board's rules: who is granted a lease on a job, how results are
//! committed, revealed, and compared, and when a result stands (spec, "Job
//! states" and "Verification follows supply"; section 2 "Lifecycle"; section 3
//! "Assignment" and "Open market"; section 4; section 5 "Limits on provider
//! leverage").
//!
//! Nodes ask for work; they do not choose it. Each request for leases is
//! answered by drawing, for each waiting job, one node at random among the
//! eligible nodes asking, weighted by reliability, availability, and fit, from
//! balanced accounts first. A node's owner narrows what it is offered with
//! work preferences, and an account that opted out of standard allocation in
//! the large tiers is granted leases there through its standing bids.
//!
//! A node commits a hash of its result before anyone reveals. A job picked up
//! twice is confirmed when the two results match; a job picked up once is
//! accepted unconfirmed and, now and then, audited. When nodes are scarce the
//! network keeps working at lower assurance, and every receipt says which
//! level the result got.
//!
//! Like `Accounts`, this does no I/O. The caller passes in the time, each round
//! of the public randomness beacon, each node's and account's standing, and the
//! contents of each shard of state, so the same calls always produce the same
//! decisions. Only the choice of known-answer jobs uses randomness of the
//! caller's own, because a node must not be able to tell one is coming. What
//! the caller must act on comes back through lists it takes after each call:
//! jobs that closed, score events, and draws to publish.
//!
//! Nothing here is durable: leases and commitments are needed only until a job
//! closes (spec, "Data management"). What lasts is the receipt, which the caller
//! stores.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use rand::{Rng, RngExt};

use crate::draw::{Beacon, Draw, Eligible, Pickup, WEIGHT_SCALE, is_audited};
use crate::envelope::Envelope;
use crate::inference::{PipelineStage, StageLease, StageRef, approved, stage_share_ppm};
use crate::job::{
    Failure, Fallback, JobResult, JobStatus, Lease, MAX_JOB_SLOTS, MAX_MODELS, MAX_TIER, Manifest,
    Outcome, Receipt, Spawn, Verification, Verify,
};
use crate::keys::{AccountId, ContentHash, JobId, PublicKey, ShareSet};
use crate::ledger::{Credits, FeeSplit, HOLDBACK_PERCENT, HOLDBACK_SECS, Payout, compute_fee};
use crate::market::{
    AccountStanding, Bid, BidError, BidOrder, Hardware, JobClass, LeasePrice, MARKET_FIRST_TIER,
    MAX_BIDS_PER_ACCOUNT, PREMIUM_SCALE, Rate, Rates, SHARE_CAP_MIN_ACCOUNTS, Shares,
    WorkPreferences, fallback_rate, share_cap_ppm, taper,
};
use crate::privacy;
use crate::scores::{Conduct, FIRST_STAKED_TIER, ScoreEvent, Standing};
use crate::state::ShardRef;

/// A node counts as asking for work for this long after its last request.
pub const ONLINE_WINDOW_SECS: u64 = 10;

/// How long a closed job stays readable by its submitter before it is forgotten.
pub const CLOSED_JOB_RETENTION_SECS: u64 = 3600;

/// How long a node has to commit a result once it is granted a lease. After
/// that the lease reopens for another node. A starting value.
pub const LEASE_SECS: u64 = 300;

/// How long after the first commitment a second node may pick a job up and
/// confirm it, for an opportunistic job. A starting value.
pub const CONFIRMATION_WINDOW_SECS: u64 = 60;

/// How long the nodes that committed have to reveal their results. A starting
/// value.
pub const REVEAL_WINDOW_SECS: u64 = 60;

/// After a node commits a result, the chance that its next job is one whose
/// answer is already known. A starting value.
pub const KNOWN_ANSWER_RATE: f64 = 0.02;

/// How many confirmed jobs are remembered to be reissued as known-answer jobs.
const KNOWN_ANSWERS_KEPT: usize = 256;

/// How long a node is given to finish a known-answer job or an audit.
const CHECK_DEADLINE_SECS: u64 = 600;

/// How many further nodes are drawn to settle a dispute before the job is
/// given up as rejected. Each runs the job again; the first result that two
/// nodes report stands. A starting value.
pub const MAX_REFEREES: u32 = 2;

#[derive(Debug, thiserror::Error)]
pub enum JobError {
    #[error("this job was already submitted")]
    JobExists,
    #[error("the job's deadline has already passed")]
    DeadlinePassed,
    #[error("the compute cap must be at least one unit")]
    NoComputeCap,
    #[error("a job needs between 1 and {MAX_JOB_SLOTS} slots")]
    BadSlots,
    #[error("no such job")]
    UnknownJob,
    #[error("the job is not taking results now")]
    NotTakingResults,
    #[error("this node holds no lease on the job")]
    NotLeased,
    #[error("this node already committed a result for the job")]
    AlreadyCommitted,
    #[error("the revealed result does not match the node's commitment")]
    NotTheCommitment,
    #[error("the result claims more compute than the job was allowed")]
    UnitsOverCap,
    #[error(transparent)]
    Bid(#[from] BidError),
    #[error("{0}")]
    BadPreferences(&'static str),
    #[error(
        "an inference job names between 1 and {MAX_MODELS} approved models, and no other job names any"
    )]
    BadModels,
    #[error("an inference job cannot fall back to the control plane, which runs no models")]
    InferenceFallback,
}

/// A job that has just closed, for the caller to settle.
pub struct ClosedJob {
    pub job_id: JobId,
    pub how: Closure,
}

pub enum Closure {
    /// A result stood. The caller records `split` and `payouts` against the
    /// job's escrow and stores the receipt.
    Settled {
        receipt: Box<Receipt>,
        split: FeeSplit,
        payouts: Vec<Payout>,
        /// The follow-up jobs the job asked for, if any, for the caller to start.
        follow_ups: Option<FollowUps>,
    },
    /// No node was right. The payer is refunded.
    Rejected,
    /// No node finished in time and no fallback was set. The payer is refunded.
    Expired,
    /// No node finished in time and the job asked to fall back to always-on.
    /// The caller runs it in the control plane and answers with
    /// [`Jobs::fall_back`].
    FallBack(Box<FallBackJob>),
    /// A node passed a known-answer job or an audit, and is paid from the
    /// verification fund.
    CheckPassed { account: AccountId, reward: Credits },
    /// The hold on a job's payouts has run out. The caller releases it.
    Release,
    /// An audit found an unconfirmed result wrong. The caller takes what was
    /// held back from the account's payout. The job's ID is the audited job's.
    AuditFailed { account: AccountId },
    /// A dispute found a node's result wrong. The caller takes what settling
    /// the dispute cost out of its account's stake (spec section 5: wrong
    /// nodes pay for refereeing).
    WrongInDispute { node: PublicKey, cost: Credits },
}

/// A job to run as an always-on fallback.
pub struct FallBackJob {
    pub manifest: Manifest,
    pub seed: JobId,
    /// The contents of the shard the job is handed, if any.
    pub shard: Option<ContentHash>,
}

/// The follow-up jobs a settled job asked for. The caller starts them in order,
/// each like a job its submitter had sent, for as long as `spare` covers what
/// is set aside for them.
pub struct FollowUps {
    /// The account that submitted the job that asked.
    pub submitter: AccountId,
    /// What that job left of its fee cap.
    pub spare: Credits,
    pub jobs: Vec<(JobId, Manifest)>,
}

/// Nodes disagreed about a job, and further nodes are drawn to run it again
/// (spec section 4, "Checkpoint disputes"). The control plane holds no job
/// data, so it cannot run the job itself; a node drawn at random can, and like
/// a second pickup it cannot tell that the job it was granted is disputed.
struct Refereeing {
    /// The results that disagree, and those of referees that agreed with none.
    reports: Vec<(PublicKey, JobResult)>,
    /// How many referees have reported.
    referees: u32,
}

/// What a node learns when it asks for work.
#[derive(Default)]
pub struct Granted {
    /// Every lease the node holds and has not committed a result for.
    pub leases: Vec<Lease>,
    /// Stages the node was drawn to run for nodes holding leases.
    pub stages: Vec<StageLease>,
    /// Jobs whose commitment window has closed, whose results the node should
    /// reveal now.
    pub reveal: Vec<JobId>,
}

/// The standing of nodes and accounts, which the caller keeps.
pub struct Standings<'a> {
    pub node: &'a dyn Fn(&PublicKey, &AccountId) -> Standing,
    pub account: &'a dyn Fn(&AccountId) -> AccountStanding,
}

enum Kind {
    Submitted {
        submitter: AccountId,
    },
    /// Reissued to one node to see whether it reports what is already known.
    KnownAnswer {
        expected: Box<Known>,
    },
    /// A spot check of a result accepted unconfirmed.
    Audit {
        audited: JobId,
        expected: Box<Known>,
        node: PublicKey,
        account: AccountId,
    },
}

/// A result that stood, kept to be reissued as a check.
#[derive(Clone)]
struct Known {
    manifest: Manifest,
    /// The accounts the job was kept from, which it is kept from again.
    kept_from: HashSet<AccountId>,
    seed: JobId,
    shard: Option<ContentHash>,
    result: JobResult,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Taking leases and commitments.
    Open,
    /// The nodes that committed are revealing.
    Revealing {
        until: u64,
    },
    Closed,
}

struct Job {
    kind: Kind,
    manifest: Manifest,
    /// The accounts whose nodes are never granted the job.
    kept_from: HashSet<AccountId>,
    submitted_at: u64,
    /// The posted rate locked when the job was submitted, per unit.
    rate: u64,
    shard: Option<ContentHash>,
    held_shard: bool,
    leases: Vec<Held>,
    draws: Vec<Draw>,
    /// Until when a second node may pick the job up, once the first committed.
    confirm_until: Option<u64>,
    /// Set while a dispute about the job is being settled.
    refereeing: Option<Refereeing>,
    phase: Phase,
    status: JobStatus,
    closed_at: Option<u64>,
}

/// A node drawn to run one stage of a model for a node holding a lease.
#[derive(Clone, Copy)]
struct Assigned {
    model: ContentHash,
    stage: u32,
    node: PublicKey,
    account: AccountId,
}

/// Whether a node with `hardware` can run every model in `models`.
fn runs_models(hardware: Option<&Hardware>, models: &[ContentHash]) -> bool {
    models.iter().all(|id| {
        let Some(hardware) = hardware else {
            return false;
        };
        let gpu_ready =
            approved(id).is_some_and(|model| !model.needs_gpu) || !hardware.gpus.is_empty();
        hardware.models.contains(id) && gpu_ready
    })
}

/// A node that may be drawn for a lease: its entry in the draw, its account,
/// its account's taper, and the bid it would fill, if any.
type Candidate = (Eligible, AccountId, u64, Option<ContentHash>);

/// A lease as the job board holds it.
struct Held {
    node: PublicKey,
    account: AccountId,
    price: LeasePrice,
    /// What the account is paid per unit, in thousandths of `price`.
    taper: u64,
    expires_at: u64,
    proven: bool,
    /// The node was drawn to settle a dispute.
    referee: bool,
    /// When the lease was granted and when the node committed, which time a
    /// job whose compute is known as a measure of the node's hardware.
    granted_at: u64,
    committed_at: u64,
    commitment: Option<ContentHash>,
    /// The transcript of the node's model calls, which it named when it
    /// committed.
    transcript: Option<ContentHash>,
    /// The nodes drawn to run the stages of the job's models that this node
    /// does not hold. Each takes a slot until this node commits.
    pipeline: Vec<Assigned>,
    revealed: Option<(JobResult, Envelope)>,
}

/// A node that has asked for work.
struct Node {
    account: AccountId,
    last_ask: u64,
    known_answer_due: bool,
}

/// Every open job, every node asking for work, their owners' preferences and
/// bids, and the posted rates.
#[derive(Default)]
pub struct Jobs {
    jobs: HashMap<JobId, Job>,
    /// Jobs taking leases, oldest first.
    open: VecDeque<JobId>,
    nodes: HashMap<PublicKey, Node>,
    preferences: HashMap<PublicKey, WorkPreferences>,
    /// What each node says its hardware is.
    hardware: HashMap<PublicKey, Hardware>,
    bids: BTreeMap<ContentHash, Bid>,
    rates: Rates,
    shares: Shares,
    known: VecDeque<Known>,
    /// Jobs whose payouts are held back, and when the hold runs out.
    holds: VecDeque<(u64, JobId)>,
    closed: Vec<ClosedJob>,
    events: Vec<ScoreEvent>,
    draws: Vec<Draw>,
    beacon: Option<Beacon>,
}

impl Jobs {
    /// Whether `manifest` describes a job that can be accepted as `id` now.
    pub fn check(&self, id: &JobId, manifest: &Manifest, now: u64) -> Result<(), JobError> {
        if manifest.deadline <= now {
            return Err(JobError::DeadlinePassed);
        }
        if manifest.compute_cap == 0 {
            return Err(JobError::NoComputeCap);
        }
        if manifest.slots == 0 || manifest.slots > MAX_JOB_SLOTS {
            return Err(JobError::BadSlots);
        }
        if self.jobs.contains_key(id) {
            return Err(JobError::JobExists);
        }
        let inference = manifest.class == JobClass::Inference;
        let models_fit = if inference {
            (1..=MAX_MODELS).contains(&manifest.models.len())
                && manifest.models.iter().all(|id| approved(id).is_some())
        } else {
            manifest.models.is_empty()
        };
        if !models_fit {
            return Err(JobError::BadModels);
        }
        if inference && manifest.fallback.is_some() {
            return Err(JobError::InferenceFallback);
        }
        Ok(())
    }

    /// The rate posted now for a job's class and tier.
    pub fn posted(&self, manifest: &Manifest) -> Rate {
        self.rates.posted(manifest.class, manifest.tier())
    }

    /// Every posted rate.
    pub fn rates(&self) -> Vec<Rate> {
        self.rates.all()
    }

    /// The most a job can cost its payer if it is submitted now.
    pub fn highest_fee(&self, manifest: &Manifest) -> Credits {
        let rate = self.posted(manifest).micros_per_unit;
        manifest.highest_fee(rate, fallback_rate(manifest.class))
    }

    /// Takes in the newest round of public randomness.
    pub fn set_beacon(&mut self, beacon: Beacon) {
        self.beacon = Some(beacon);
    }

    /// Accepts a job at the rate posted now. The caller sets aside its highest
    /// possible fee first, so a job never runs without the credits to pay for it.
    ///
    /// `kept_from` holds the accounts that the keys in the manifest's
    /// `keep_from` belong to. Only the caller knows which account a key is of.
    pub fn submit(
        &mut self,
        id: JobId,
        submitter: AccountId,
        manifest: Manifest,
        kept_from: HashSet<AccountId>,
        now: u64,
    ) -> Result<(), JobError> {
        self.check(&id, &manifest, now)?;
        let rate = self.posted(&manifest).micros_per_unit;
        let mut job = Job::new(Kind::Submitted { submitter }, manifest, rate, now);
        job.kept_from = kept_from;
        self.jobs.insert(id, job);
        self.open.push_back(id);
        Ok(())
    }

    /// Stores a device's work preferences.
    pub fn set_preferences(
        &mut self,
        device: PublicKey,
        preferences: WorkPreferences,
    ) -> Result<(), JobError> {
        if let Some(problem) = preferences.problem() {
            return Err(JobError::BadPreferences(problem));
        }
        self.preferences.insert(device, preferences);
        Ok(())
    }

    /// Notes what a node says its hardware is.
    pub fn declare_hardware(&mut self, node: PublicKey, hardware: Hardware) {
        self.hardware.insert(node, hardware);
    }

    /// A device's work preferences: what it stored, or the recommended default.
    pub fn preferences(&self, device: &PublicKey) -> WorkPreferences {
        self.preferences.get(device).cloned().unwrap_or_default()
    }

    /// Places a standing bid for `account`.
    pub fn place_bid(
        &mut self,
        account: AccountId,
        order: BidOrder,
        id: ContentHash,
        now: u64,
    ) -> Result<Bid, JobError> {
        order.check(now)?;
        let held = self
            .bids
            .values()
            .filter(|bid| bid.account == account && bid.is_live(now));
        if held.count() >= MAX_BIDS_PER_ACCOUNT {
            return Err(BidError::TooManyBids.into());
        }
        let bid = Bid {
            id,
            account,
            order,
            filled: 0,
        };
        self.bids.insert(id, bid.clone());
        Ok(bid)
    }

    /// Withdraws one of `account`'s bids. Leases already granted against it stand.
    pub fn cancel_bid(&mut self, account: &AccountId, id: &ContentHash) -> Result<(), JobError> {
        match self.bids.get(id) {
            Some(bid) if bid.account == *account => {
                self.bids.remove(id);
                Ok(())
            }
            _ => Err(BidError::UnknownBid.into()),
        }
    }

    /// Whether `node` holds a lease on `job_id` that is still taking results.
    pub fn holds_lease(&self, job_id: &JobId, node: &PublicKey) -> bool {
        self.jobs.get(job_id).is_some_and(|job| {
            job.closed_at.is_none() && job.leases.iter().any(|held| held.node == *node)
        })
    }

    /// `account`'s live bids.
    pub fn bids_of(&self, account: &AccountId, now: u64) -> Vec<Bid> {
        let mine = self.bids.values().filter(|bid| bid.account == *account);
        mine.filter(|bid| bid.is_live(now)).cloned().collect()
    }

    /// Records that `node` is asking for work and grants it leases. Each request
    /// also grants waiting jobs to other nodes asking, so a lease is always drawn
    /// from everyone asking and not given to whoever asked first.
    ///
    /// `contents` gives the contents a shard of state has now. The caller must
    /// have applied what every settled job left behind before calling this.
    /// `stored` says whether a blob can still be fetched from storage nodes,
    /// which a job reissued as a check needs of every blob it reads.
    #[allow(clippy::too_many_arguments)]
    pub fn request_lease(
        &mut self,
        node: PublicKey,
        account: AccountId,
        now: u64,
        rng: &mut impl Rng,
        standings: &Standings,
        contents: &dyn Fn(&ShardRef) -> Option<ContentHash>,
        stored: &dyn Fn(&ContentHash) -> bool,
    ) -> Granted {
        self.sweep(now);
        let entry = self.nodes.entry(node).or_insert(Node {
            account,
            last_ask: now,
            known_answer_due: false,
        });
        entry.last_ask = now;
        entry.account = account;
        if self.nodes[&node].known_answer_due && self.free_slots(&node) > 0 {
            let standing = (standings.node)(&node, &account);
            self.issue_check(node, account, now, rng, &standing, stored);
        }
        self.grant(now, standings, contents);

        let mut granted = Granted::default();
        for (job_id, job) in &self.jobs {
            if job.closed_at.is_some() {
                continue;
            }
            let Some(held) = job.leases.iter().find(|held| held.node == node) else {
                continue;
            };
            match (job.phase, held.commitment, &held.revealed) {
                (Phase::Open, None, _) => granted.leases.push(job.lease_for(*job_id, held)),
                (Phase::Revealing { .. }, Some(_), None) => granted.reveal.push(*job_id),
                _ => {}
            }
        }
        for (job_id, job) in &self.jobs {
            if job.closed_at.is_some() {
                continue;
            }
            for held in job.leases.iter().filter(|held| held.commitment.is_none()) {
                for assigned in held.pipeline.iter().filter(|a| a.node == node) {
                    granted.stages.push(StageLease {
                        job_id: *job_id,
                        model: assigned.model,
                        stage: assigned.stage,
                        head: held.node,
                        expires_at: held.expires_at,
                    });
                }
            }
        }
        granted
    }

    /// Records a node's commitment to its result.
    pub fn commit_result(
        &mut self,
        node: PublicKey,
        job_id: JobId,
        commitment: ContentHash,
        transcript: Option<ContentHash>,
        now: u64,
        rng: &mut impl Rng,
    ) -> Result<(), JobError> {
        self.sweep(now);
        let job = self.jobs.get_mut(&job_id).ok_or(JobError::UnknownJob)?;
        if job.phase != Phase::Open {
            return Err(JobError::NotTakingResults);
        }
        let held = job
            .leases
            .iter_mut()
            .find(|held| held.node == node)
            .ok_or(JobError::NotLeased)?;
        if held.commitment.is_some() {
            return Err(JobError::AlreadyCommitted);
        }
        held.commitment = Some(commitment);
        held.transcript = transcript;
        held.committed_at = now;
        self.events.push(ScoreEvent {
            node,
            at: now,
            what: Conduct::Finished,
        });
        if matches!(job.kind, Kind::Submitted { .. })
            && let Some(entry) = self.nodes.get_mut(&node)
            && rng.random_bool(KNOWN_ANSWER_RATE)
        {
            entry.known_answer_due = true;
        }
        let committed = job.committed();
        if committed == 1 && job.confirm_until.is_none() {
            // A first result from a node on probation is paid only once a
            // second pickup confirms it, so it waits for one until the
            // deadline. So does an inference result, which is confirmed only
            // by a node that checks its proofs (spec section 4, "TOPLOC proof").
            let checked_inference =
                job.manifest.class == JobClass::Inference && job.manifest.verify != Verify::None;
            let waits_to_deadline = job.manifest.verify == Verify::Required
                || checked_inference
                || !job.leases[0].proven;
            job.confirm_until = Some(if waits_to_deadline {
                job.manifest.deadline
            } else {
                now + CONFIRMATION_WINDOW_SECS
            });
        }
        self.advance(job_id, now);
        Ok(())
    }

    /// Records a node's revealed result, which must match its commitment.
    pub fn reveal_result(
        &mut self,
        node: PublicKey,
        result: JobResult,
        salt: ContentHash,
        envelope: Envelope,
        now: u64,
    ) -> Result<(), JobError> {
        self.sweep(now);
        let job_id = result.job_id;
        let job = self.jobs.get_mut(&job_id).ok_or(JobError::UnknownJob)?;
        if !matches!(job.phase, Phase::Revealing { .. }) {
            return Err(JobError::NotTakingResults);
        }
        if result.units > job.manifest.compute_cap {
            return Err(JobError::UnitsOverCap);
        }
        let held = job
            .leases
            .iter_mut()
            .find(|held| held.node == node)
            .ok_or(JobError::NotLeased)?;
        if held.commitment != Some(result.commitment(&node, &salt))
            || held.transcript != result.transcript
        {
            return Err(JobError::NotTheCommitment);
        }
        held.revealed = Some((result, envelope));
        self.advance(job_id, now);
        Ok(())
    }

    /// Closes a disputed job: `right` are the nodes, referees among them,
    /// whose result stands, if two agreed. Every other node that reported pays
    /// for what the referees' runs cost.
    fn decide(&mut self, job_id: JobId, right: &[PublicKey], now: u64) {
        let Some(job) = self.jobs.get_mut(&job_id) else {
            return;
        };
        let Some(refereeing) = &job.refereeing else {
            return;
        };
        let referee_units: u64 = job
            .leases
            .iter()
            .filter(|held| held.referee)
            .filter_map(|held| held.revealed.as_ref().map(|(result, _)| result.units))
            .sum();
        let cost = compute_fee(referee_units, job.rate);
        let wrong: Vec<PublicKey> = refereeing
            .reports
            .iter()
            .map(|(node, _)| *node)
            .filter(|node| !right.contains(node))
            .collect();
        // Referees whose result stood are paid as checkers are; the job's
        // payer pays only for the runs it asked for.
        // With no two results agreeing, nobody is found right or wrong.
        let decided = !right.is_empty();
        for held in job.leases.iter().filter(|held| held.referee && decided) {
            let Some((result, _)) = &held.revealed else {
                continue;
            };
            let what = if right.contains(&held.node) {
                let reward = compute_fee(result.units, job.rate).percent(85);
                self.closed.push(ClosedJob {
                    job_id,
                    how: Closure::CheckPassed {
                        account: held.account,
                        reward,
                    },
                });
                Conduct::Matched
            } else {
                Conduct::Wrong
            };
            self.events.push(ScoreEvent {
                node: held.node,
                at: now,
                what,
            });
        }
        if !right.is_empty() {
            for node in wrong {
                self.closed.push(ClosedJob {
                    job_id,
                    how: Closure::WrongInDispute { node, cost },
                });
            }
        }
        let job = self.jobs.get_mut(&job_id).expect("checked above");
        job.leases.retain(|held| !held.referee);
        if let Kind::Audit {
            audited,
            account,
            node,
            ..
        } = &job.kind
        {
            // The audited node was wrong unless the referee said otherwise.
            let (audited, account, node) = (*audited, *account, *node);
            let original_right = right.contains(&node);
            if decided && !original_right {
                self.events.push(ScoreEvent {
                    node,
                    at: now,
                    what: Conduct::Wrong,
                });
                self.closed.push(ClosedJob {
                    job_id: audited,
                    how: Closure::AuditFailed { account },
                });
            }
            let job = self.jobs.get_mut(&job_id).expect("checked above");
            let Some(auditor) = job.leases.first() else {
                job.finish(JobStatus::Rejected, now);
                return;
            };
            let auditor = auditor.node;
            let auditor_account = job.leases[0].account;
            let auditor_right = right.contains(&auditor);
            if decided {
                self.events.push(ScoreEvent {
                    node: auditor,
                    at: now,
                    what: if auditor_right {
                        Conduct::Matched
                    } else {
                        Conduct::Wrong
                    },
                });
            }
            if auditor_right {
                let units = job.leases[0].revealed.as_ref().map_or(0, |(r, _)| r.units);
                let reward = compute_fee(units, job.rate).percent(85);
                self.closed.push(ClosedJob {
                    job_id,
                    how: Closure::CheckPassed {
                        account: auditor_account,
                        reward,
                    },
                });
            }
            job.finish(JobStatus::Rejected, now);
            return;
        }
        self.close(job_id, right, now);
    }

    /// Settles a job that ran as an always-on fallback, with the result the
    /// control plane computed. Its fee goes to the treasury, which pays for the
    /// control plane's capacity.
    pub fn fall_back(&mut self, job_id: JobId, result: JobResult, now: u64) {
        let Some(job) = self.jobs.get_mut(&job_id) else {
            return;
        };
        let Kind::Submitted { submitter } = job.kind else {
            return;
        };
        let total = compute_fee(result.units, fallback_rate(job.manifest.class));
        let split = FeeSplit {
            total,
            treasury: total,
            ..FeeSplit::default()
        };
        let receipt = job.receipt(
            job_id,
            &result,
            split,
            Vec::new(),
            Verification::AlwaysOn,
            0,
            now,
        );
        let follow_ups = job.follow_ups(job_id, submitter, &receipt);
        job.status = JobStatus::Settled {
            receipt: Box::new(receipt.clone()),
        };
        job.closed_at = Some(now);
        job.phase = Phase::Closed;
        self.closed.push(ClosedJob {
            job_id,
            how: Closure::Settled {
                receipt: Box::new(receipt),
                split,
                payouts: Vec::new(),
                follow_ups,
            },
        });
    }

    /// Where a job stands. Only the account that submitted it may ask.
    pub fn status(
        &mut self,
        id: JobId,
        account: AccountId,
        now: u64,
    ) -> Result<JobStatus, JobError> {
        self.sweep(now);
        match self.jobs.get(&id) {
            Some(Job {
                kind: Kind::Submitted { submitter },
                status,
                ..
            }) if *submitter == account => Ok(status.clone()),
            _ => Err(JobError::UnknownJob),
        }
    }

    /// Whether any node of `account` holds a lease that is not settled.
    pub fn is_working(&self, account: &AccountId) -> bool {
        self.jobs.values().any(|job| {
            job.closed_at.is_none() && job.leases.iter().any(|held| held.account == *account)
        })
    }

    /// The jobs that closed since this was last called. Each has credits to move.
    pub fn take_closed(&mut self) -> Vec<ClosedJob> {
        std::mem::take(&mut self.closed)
    }

    /// What nodes did since this was last called, for their scores.
    pub fn take_events(&mut self) -> Vec<ScoreEvent> {
        std::mem::take(&mut self.events)
    }

    /// The draws of the jobs that closed since this was last called.
    pub fn take_draws(&mut self) -> Vec<Draw> {
        std::mem::take(&mut self.draws)
    }

    /// Lets time pass: expires leases and jobs, moves jobs whose windows closed,
    /// releases holds, retunes rates, and forgets what is no longer needed.
    pub fn tick(&mut self, now: u64) {
        self.sweep(now);
    }

    /// Moves a job on as far as its commitments and reveals allow.
    fn advance(&mut self, job_id: JobId, now: u64) {
        let Some(job) = self.jobs.get_mut(&job_id) else {
            return;
        };
        match job.phase {
            Phase::Open if job.refereeing.is_some() => {
                let referee_committed = job.leases.iter().any(|held| {
                    held.referee && held.commitment.is_some() && held.revealed.is_none()
                });
                if referee_committed {
                    job.phase = Phase::Revealing {
                        until: now + REVEAL_WINDOW_SECS,
                    };
                }
            }
            Phase::Open => {
                let committed = job.committed();
                let pending = job.leases.iter().any(|held| held.commitment.is_none());
                let single = matches!(job.kind, Kind::KnownAnswer { .. } | Kind::Audit { .. })
                    || job.manifest.verify == Verify::None;
                let window_over = job.confirm_until.is_some_and(|until| now >= until);
                if committed >= 2 || (committed == 1 && (single || (window_over && !pending))) {
                    job.phase = Phase::Revealing {
                        until: now + REVEAL_WINDOW_SECS,
                    };
                    job.status = JobStatus::Committed;
                }
            }
            Phase::Revealing { until } => {
                let committed = job.leases.iter().filter(|held| held.commitment.is_some());
                let all_in = committed.clone().all(|held| held.revealed.is_some());
                if all_in || now >= until {
                    self.evaluate(job_id, now);
                }
            }
            Phase::Closed => {}
        }
    }

    /// Starts settling a dispute: the job takes a lease again, for a referee.
    fn dispute(&mut self, job_id: JobId, reports: Vec<(PublicKey, JobResult)>) {
        let job = self.jobs.get_mut(&job_id).expect("disputed jobs exist");
        job.refereeing = Some(Refereeing {
            reports,
            referees: 0,
        });
        job.phase = Phase::Open;
        job.status = JobStatus::Disputed;
        job.confirm_until = None;
        if !self.open.contains(&job_id) {
            self.open.push_back(job_id);
        }
    }

    /// Weighs a referee's result, once it is revealed or its time is up.
    fn referee_reported(&mut self, job_id: JobId, now: u64) {
        let job = self.jobs.get_mut(&job_id).expect("evaluated jobs exist");
        let refereeing = job.refereeing.as_mut().expect("the job is disputed");
        let reported = job.leases.iter().find(|held| {
            held.referee
                && !refereeing
                    .reports
                    .iter()
                    .any(|(node, _)| *node == held.node)
        });
        if let Some(held) = reported {
            let (found, _) = held.revealed.clone().expect("kept leases revealed");
            let node = held.node;
            refereeing.referees += 1;
            let mut right: Vec<PublicKey> = refereeing
                .reports
                .iter()
                .filter(|(_, result)| *result == found)
                .map(|(node, _)| *node)
                .collect();
            if !right.is_empty() {
                right.push(node);
                self.decide(job_id, &right, now);
                return;
            }
            refereeing.reports.push((node, found));
        }
        if refereeing.referees >= MAX_REFEREES || job.manifest.deadline <= now {
            self.decide(job_id, &[], now);
            return;
        }
        job.phase = Phase::Open;
        job.status = JobStatus::Disputed;
        if !self.open.contains(&job_id) {
            self.open.push_back(job_id);
        }
    }

    /// Compares the revealed results of a job whose reveal window is over.
    fn evaluate(&mut self, job_id: JobId, now: u64) {
        let job = self.jobs.get_mut(&job_id).expect("evaluated jobs exist");
        // Nodes that committed and never revealed are treated as having missed.
        for held in &job.leases {
            if held.commitment.is_some() && held.revealed.is_none() {
                self.events.push(ScoreEvent {
                    node: held.node,
                    at: now,
                    what: Conduct::Missed,
                });
            }
        }
        job.leases.retain(|held| held.revealed.is_some());
        if job.refereeing.is_some() {
            self.referee_reported(job_id, now);
            return;
        }
        let reveals: Vec<(PublicKey, JobResult)> = job
            .leases
            .iter()
            .map(|held| (held.node, held.revealed.as_ref().expect("kept").0.clone()))
            .collect();

        if let Kind::KnownAnswer { expected } | Kind::Audit { expected, .. } = &job.kind {
            let Some((node, found)) = reveals.first().cloned() else {
                job.finish(JobStatus::Expired, now);
                return;
            };
            let passed = found
                == JobResult {
                    job_id,
                    ..expected.result.clone()
                };
            // The job's compute is known, so how long the node took to commit
            // measures its hardware (spec section 1, "confirmed by benchmark
            // jobs").
            if passed {
                let held = &job.leases[0];
                let seconds = held.committed_at.saturating_sub(held.granted_at).max(1);
                self.events.push(ScoreEvent {
                    node,
                    at: now,
                    what: Conduct::Benchmarked {
                        units: found.units,
                        millis: seconds * 1000,
                    },
                });
            }
            match (&job.kind, passed) {
                (Kind::KnownAnswer { .. }, _) => {
                    let what = if passed {
                        Conduct::Matched
                    } else {
                        Conduct::FailedKnownAnswer
                    };
                    self.events.push(ScoreEvent {
                        node,
                        at: now,
                        what,
                    });
                    if passed {
                        let reward = compute_fee(found.units, job.rate).percent(85);
                        let account = job.leases[0].account;
                        self.closed.push(ClosedJob {
                            job_id,
                            how: Closure::CheckPassed { account, reward },
                        });
                    }
                    job.finish(JobStatus::Rejected, now);
                }
                (Kind::Audit { .. }, true) => {
                    self.events.push(ScoreEvent {
                        node,
                        at: now,
                        what: Conduct::Matched,
                    });
                    let reward = compute_fee(found.units, job.rate).percent(85);
                    let account = job.leases[0].account;
                    self.closed.push(ClosedJob {
                        job_id,
                        how: Closure::CheckPassed { account, reward },
                    });
                    job.finish(JobStatus::Rejected, now);
                }
                (
                    Kind::Audit {
                        node: audited_node,
                        expected,
                        ..
                    },
                    false,
                ) => {
                    // The auditor and the audited node disagree: referees
                    // settle which of them was right.
                    let original = (
                        *audited_node,
                        JobResult {
                            job_id,
                            ..expected.result.clone()
                        },
                    );
                    self.dispute(job_id, vec![original, (node, found)]);
                }
                (Kind::Submitted { .. }, _) => unreachable!("matched above"),
            }
            return;
        }

        match reveals.as_slice() {
            [] => {
                // Nobody revealed. The job takes leases again while it can.
                job.phase = Phase::Open;
                job.confirm_until = None;
                job.status = JobStatus::Submitted;
                if !self.open.contains(&job_id) {
                    self.open.push_back(job_id);
                }
            }
            [(node, _)] => {
                let required = job.manifest.verify == Verify::Required;
                if required && now < job.manifest.deadline {
                    // A required job needs a second result, and this one is now
                    // public, so a new pickup could copy it: both run again.
                    let node = *node;
                    job.leases.clear();
                    job.phase = Phase::Open;
                    job.confirm_until = None;
                    job.status = JobStatus::Submitted;
                    self.events.push(ScoreEvent {
                        node,
                        at: now,
                        what: Conduct::Finished,
                    });
                    if !self.open.contains(&job_id) {
                        self.open.push_back(job_id);
                    }
                } else if required {
                    self.expire(job_id, now);
                } else {
                    let node = *node;
                    self.close(job_id, &[node], now);
                }
            }
            [(_, first), (_, second), ..] if first == second => {
                let everyone: Vec<PublicKey> = reveals.iter().map(|(node, _)| *node).collect();
                self.close(job_id, &everyone, now);
            }
            _ => self.dispute(job_id, reveals),
        }
    }

    /// Closes a submitted job. `right` are the nodes whose result stands;
    /// everyone else who revealed was wrong.
    fn close(&mut self, job_id: JobId, right: &[PublicKey], now: u64) {
        let beacon = self.beacon;
        let job = self
            .jobs
            .get_mut(&job_id)
            .expect("the job being closed exists");
        self.draws.append(&mut job.draws);
        let disputed = job.refereeing.is_some();
        for held in &job.leases {
            let what = if right.contains(&held.node) {
                Conduct::Matched
            } else {
                Conduct::Wrong
            };
            if held.revealed.is_some() {
                self.events.push(ScoreEvent {
                    node: held.node,
                    at: now,
                    what,
                });
            }
        }
        let standing: Vec<&Held> = job
            .leases
            .iter()
            .filter(|held| right.contains(&held.node))
            .collect();
        let Some(first) = standing.first() else {
            job.finish(JobStatus::Rejected, now);
            self.closed.push(ClosedJob {
                job_id,
                how: Closure::Rejected,
            });
            return;
        };
        let result = first
            .revealed
            .as_ref()
            .expect("standing results were revealed")
            .0
            .clone();
        let Kind::Submitted { submitter } = job.kind else {
            return;
        };

        // The first node's model calls did not hold up, so no node has the
        // job's real output: checkers only replay a transcript. The job is
        // run again from the start, kept from the accounts found wrong, while
        // its deadline allows; those found right are paid as checkers are.
        let rejected = Outcome::Failed {
            failure: Failure::ProofRejected,
        };
        if result.outcome == rejected && now < job.manifest.deadline {
            for held in &standing {
                let reward = compute_fee(result.units, job.rate).percent(85);
                self.closed.push(ClosedJob {
                    job_id,
                    how: Closure::CheckPassed {
                        account: held.account,
                        reward,
                    },
                });
            }
            let wrong: Vec<AccountId> = job
                .leases
                .iter()
                .filter(|held| !right.contains(&held.node))
                .map(|held| held.account)
                .collect();
            job.kept_from.extend(wrong);
            job.leases.clear();
            job.refereeing = None;
            job.confirm_until = None;
            job.phase = Phase::Open;
            job.status = JobStatus::Submitted;
            if !self.open.contains(&job_id) {
                self.open.push_back(job_id);
            }
            return;
        }

        let verified = match (disputed, standing.len()) {
            (true, _) => Verification::Refereed,
            (false, 1) => Verification::Unconfirmed,
            (false, _) => Verification::Confirmed,
        };
        // A result from a node on probation is paid only once confirmed.
        let paid: Vec<&&Held> = standing
            .iter()
            .filter(|held| verified != Verification::Unconfirmed || held.proven)
            .collect();
        let runs: Vec<Credits> = paid
            .iter()
            .map(|held| {
                let full = held.price.fee(result.units, job.rate);
                Credits::from_micros(
                    full.micros() / PREMIUM_SCALE * held.taper
                        + full.micros() % PREMIUM_SCALE * held.taper / PREMIUM_SCALE,
                )
            })
            .collect();
        let (split, pay) = FeeSplit::of_runs(&runs);
        let unconfirmed = verified == Verification::Unconfirmed;
        let holdback = |amount: Credits| {
            if unconfirmed {
                amount.percent(HOLDBACK_PERCENT)
            } else {
                Credits::ZERO
            }
        };
        // Each node drawn to run a stage for a paid run gets its share of it.
        let share = stage_share_ppm(&job.manifest.models);
        let mut payouts: Vec<Payout> = Vec::new();
        for (held, amount) in paid.iter().zip(pay) {
            let mut left = amount;
            for assigned in &held.pipeline {
                let part = Credits::from_micros(
                    (u128::from(amount.micros()) * u128::from(share) / 1_000_000) as u64,
                );
                left = left - part;
                payouts.push(Payout {
                    account: assigned.account,
                    amount: part,
                    held: holdback(part),
                });
            }
            payouts.push(Payout {
                account: held.account,
                amount: left,
                held: holdback(left),
            });
        }
        let envelopes = standing
            .iter()
            .map(|held| held.revealed.as_ref().expect("revealed").1.clone())
            .collect();
        let receipt = job.receipt(
            job_id,
            &result,
            split,
            envelopes,
            verified,
            paid.len() as u32,
            now,
        );
        job.status = JobStatus::Settled {
            receipt: Box::new(receipt.clone()),
        };
        job.closed_at = Some(now);
        job.phase = Phase::Closed;

        let known = Known {
            manifest: job.manifest.clone(),
            kept_from: job.kept_from.clone(),
            seed: job_id,
            shard: job.shard,
            result: result.clone(),
        };
        // Two nodes agreeing is what makes an answer known. A share is never
        // reissued: the node asked might hold another share of the same job.
        let reusable = job.manifest.share.is_none();
        if verified == Verification::Confirmed && reusable {
            self.known.push_back(known.clone());
            if self.known.len() > KNOWN_ANSWERS_KEPT {
                self.known.pop_front();
            }
        }
        if unconfirmed && payouts.iter().any(|payout| payout.held > Credits::ZERO) {
            self.holds.push_back((now + HOLDBACK_SECS, job_id));
        }
        let follow_ups = job.follow_ups(job_id, submitter, &receipt);
        let audited =
            unconfirmed && reusable && beacon.is_some_and(|beacon| is_audited(&job_id, &beacon));
        let node = first.node;
        let account = first.account;
        self.closed.push(ClosedJob {
            job_id,
            how: Closure::Settled {
                receipt: Box::new(receipt),
                split,
                payouts,
                follow_ups,
            },
        });
        if audited {
            self.open_audit(job_id, known, node, account, now);
        }
    }

    /// Queues a spot check of a result accepted unconfirmed. It is handed out
    /// under an ID of its own, like any other job.
    fn open_audit(
        &mut self,
        audited: JobId,
        known: Known,
        node: PublicKey,
        account: AccountId,
        now: u64,
    ) {
        let mut seed = audited.as_bytes().to_vec();
        seed.extend_from_slice(b"/audit");
        let id = JobId::from_bytes(*ContentHash::of(&seed).as_bytes());
        let mut manifest = known.manifest.clone();
        manifest.deadline = now + CHECK_DEADLINE_SECS;
        let rate = self.jobs.get(&audited).map_or(0, |job| job.rate);
        let mut kept_from = known.kept_from.clone();
        kept_from.insert(account);
        let shard = known.shard;
        let mut job = Job::new(
            Kind::Audit {
                audited,
                expected: Box::new(known),
                node,
                account,
            },
            manifest,
            rate,
            now,
        );
        job.kept_from = kept_from;
        job.shard = shard;
        self.jobs.insert(id, job);
        self.open.push_back(id);
    }

    /// Ends a job that no node finished: falls back, or expires.
    fn expire(&mut self, job_id: JobId, now: u64) {
        let job = self.jobs.get_mut(&job_id).expect("expired jobs exist");
        self.draws.append(&mut job.draws);
        for held in &job.leases {
            if held.commitment.is_none() {
                self.events.push(ScoreEvent {
                    node: held.node,
                    at: now,
                    what: Conduct::Missed,
                });
            }
        }
        if !matches!(job.kind, Kind::Submitted { .. }) {
            job.finish(JobStatus::Expired, now);
            return;
        }
        if job.manifest.fallback == Some(Fallback::AlwaysOn) {
            job.phase = Phase::Closed;
            job.leases.clear();
            self.closed.push(ClosedJob {
                job_id,
                how: Closure::FallBack(Box::new(FallBackJob {
                    manifest: job.manifest.clone(),
                    seed: job_id,
                    shard: job.shard,
                })),
            });
            return;
        }
        job.finish(JobStatus::Expired, now);
        self.closed.push(ClosedJob {
            job_id,
            how: Closure::Expired,
        });
    }

    /// Slots a node has free under its owner's preferences.
    fn free_slots(&self, node: &PublicKey) -> u32 {
        let used: u32 = self
            .jobs
            .values()
            .filter(|job| job.closed_at.is_none())
            .filter(|job| {
                job.leases
                    .iter()
                    // A node that committed has finished running the job.
                    .any(|held| held.node == *node && held.commitment.is_none())
            })
            .map(|job| job.manifest.slots)
            .sum();
        // A stage a node runs for another takes one slot until that node commits.
        let stages = self
            .jobs
            .values()
            .filter(|job| job.closed_at.is_none())
            .flat_map(|job| &job.leases)
            .filter(|held| held.commitment.is_none())
            .flat_map(|held| &held.pipeline)
            .filter(|assigned| assigned.node == *node)
            .count() as u32;
        let used = used + stages;
        // A node offers no more slots than its hardware runs.
        let offered = match self.hardware.get(node) {
            Some(hardware) => self.preferences(node).slots.min(hardware.slots()),
            None => self.preferences(node).slots,
        };
        offered.saturating_sub(used)
    }

    /// The stages of `models` a node does not hold itself, each with its
    /// model's place among them.
    fn missing_stages(
        &self,
        node: &PublicKey,
        models: &[ContentHash],
    ) -> Vec<(u32, ContentHash, u32)> {
        let held = self
            .hardware
            .get(node)
            .map(|h| h.stages.as_slice())
            .unwrap_or(&[]);
        let mut missing = Vec::new();
        for (index, id) in models.iter().enumerate() {
            let stages = approved(id).map_or(0, |model| model.stages);
            for stage in 0..stages {
                let wanted = StageRef { model: *id, stage };
                if !held.contains(&wanted) {
                    missing.push((index as u32, *id, stage));
                }
            }
        }
        missing
    }

    /// The nodes that could be drawn to run `stage` of `model`: asking for
    /// work, with a free slot, holding the stage, taking inference work, and
    /// of no account in `barred`. In order of node key, with their accounts.
    fn stage_pool(
        &self,
        free: &BTreeMap<PublicKey, (AccountId, u32)>,
        model: &ContentHash,
        stage: u32,
        barred: &HashSet<AccountId>,
        standings: &Standings,
    ) -> Vec<(Eligible, AccountId)> {
        let wanted = StageRef {
            model: *model,
            stage,
        };
        free.iter()
            .filter(|(_, (account, slots))| *slots > 0 && !barred.contains(account))
            .filter(|(key, _)| {
                self.hardware
                    .get(key)
                    .is_some_and(|hardware| hardware.stages.contains(&wanted))
            })
            .filter(|(key, _)| {
                let preferences = self.preferences(key);
                preferences.contributing && preferences.classes.contains(&JobClass::Inference)
            })
            .map(|(key, (account, _))| {
                let standing = (standings.node)(key, account);
                let eligible = Eligible {
                    node: *key,
                    weight: (standing.weight(JobClass::Inference) * WEIGHT_SCALE).round() as u64,
                    balanced: (standings.account)(account).balanced,
                };
                (eligible, *account)
            })
            .collect()
    }

    /// Grants a lease on each open job that needs one, to a node drawn from the
    /// eligible nodes asking. Without a current beacon round, jobs wait.
    ///
    /// Jobs on the same shard of state run one at a time. The shares of one
    /// private job go to different accounts. A job is never granted to a node
    /// of an account it is kept from, or to a second node of an account that
    /// already holds a lease on it.
    fn grant(
        &mut self,
        now: u64,
        standings: &Standings,
        contents: &dyn Fn(&ShardRef) -> Option<ContentHash>,
    ) {
        let Some(beacon) = self.beacon.filter(|beacon| beacon.is_current(now)) else {
            return;
        };
        let mut free: BTreeMap<PublicKey, (AccountId, u32)> = BTreeMap::new();
        for (key, node) in &self.nodes {
            if now.saturating_sub(node.last_ask) <= ONLINE_WINDOW_SECS {
                let slots = self.free_slots(key);
                if slots > 0 {
                    free.insert(*key, (node.account, slots));
                }
            }
        }
        let mut held_shards: HashSet<ShardRef> = self
            .jobs
            .values()
            .filter(|job| job.held_shard && job.closed_at.is_none())
            .filter_map(|job| job.manifest.state.clone())
            .collect();
        let mut holders: HashMap<(AccountId, ShareSet), HashSet<AccountId>> = HashMap::new();
        for job in self.jobs.values() {
            if let Some(shares) = job.shares() {
                holders
                    .entry(shares)
                    .or_default()
                    .extend(job.leases.iter().map(|held| held.account));
            }
        }
        // Active accounts and shares per tier, for share caps.
        let mut tier_shares: HashMap<u32, (usize, HashMap<AccountId, u64>)> = HashMap::new();
        for tier in 0..=MAX_TIER {
            let (granted, shares) = self.shares.of_tier(tier, now);
            let asking: HashSet<AccountId> = free.values().map(|(account, _)| *account).collect();
            let active = granted.max(asking.len());
            tier_shares.insert(tier, (active, shares));
        }

        let open: Vec<JobId> = self.open.iter().copied().collect();
        let mut still_open = VecDeque::new();
        for id in open {
            let Some(job) = self.jobs.get(&id) else {
                continue;
            };
            if job.closed_at.is_some() || job.phase != Phase::Open {
                continue;
            }
            let wanted = job.leases_wanted(now);
            if wanted == 0 {
                still_open.push_back(id);
                continue;
            }
            if job
                .manifest
                .state
                .as_ref()
                .is_some_and(|shard| held_shards.contains(shard) && !job.held_shard)
            {
                still_open.push_back(id);
                continue;
            }
            let apart: HashSet<AccountId> = job
                .manifest
                .apart_from
                .iter()
                .filter_map(|other| self.jobs.get(other))
                .flat_map(|other| other.leases.iter().map(|held| held.account))
                .collect();
            let mut barred = job.kept_from.clone();
            barred.extend(apart);
            barred.extend(job.leases.iter().map(|held| held.account));
            if let Some(shares) = job.shares().and_then(|shares| holders.get(&shares)) {
                barred.extend(shares.iter().copied());
            }
            let pickup = match (&job.kind, job.leases.len()) {
                _ if job.refereeing.is_some() => Pickup::Referee,
                (Kind::Audit { .. }, _) => Pickup::Audit,
                (_, 0) => Pickup::First,
                _ => Pickup::Second,
            };
            let tier = job.manifest.tier();
            let class = job.manifest.class;
            let (active, shares) = &tier_shares[&tier];
            let capped = *active >= SHARE_CAP_MIN_ACCOUNTS;

            // Who may be granted this lease in standard allocation, and through
            // the open market.
            let mut standard: Vec<(Eligible, AccountId, u64)> = Vec::new();
            let mut market: Vec<(Eligible, AccountId, u64, ContentHash, u64)> = Vec::new();
            for (key, (account, slots)) in &free {
                if *slots < job.manifest.slots || barred.contains(account) {
                    continue;
                }
                let standing = (standings.node)(key, account);
                if !may_take(&standing, &job.manifest, job.rate) {
                    continue;
                }
                // A job that calls models is offered only to a node that
                // holds them all, and has a GPU if one of them needs it.
                if !runs_models(self.hardware.get(key), &job.manifest.models) {
                    continue;
                }
                let mut barred_here = barred.clone();
                barred_here.insert(*account);
                let stages_drawable = self.missing_stages(key, &job.manifest.models).iter().all(
                    |(_, model, stage)| {
                        !self
                            .stage_pool(&free, model, *stage, &barred_here, standings)
                            .is_empty()
                    },
                );
                if !stages_drawable {
                    continue;
                }
                let account_standing = (standings.account)(account);
                let cap = share_cap_ppm(&account_standing, now);
                let share = shares.get(account).copied().unwrap_or(0);
                if capped && share >= cap {
                    continue;
                }
                let taper = if capped {
                    taper(share, cap)
                } else {
                    PREMIUM_SCALE
                };
                let eligible = Eligible {
                    node: *key,
                    weight: (standing.weight(class) * WEIGHT_SCALE).round() as u64,
                    balanced: account_standing.balanced,
                };
                let preferences = self.preferences(key);
                if preferences.accepts(class, tier, job.rate) {
                    standard.push((eligible, *account, taper));
                }
                // Only a first pickup in the market tiers fills a bid; the
                // confirming node is always drawn at random.
                let market_ok = preferences.contributing && preferences.classes.contains(&class);
                if pickup == Pickup::First && tier >= MARKET_FIRST_TIER && market_ok {
                    let best = self
                        .bids
                        .values()
                        .filter(|bid| bid.account == *account && bid.is_live(now))
                        .filter(|bid| bid.matches(class, tier, job.rate, job.manifest.compute_cap))
                        .min_by_key(|bid| bid.rank_price(job.manifest.compute_cap));
                    if let Some(bid) = best {
                        market.push((
                            eligible,
                            *account,
                            taper,
                            bid.id,
                            bid.rank_price(job.manifest.compute_cap),
                        ));
                    }
                }
            }

            // The lowest bids win; the draw breaks ties among them.
            let lowest = market.iter().map(|entry| entry.4).min();
            let (draw_from, filled): (Vec<Candidate>, bool) = match lowest {
                Some(lowest) => (
                    market
                        .into_iter()
                        .filter(|entry| entry.4 == lowest)
                        .map(|(eligible, account, taper, bid, _)| {
                            (eligible, account, taper, Some(bid))
                        })
                        .collect(),
                    true,
                ),
                None => (
                    standard
                        .into_iter()
                        .map(|(eligible, account, taper)| (eligible, account, taper, None))
                        .collect(),
                    false,
                ),
            };
            let eligible: Vec<Eligible> = draw_from.iter().map(|entry| entry.0).collect();
            let Some(draw) = Draw::make(id, pickup, now, beacon, eligible, None) else {
                still_open.push_back(id);
                continue;
            };
            let (_, account, taper, bid) = draw_from
                .iter()
                .find(|entry| entry.0.node == draw.node)
                .copied()
                .expect("the drawn node was eligible");
            let draw = Draw {
                bid: bid.filter(|_| filled),
                ..draw
            };
            let node = draw.node;
            let price = match bid.and_then(|bid| self.bids.get_mut(&bid)) {
                Some(bid) => {
                    bid.filled += 1;
                    LeasePrice::Bid {
                        bid: bid.id,
                        price: bid.order.price,
                    }
                }
                None => LeasePrice::Posted {
                    micros_per_unit: job.rate,
                },
            };
            // Draw a node for each stage of the job's models the node lacks.
            let lease = job.leases.len() as u32;
            let mut barred_here = barred.clone();
            barred_here.insert(account);
            let mut pipeline = Vec::new();
            let mut stage_draws = Vec::new();
            for (model_index, model, stage) in self.missing_stages(&node, &job.manifest.models) {
                let pool = self.stage_pool(&free, &model, stage, &barred_here, standings);
                let eligible: Vec<Eligible> = pool.iter().map(|(e, _)| *e).collect();
                let pickup = Pickup::Stage {
                    lease,
                    model: model_index,
                    stage,
                };
                let Some(draw) = Draw::make(id, pickup, now, beacon, eligible, None) else {
                    break;
                };
                let stage_account = pool
                    .iter()
                    .find(|(e, _)| e.node == draw.node)
                    .map(|(_, a)| *a)
                    .expect("the drawn node was eligible");
                if let Some((_, slots)) = free.get_mut(&draw.node) {
                    *slots = slots.saturating_sub(1);
                }
                pipeline.push(Assigned {
                    model,
                    stage,
                    node: draw.node,
                    account: stage_account,
                });
                stage_draws.push(draw);
            }
            if pipeline.len() < self.missing_stages(&node, &job.manifest.models).len() {
                // A stage node ran out of slots to an earlier stage; try again
                // next round.
                for assigned in &pipeline {
                    if let Some((_, slots)) = free.get_mut(&assigned.node) {
                        *slots += 1;
                    }
                }
                still_open.push_back(id);
                continue;
            }
            let job = self.jobs.get(&id).expect("open jobs exist");
            let proven = (standings.node)(&node, &account).proven;
            if let Some((_, slots)) = free.get_mut(&node) {
                *slots = slots.saturating_sub(job.manifest.slots);
            }
            if let Some(shares) = self.jobs[&id].shares() {
                holders.entry(shares).or_default().insert(account);
            }
            self.shares.record(tier, account, now);
            let job = self.jobs.get_mut(&id).expect("open jobs exist");
            if job.leases.is_empty()
                && let Some(shard) = &job.manifest.state
            {
                job.shard = contents(shard);
                job.held_shard = true;
                held_shards.insert(shard.clone());
            }
            job.leases.push(Held {
                node,
                account,
                price,
                taper,
                expires_at: (now + LEASE_SECS).min(job.manifest.deadline),
                proven,
                referee: job.refereeing.is_some(),
                granted_at: now,
                committed_at: 0,
                commitment: None,
                transcript: None,
                pipeline,
                revealed: None,
            });
            job.draws.push(draw);
            job.draws.extend(stage_draws);
            if matches!(job.status, JobStatus::Submitted) {
                job.status = JobStatus::Leased;
            }
            still_open.push_back(id);
        }
        self.open = still_open;
    }

    /// Hands `node` a job whose answer is already known, if there is one it may take.
    fn issue_check(
        &mut self,
        node: PublicKey,
        account: AccountId,
        now: u64,
        rng: &mut impl Rng,
        standing: &Standing,
        stored: &dyn Fn(&ContentHash) -> bool,
    ) {
        let usable: Vec<&Known> = self
            .known
            .iter()
            .filter(|known| standing.max_tier >= known.manifest.tier())
            .filter(|known| !known.kept_from.contains(&account))
            .filter(|known| {
                let manifest = &known.manifest;
                let shard = known.shard.iter();
                [manifest.module, manifest.input]
                    .iter()
                    .chain(&manifest.reads)
                    .chain(shard)
                    .all(stored)
            })
            .collect();
        if usable.is_empty() {
            return;
        }
        let expected = usable[rng.random_range(0..usable.len())].clone();
        // To the node this must look like any other job, so it gets a deadline of
        // its own and an ID drawn at random.
        let job_id = JobId::from_bytes(rng.random());
        let mut manifest = expected.manifest.clone();
        manifest.deadline = now + CHECK_DEADLINE_SECS;
        let rate = self
            .rates
            .posted(manifest.class, manifest.tier())
            .micros_per_unit;
        let shard = expected.shard;
        let mut job = Job::new(
            Kind::KnownAnswer {
                expected: Box::new(expected),
            },
            manifest,
            rate,
            now,
        );
        job.shard = shard;
        job.leases.push(Held {
            node,
            account,
            price: LeasePrice::Posted {
                micros_per_unit: rate,
            },
            taper: PREMIUM_SCALE,
            expires_at: now + CHECK_DEADLINE_SECS,
            proven: true,
            referee: false,
            granted_at: now,
            committed_at: 0,
            commitment: None,
            transcript: None,
            pipeline: Vec::new(),
            revealed: None,
        });
        job.status = JobStatus::Leased;
        self.jobs.insert(job_id, job);
        if let Some(entry) = self.nodes.get_mut(&node) {
            entry.known_answer_due = false;
        }
    }

    /// Expires leases and jobs, moves jobs whose windows closed, releases holds,
    /// retunes rates, and forgets what is no longer needed. This walks every
    /// job and node; fine while both live in one process's memory.
    fn sweep(&mut self, now: u64) {
        // Leases whose node never committed reopen for another node.
        let mut refill: Vec<ContentHash> = Vec::new();
        for job in self.jobs.values_mut() {
            if job.closed_at.is_some() || job.phase != Phase::Open {
                continue;
            }
            job.leases.retain(|held| {
                let lapsed = held.commitment.is_none() && held.expires_at <= now;
                if lapsed {
                    self.events.push(ScoreEvent {
                        node: held.node,
                        at: now,
                        what: Conduct::Missed,
                    });
                    if let LeasePrice::Bid { bid, .. } = held.price {
                        refill.push(bid);
                    }
                }
                !lapsed
            });
            if job.leases.is_empty() && matches!(job.status, JobStatus::Leased) {
                job.status = JobStatus::Submitted;
            }
        }
        for bid in refill {
            if let Some(bid) = self.bids.get_mut(&bid) {
                bid.filled = bid.filled.saturating_sub(1);
            }
        }

        let ids: Vec<JobId> = self.jobs.keys().copied().collect();
        for id in ids {
            let job = &self.jobs[&id];
            if job.closed_at.is_some() || job.phase == Phase::Closed {
                continue;
            }
            let past_deadline = job.manifest.deadline <= now;
            match job.phase {
                Phase::Open if past_deadline && job.refereeing.is_some() => {
                    // The dispute could not be settled in time.
                    self.decide(id, &[], now);
                }
                Phase::Open if past_deadline => {
                    if job.committed() > 0 {
                        // Whatever was committed by the deadline is revealed.
                        let job = self.jobs.get_mut(&id).expect("exists");
                        job.leases.retain(|held| held.commitment.is_some());
                        job.phase = Phase::Revealing {
                            until: now + REVEAL_WINDOW_SECS,
                        };
                        job.status = JobStatus::Committed;
                    } else {
                        self.expire(id, now);
                    }
                }
                Phase::Open | Phase::Revealing { .. } => self.advance(id, now),
                Phase::Closed => {}
            }
        }

        while self.holds.front().is_some_and(|(at, _)| *at <= now) {
            let (_, job_id) = self.holds.pop_front().expect("checked");
            self.closed.push(ClosedJob {
                job_id,
                how: Closure::Release,
            });
        }

        if self.rates.is_due(now) {
            let load = self.load(now);
            self.rates.retune(&load, now);
        }

        self.open
            .retain(|id| self.jobs.get(id).is_some_and(|job| job.closed_at.is_none()));
        self.jobs.retain(|_, job| {
            job.closed_at
                .is_none_or(|at| now.saturating_sub(at) <= CLOSED_JOB_RETENTION_SECS)
        });
        self.nodes
            .retain(|_, node| now.saturating_sub(node.last_ask) <= ONLINE_WINDOW_SECS * 6);
        self.bids.retain(|_, bid| bid.is_live(now));
    }

    /// For each class and tier: the jobs waiting for a lease and the nodes
    /// asking that would take one.
    fn load(&self, now: u64) -> HashMap<(JobClass, u32), (u64, u64)> {
        let mut load: HashMap<(JobClass, u32), (u64, u64)> = HashMap::new();
        for job in self.jobs.values() {
            if job.closed_at.is_none() && job.phase == Phase::Open && job.leases_wanted(now) > 0 {
                load.entry((job.manifest.class, job.manifest.tier()))
                    .or_default()
                    .0 += 1;
            }
        }
        for (key, node) in &self.nodes {
            if now.saturating_sub(node.last_ask) > ONLINE_WINDOW_SECS {
                continue;
            }
            let preferences = self.preferences(key);
            for class in JobClass::ALL {
                for tier in 0..=MAX_TIER {
                    let rate = self.rates.posted(class, tier).micros_per_unit;
                    if preferences.accepts(class, tier, rate) {
                        load.entry((class, tier)).or_default().1 += 1;
                    }
                }
            }
        }
        load
    }
}

impl Job {
    fn new(kind: Kind, manifest: Manifest, rate: u64, now: u64) -> Self {
        Self {
            kind,
            manifest,
            kept_from: HashSet::new(),
            submitted_at: now,
            rate,
            shard: None,
            held_shard: false,
            leases: Vec::new(),
            draws: Vec::new(),
            confirm_until: None,
            refereeing: None,
            phase: Phase::Open,
            status: JobStatus::Submitted,
            closed_at: None,
        }
    }

    fn committed(&self) -> usize {
        self.leases
            .iter()
            .filter(|held| held.commitment.is_some())
            .count()
    }

    /// How many more leases the job wants granted now.
    fn leases_wanted(&self, now: u64) -> usize {
        if let Some(refereeing) = &self.refereeing {
            let waiting = self
                .leases
                .iter()
                .any(|held| held.referee && held.revealed.is_none());
            return usize::from(!waiting && refereeing.referees < MAX_REFEREES);
        }
        let single = matches!(self.kind, Kind::KnownAnswer { .. } | Kind::Audit { .. })
            || self.manifest.verify == Verify::None;
        let target = if single { 1 } else { 2 };
        if self.leases.len() >= target {
            return 0;
        }
        // The second lease is offered only once the first node committed, and
        // only while the confirmation window is open.
        if !self.leases.is_empty()
            && (self.committed() == 0 || self.confirm_until.is_none_or(|until| now >= until))
        {
            return 0;
        }
        1
    }

    /// If the job is a share of a private job: whose, and the name its shares
    /// carry.
    fn shares(&self) -> Option<(AccountId, ShareSet)> {
        match (&self.kind, self.manifest.share) {
            (Kind::Submitted { submitter }, Some(share)) => Some((*submitter, share.set)),
            _ => None,
        }
    }

    fn lease_for(&self, job_id: JobId, held: &Held) -> Lease {
        let (seed, transcript) = match &self.kind {
            Kind::KnownAnswer { expected } | Kind::Audit { expected, .. } => {
                (expected.seed, expected.result.transcript)
            }
            // Every node after the first replays the first node's model calls.
            Kind::Submitted { .. } => {
                let first = self.leases.first().filter(|first| first.node != held.node);
                (job_id, first.and_then(|first| first.transcript))
            }
        };
        Lease {
            job_id,
            expires_at: held.expires_at,
            price: held.price,
            manifest: self.manifest.as_handed(),
            seed,
            shard: self.shard,
            transcript,
            pipeline: held
                .pipeline
                .iter()
                .map(|assigned| PipelineStage {
                    model: assigned.model,
                    stage: assigned.stage,
                    node: assigned.node,
                    address: None,
                })
                .collect(),
        }
    }

    fn finish(&mut self, status: JobStatus, now: u64) {
        self.status = status;
        self.closed_at = Some(now);
        self.phase = Phase::Closed;
    }

    #[allow(clippy::too_many_arguments)]
    fn receipt(
        &self,
        job_id: JobId,
        result: &JobResult,
        split: FeeSplit,
        results: Vec<Envelope>,
        verified: Verification,
        runs: u32,
        now: u64,
    ) -> Receipt {
        Receipt {
            job_id,
            module: self.manifest.module,
            input: self.manifest.input,
            units: result.units,
            fee: split,
            outcome: result.outcome.clone(),
            state: self.manifest.state.clone(),
            handed: self.shard,
            results,
            pieces: Vec::new(),
            route: privacy::route_to_nodes(&self.manifest),
            verified,
            runs,
            seconds: now.saturating_sub(self.submitted_at),
            enclave: false,
        }
    }

    fn follow_ups(
        &self,
        job_id: JobId,
        submitter: AccountId,
        receipt: &Receipt,
    ) -> Option<FollowUps> {
        let Outcome::Completed { spawned, .. } = &receipt.outcome else {
            return None;
        };
        if spawned.is_empty() {
            return None;
        }
        let ask = |(index, spawn): (usize, &Spawn)| {
            let asked = Manifest {
                module: spawn.module,
                input: spawn.input,
                compute_cap: spawn.compute_cap,
                split: None,
                state: None,
                ..self.manifest.clone()
            };
            let manifest = Manifest {
                max_fee: asked.highest_fee(self.rate, fallback_rate(asked.class)),
                ..asked
            };
            (JobId::follow_up(&job_id, index as u64), manifest)
        };
        Some(FollowUps {
            submitter,
            spare: self.manifest.max_fee - receipt.fee.total,
            jobs: spawned.iter().enumerate().map(ask).collect(),
        })
    }
}

/// Whether a node with this standing may be granted this job: its tier must be
/// within the node's reach, and from tier 3 up the node's account must have
/// staked enough to cover the job (spec section 3, "Tiers").
fn may_take(standing: &Standing, manifest: &Manifest, rate: u64) -> bool {
    let tier = manifest.tier();
    tier <= standing.max_tier
        && (tier < FIRST_STAKED_TIER || standing.stake >= compute_fee(manifest.compute_cap, rate))
}
