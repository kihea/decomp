//! The coordinator's job rules: which nodes run a job, how their work is checked,
//! and when a result stands (spec section 2 "Lifecycle", section 3 "Assignment",
//! and section 4).
//!
//! Like `Accounts`, this does no I/O. The caller passes in the time, each round
//! of the public randomness beacon, each node's standing, and the contents of
//! each shard of state, so the same calls always produce the same decisions.
//! Which nodes run a job is drawn from the beacon (see `draw`); only the choice
//! of known-answer jobs uses randomness of the caller's own, because a node must
//! not be able to tell one is coming. What the caller must act on comes back through three lists it
//! takes after each call: jobs that closed, score events, and disputes to referee.
//!
//! Nothing here is durable: assignments and results are needed only until a job
//! closes (spec section 6). What lasts is the receipt, which the caller stores.

use std::collections::{HashMap, HashSet, VecDeque};

use rand::{Rng, RngExt};

use crate::draw::{Beacon, Draw, Eligible, WEIGHT_SCALE};
use crate::envelope::Envelope;
use crate::job::{
    Assignment, JobResult, JobStatus, Manifest, Outcome, Receipt, Spawn, Verification,
};
use crate::keys::{AccountId, ContentHash, JobId, PublicKey, ShareSet};
use crate::ledger::{Credits, FeeSplit, fee};
use crate::privacy;
use crate::scores::{Conduct, FIRST_STAKED_TIER, ScoreEvent, Standing};
use crate::state::ShardRef;

/// A node counts as online for this long after it last checked in.
pub const ONLINE_WINDOW_SECS: u64 = 10;

/// How long a closed job stays readable by its submitter before it is forgotten.
pub const CLOSED_JOB_RETENTION_SECS: u64 = 3600;

/// After a node reports a result, the chance that its next job is one whose
/// answer the coordinator already knows. A starting value.
pub const KNOWN_ANSWER_RATE: f64 = 0.02;

/// How many verified jobs are remembered to be reissued as known-answer jobs.
const KNOWN_ANSWERS_KEPT: usize = 256;

/// How long a node is given to finish a known-answer job.
const KNOWN_ANSWER_DEADLINE_SECS: u64 = 300;

#[derive(Debug, thiserror::Error)]
pub enum JobError {
    #[error("this job was already submitted")]
    JobExists,
    #[error("a job must run on at least one node")]
    NoNodes,
    #[error("the job's deadline has already passed")]
    DeadlinePassed,
    #[error("the compute cap must be at least one unit")]
    NoComputeCap,
    #[error("no such job")]
    UnknownJob,
    #[error("the job is no longer accepting results")]
    JobClosed,
    #[error("this node was not assigned the job")]
    NotAssigned,
    #[error("this node already reported a result for the job")]
    AlreadyReported,
    #[error("the result claims more compute than the job was allowed")]
    UnitsOverCap,
}

/// A job that has just closed, for the caller to settle.
pub struct ClosedJob {
    pub job_id: JobId,
    pub how: Closure,
}

pub enum Closure {
    /// A result stood. `node_accounts` are paid from the job's fee. `checker`, if
    /// any, ran the job as a spot check and is paid the same from the
    /// verification fund.
    Verified {
        receipt: Box<Receipt>,
        node_accounts: Vec<AccountId>,
        checker: Option<AccountId>,
        /// The follow-up jobs the job asked for, if any, for the caller to start.
        follow_ups: Option<FollowUps>,
    },
    /// No node was right. The submitter is refunded.
    Disputed,
    /// The submitter is refunded.
    Expired,
    /// A node got a known-answer job right and is paid from the verification fund.
    KnownAnswerPassed { account: AccountId, reward: Credits },
}

/// The follow-up jobs a verified job asked for. The caller starts them in order,
/// each like a job its submitter had sent, for as long as `spare` covers what
/// is set aside for them.
pub struct FollowUps {
    /// The account that submitted the job that asked.
    pub submitter: AccountId,
    /// What that job left of its fee cap.
    pub spare: Credits,
    pub jobs: Vec<(JobId, Manifest)>,
}

/// Nodes disagreed about a job. The caller referees it (which means running the
/// job, so it cannot happen here) and answers with [`Jobs::resolve`].
pub struct Dispute {
    pub job_id: JobId,
    pub manifest: Manifest,
    /// The contents of the shard the job was handed, if any.
    pub shard: Option<ContentHash>,
    pub reports: Vec<(PublicKey, JobResult)>,
}

enum Kind {
    Submitted {
        submitter: AccountId,
    },
    /// Reissued to one node to see whether it reports what is already known.
    KnownAnswer {
        expected: Box<Known>,
    },
}

/// A verified job and its result, kept to be reissued as a known-answer job.
#[derive(Clone)]
struct Known {
    manifest: Manifest,
    /// The accounts the job was kept from, which it is kept from again.
    kept_from: HashSet<AccountId>,
    seed: JobId,
    /// The contents of the shard the job was handed, if any.
    shard: Option<ContentHash>,
    units: u64,
    outcome: Outcome,
    checkpoints: ContentHash,
}

struct Job {
    kind: Kind,
    manifest: Manifest,
    /// The accounts whose nodes are never handed the job: those that the keys
    /// in its manifest's `keep_from` belong to.
    kept_from: HashSet<AccountId>,
    /// When the job was accepted, in seconds since 1970.
    submitted_at: u64,
    /// The contents its shard had when the job was handed out, which is what
    /// every node on it reads. `None` before then, and for an empty shard.
    shard: Option<ContentHash>,
    workers: Vec<Worker>,
    /// How its nodes were drawn, kept until the job closes and then published.
    draw: Option<Draw>,
    results: Vec<Report>,
    status: JobStatus,
    closed_at: Option<u64>,
    /// The nodes disagreed and the caller is working out which is right.
    in_dispute: bool,
}

/// A node running a job. A checker runs it as a spot check: the same work, paid
/// from the verification fund instead of the job's fee.
struct Worker {
    node: PublicKey,
    account: AccountId,
    checker: bool,
}

struct Report {
    node: PublicKey,
    result: JobResult,
    /// The signed request the result arrived in, kept for the receipt.
    envelope: Envelope,
}

/// A device that has checked in for work. A node runs one job at a time.
struct Node {
    account: AccountId,
    last_seen: u64,
    job: Option<JobId>,
    /// Its next job should be a known-answer job.
    known_answer_due: bool,
}

/// An idle, online node being considered for a job.
struct Candidate {
    node: PublicKey,
    account: AccountId,
    standing: Standing,
}

/// Every open job and every node known to be online.
#[derive(Default)]
pub struct Jobs {
    jobs: HashMap<JobId, Job>,
    /// Jobs not yet handed to nodes, oldest first.
    waiting: VecDeque<JobId>,
    nodes: HashMap<PublicKey, Node>,
    known: VecDeque<Known>,
    closed: Vec<ClosedJob>,
    events: Vec<ScoreEvent>,
    disputes: Vec<Dispute>,
    /// The draws of jobs that have closed, for the caller to publish.
    draws: Vec<Draw>,
    /// The newest round of public randomness the caller has passed in.
    beacon: Option<Beacon>,
}

impl Jobs {
    /// Whether `manifest` describes a job that can be accepted as `id` right now.
    pub fn check(&self, id: &JobId, manifest: &Manifest, now: u64) -> Result<(), JobError> {
        if manifest.redundancy == Some(0) {
            return Err(JobError::NoNodes);
        }
        if manifest.deadline <= now {
            return Err(JobError::DeadlinePassed);
        }
        if manifest.compute_cap == 0 {
            return Err(JobError::NoComputeCap);
        }
        if self.jobs.contains_key(id) {
            return Err(JobError::JobExists);
        }
        Ok(())
    }

    /// Takes in the newest round of public randomness, which the nodes for
    /// waiting jobs are drawn from (spec section 6: "Assignment draws on public
    /// randomness").
    pub fn set_beacon(&mut self, beacon: Beacon) {
        self.beacon = Some(beacon);
    }

    /// Accepts a job. The caller sets aside its highest possible fee first, so a
    /// job never runs without the credits to pay for it.
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
        let mut job = Job::new(Kind::Submitted { submitter }, manifest, now);
        job.kept_from = kept_from;
        self.jobs.insert(id, job);
        self.waiting.push_back(id);
        Ok(())
    }

    /// Records that `node` is online and returns the job it should be running.
    /// Nodes do not choose work: each check-in also hands waiting jobs to idle
    /// nodes drawn at random, and a node learns of its job here.
    ///
    /// `contents` gives the contents a shard of state has now. The caller must
    /// have applied what every verified job left behind before calling this, so
    /// that a job handed out here starts from where the last one ended.
    pub fn check_in(
        &mut self,
        node: PublicKey,
        account: AccountId,
        now: u64,
        rng: &mut impl Rng,
        standing: &dyn Fn(&PublicKey, &AccountId) -> Standing,
        contents: &dyn Fn(&ShardRef) -> Option<ContentHash>,
    ) -> Option<Assignment> {
        self.sweep(now);
        let entry = self.nodes.entry(node).or_insert(Node {
            account,
            last_seen: now,
            job: None,
            known_answer_due: false,
        });
        entry.last_seen = now;
        self.assign(now, standing, contents);
        if self.nodes[&node].job.is_none() && self.nodes[&node].known_answer_due {
            self.issue_known_answer(node, account, now, rng, &standing(&node, &account));
        }

        let job_id = self.nodes[&node].job?;
        let job = &self.jobs[&job_id];
        let seed = match &job.kind {
            Kind::KnownAnswer { expected } => expected.seed,
            Kind::Submitted { .. } => job_id,
        };
        Some(Assignment {
            job_id,
            manifest: job.manifest.as_handed(),
            seed,
            shard: job.shard,
        })
    }

    /// Records a node's result. Once every node on the job has reported, the job
    /// closes if they agree, and goes to the caller as a dispute if they do not.
    pub fn report(
        &mut self,
        node: PublicKey,
        result: JobResult,
        envelope: Envelope,
        now: u64,
        rng: &mut impl Rng,
    ) -> Result<(), JobError> {
        self.sweep(now);
        let job_id = result.job_id;
        let job = self.jobs.get_mut(&job_id).ok_or(JobError::UnknownJob)?;
        if job.closed_at.is_some() || job.in_dispute {
            return Err(JobError::JobClosed);
        }
        if !job.workers.iter().any(|worker| worker.node == node) {
            return Err(JobError::NotAssigned);
        }
        if job.results.iter().any(|report| report.node == node) {
            return Err(JobError::AlreadyReported);
        }
        // The fee is set from the units reported, and only the cap was set aside.
        if result.units > job.manifest.compute_cap {
            return Err(JobError::UnitsOverCap);
        }
        job.results.push(Report {
            node,
            result,
            envelope,
        });
        self.events.push(ScoreEvent {
            node,
            at: now,
            what: Conduct::Finished,
        });
        if let Some(entry) = self.nodes.get_mut(&node) {
            entry.job = None;
            if matches!(job.kind, Kind::Submitted { .. }) && rng.random_bool(KNOWN_ANSWER_RATE) {
                entry.known_answer_due = true;
            }
        }
        if job.results.len() < job.workers.len() {
            return Ok(());
        }

        if let Kind::KnownAnswer { expected } = &job.kind {
            let found = &job.results[0].result;
            let passed = found.units == expected.units
                && found.outcome == expected.outcome
                && found.checkpoints == expected.checkpoints;
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
                let reward = FeeSplit::of(expected.units, 1).per_node;
                let account = job.workers[0].account;
                let how = Closure::KnownAnswerPassed { account, reward };
                self.closed.push(ClosedJob { job_id, how });
            }
            job.closed_at = Some(now);
            return Ok(());
        }

        let first = &job.results[0].result;
        if job.results.iter().all(|report| report.result == *first) {
            let everyone: Vec<PublicKey> = job.results.iter().map(|report| report.node).collect();
            self.close(job_id, &everyone, now);
        } else {
            job.in_dispute = true;
            self.disputes.push(Dispute {
                job_id,
                manifest: job.manifest.clone(),
                shard: job.shard,
                reports: job
                    .results
                    .iter()
                    .map(|r| (r.node, r.result.clone()))
                    .collect(),
            });
        }
        Ok(())
    }

    /// Closes a disputed job once the caller has worked out which nodes, if any,
    /// reported the right result.
    pub fn resolve(&mut self, job_id: JobId, right: &[PublicKey], now: u64) {
        if self.jobs.get(&job_id).is_some_and(|job| job.in_dispute) {
            self.close(job_id, right, now);
        }
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

    /// Whether any node of `account` is in the middle of a job.
    pub fn is_working(&self, account: &AccountId) -> bool {
        self.nodes
            .values()
            .any(|node| node.account == *account && node.job.is_some())
    }

    /// The jobs that closed since this was last called. Each has credits to move.
    pub fn take_closed(&mut self) -> Vec<ClosedJob> {
        std::mem::take(&mut self.closed)
    }

    /// What nodes did since this was last called, for their scores.
    pub fn take_events(&mut self) -> Vec<ScoreEvent> {
        std::mem::take(&mut self.events)
    }

    /// The draws of the jobs that closed since this was last called (spec
    /// section 6: "Each operator publishes its draws").
    pub fn take_draws(&mut self) -> Vec<Draw> {
        std::mem::take(&mut self.draws)
    }

    /// The disputes that arose since this was last called.
    pub fn take_disputes(&mut self) -> Vec<Dispute> {
        std::mem::take(&mut self.disputes)
    }

    /// Closes a submitted job whose reports are all in. `right` are the nodes
    /// whose result stands; everyone else who reported was wrong.
    fn close(&mut self, job_id: JobId, right: &[PublicKey], now: u64) {
        let job = self
            .jobs
            .get_mut(&job_id)
            .expect("the job being closed exists");
        job.closed_at = Some(now);
        self.draws.extend(job.draw.take());
        // A job is closed here either because every node agreed or, if it was
        // in dispute, because the referee has said which nodes were right.
        let verified = match (job.in_dispute, right.len()) {
            (true, _) => Verification::Refereed,
            (false, 1) => Verification::Unchecked,
            (false, _) => Verification::Agreed,
        };
        job.in_dispute = false;
        for report in &job.results {
            let what = if right.contains(&report.node) {
                Conduct::Matched
            } else {
                Conduct::Wrong
            };
            self.events.push(ScoreEvent {
                node: report.node,
                at: now,
                what,
            });
        }
        let Some(standing) = job
            .results
            .iter()
            .find(|report| right.contains(&report.node))
        else {
            job.status = JobStatus::Disputed;
            self.closed.push(ClosedJob {
                job_id,
                how: Closure::Disputed,
            });
            return;
        };
        let result = standing.result.clone();

        // The job's fee pays the nodes that were right. A checker that was right
        // is paid from the verification fund, unless it is the only one who was.
        let right_workers = || {
            job.workers
                .iter()
                .filter(|worker| right.contains(&worker.node))
        };
        let mut node_accounts: Vec<AccountId> = right_workers()
            .filter(|worker| !worker.checker)
            .map(|w| w.account)
            .collect();
        let mut checker = right_workers()
            .find(|worker| worker.checker)
            .map(|w| w.account);
        if node_accounts.is_empty() {
            node_accounts.extend(checker.take());
        }

        let receipt = Receipt {
            job_id,
            module: job.manifest.module,
            input: job.manifest.input,
            units: result.units,
            fee: FeeSplit::of(result.units, node_accounts.len() as u32),
            outcome: result.outcome.clone(),
            state: job.manifest.state.clone(),
            handed: job.shard,
            results: job
                .results
                .iter()
                .filter(|report| right.contains(&report.node))
                .map(|report| report.envelope.clone())
                .collect(),
            pieces: Vec::new(),
            route: privacy::route_to_nodes(&job.manifest),
            verified,
            seconds: now.saturating_sub(job.submitted_at),
            enclave: false,
        };
        job.status = JobStatus::Verified {
            receipt: Box::new(receipt.clone()),
        };
        // Two nodes agreeing is what makes an answer known. A share is never
        // reissued: the node asked might be of an account that holds another
        // share of the same job.
        if right.len() >= 2 && job.manifest.share.is_none() {
            self.known.push_back(Known {
                manifest: job.manifest.clone(),
                kept_from: job.kept_from.clone(),
                seed: job_id,
                shard: job.shard,
                units: result.units,
                outcome: result.outcome,
                checkpoints: result.checkpoints,
            });
            if self.known.len() > KNOWN_ANSWERS_KEPT {
                self.known.pop_front();
            }
        }
        let follow_ups = match (&job.kind, &receipt.outcome) {
            (Kind::Submitted { submitter }, Outcome::Completed { spawned, .. })
                if !spawned.is_empty() =>
            {
                let ask = |(index, spawn): (usize, &Spawn)| {
                    let asked = Manifest {
                        module: spawn.module,
                        input: spawn.input,
                        compute_cap: spawn.compute_cap,
                        split: None,
                        state: None,
                        ..job.manifest.clone()
                    };
                    let manifest = Manifest {
                        max_fee: asked.highest_fee(),
                        ..asked
                    };
                    (JobId::follow_up(&job_id, index as u64), manifest)
                };
                Some(FollowUps {
                    submitter: *submitter,
                    spare: job.manifest.max_fee - receipt.fee.total,
                    jobs: spawned.iter().enumerate().map(ask).collect(),
                })
            }
            _ => None,
        };
        let how = Closure::Verified {
            receipt: Box::new(receipt),
            node_accounts,
            checker,
            follow_ups,
        };
        self.closed.push(ClosedJob { job_id, how });
    }

    /// Hands each waiting job to idle online nodes, drawn at random in proportion
    /// to their scores, one per account. A job is handed out whole or not at all,
    /// so that every node on it is drawn from everyone eligible and not from
    /// whoever checked in first. The draw comes from the newest round of the
    /// beacon by the rule in `draw`, and its record is kept with the job.
    /// Without a round that is current, jobs wait.
    ///
    /// Jobs on the same shard of state run one at a time: a job waits while
    /// another that was handed its shard is still open.
    ///
    /// The shares of one private job go to different accounts (spec section 7:
    /// "shares held by nodes on different accounts"). A share is never handed to
    /// a node of an account that was handed any share of the same job, so it
    /// waits until enough other accounts have idle nodes. Which accounts hold a
    /// job's shares is known for as long as the shares' jobs are remembered,
    /// which is why a device submits all of a job's shares together.
    ///
    /// A job is never handed to a node of an account it is kept from: one that
    /// a key in its `keep_from` belongs to, or one that was handed a job named
    /// in its `apart_from`. It waits until enough other accounts have idle
    /// nodes.
    fn assign(
        &mut self,
        now: u64,
        standing: &dyn Fn(&PublicKey, &AccountId) -> Standing,
        contents: &dyn Fn(&ShardRef) -> Option<ContentHash>,
    ) {
        let Self {
            jobs,
            waiting,
            nodes,
            beacon,
            ..
        } = self;
        let Some(beacon) = beacon.filter(|beacon| beacon.is_current(now)) else {
            return;
        };
        let mut idle: Vec<Candidate> = nodes
            .iter()
            .filter(|(_, node)| node.job.is_none() && is_online(node, now))
            .map(|(key, node)| Candidate {
                node: *key,
                account: node.account,
                standing: standing(key, &node.account),
            })
            .collect();
        // A draw lists the nodes it was made from in order of key.
        idle.sort_by_key(|candidate| candidate.node);

        // A job reissued as a known answer reads old contents and changes
        // nothing, so it does not hold its shard.
        let mut held: HashSet<ShardRef> = jobs
            .values()
            .filter(|job| matches!(job.kind, Kind::Submitted { .. }))
            .filter(|job| !job.workers.is_empty() && job.closed_at.is_none())
            .filter_map(|job| job.manifest.state.clone())
            .collect();

        // The accounts that have been handed a share of each private job.
        let mut holders: HashMap<(AccountId, ShareSet), HashSet<AccountId>> = HashMap::new();
        for job in jobs.values() {
            if let Some(shares) = job.shares() {
                let accounts = job.workers.iter().map(|worker| worker.account);
                holders.entry(shares).or_default().extend(accounts);
            }
        }

        waiting.retain(|id| {
            // The accounts that were handed a job this one is kept apart from.
            let apart_from = &jobs[id].manifest.apart_from;
            let earlier = apart_from.iter().filter_map(|other| jobs.get(other));
            let apart: Vec<AccountId> = earlier
                .flat_map(|other| other.workers.iter().map(|worker| worker.account))
                .collect();

            let job = jobs.get_mut(id).expect("waiting jobs exist");
            let shares = job.shares();
            let barred = shares.and_then(|shares| holders.get(&shares));
            let mut barred = barred.cloned().unwrap_or_default();
            barred.extend(job.kept_from.iter().copied());
            barred.extend(apart);
            if job
                .manifest
                .state
                .as_ref()
                .is_some_and(|shard| held.contains(shard))
            {
                return true;
            }
            let listed = eligible(&idle, &job.manifest, &barred);
            let redundancy = job.manifest.redundancy;
            let Some(draw) = Draw::make(*id, now, beacon, redundancy, listed) else {
                return true;
            };
            let running = draw.nodes.iter().map(|node| (node, false));
            for (drawn, checker) in running.chain(draw.checker.iter().map(|node| (node, true))) {
                let place = idle.iter().position(|candidate| candidate.node == *drawn);
                let Candidate { node, account, .. } = idle.remove(place.expect("drawn from idle"));
                nodes.get_mut(&node).expect("idle nodes exist").job = Some(*id);
                if let Some(shares) = shares {
                    holders.entry(shares).or_default().insert(account);
                }
                job.workers.push(Worker {
                    node,
                    account,
                    checker,
                });
            }
            job.draw = Some(draw);
            if let Some(shard) = &job.manifest.state {
                job.shard = contents(shard);
                held.insert(shard.clone());
            }
            job.status = JobStatus::Running;
            false
        });
    }

    /// Hands `node` a job whose answer is already known, if there is one it may take.
    fn issue_known_answer(
        &mut self,
        node: PublicKey,
        account: AccountId,
        now: u64,
        rng: &mut impl Rng,
        standing: &Standing,
    ) {
        // A job is kept from the same accounts when it is reissued as when it
        // was first handed out.
        let usable: Vec<&Known> = self
            .known
            .iter()
            .filter(|known| may_take(standing, &known.manifest))
            .filter(|known| !known.kept_from.contains(&account))
            .collect();
        if usable.is_empty() {
            return;
        }
        let expected = usable[rng.random_range(0..usable.len())].clone();
        // To the node this must look like any other job, so it gets a deadline of
        // its own and an ID drawn at random, which the node cannot work out as it
        // could one made from a count of the jobs issued this way.
        let job_id = JobId::from_bytes(rng.random());
        let mut manifest = expected.manifest.clone();
        manifest.deadline = now + KNOWN_ANSWER_DEADLINE_SECS;
        let shard = expected.shard;

        let mut job = Job::new(
            Kind::KnownAnswer {
                expected: Box::new(expected),
            },
            manifest,
            now,
        );
        job.shard = shard;
        job.workers.push(Worker {
            node,
            account,
            checker: true,
        });
        job.status = JobStatus::Running;
        self.jobs.insert(job_id, job);
        let entry = self.nodes.get_mut(&node).expect("the node just checked in");
        entry.job = Some(job_id);
        entry.known_answer_due = false;
    }

    /// Expires jobs past their deadline and forgets what is no longer needed.
    /// This walks every job and node; fine while both live in one process's memory.
    fn sweep(&mut self, now: u64) {
        let Self {
            jobs,
            waiting,
            nodes,
            closed,
            events,
            draws,
            ..
        } = self;
        for (id, job) in jobs.iter_mut() {
            if job.closed_at.is_some() || job.in_dispute || job.manifest.deadline >= now {
                continue;
            }
            job.status = JobStatus::Expired;
            job.closed_at = Some(now);
            draws.extend(job.draw.take());
            if matches!(job.kind, Kind::Submitted { .. }) {
                closed.push(ClosedJob {
                    job_id: *id,
                    how: Closure::Expired,
                });
            }
            for worker in &job.workers {
                if job.results.iter().all(|report| report.node != worker.node) {
                    events.push(ScoreEvent {
                        node: worker.node,
                        at: now,
                        what: Conduct::Missed,
                    });
                }
                if let Some(node) = nodes.get_mut(&worker.node).filter(|n| n.job == Some(*id)) {
                    node.job = None;
                }
            }
        }
        waiting.retain(|id| jobs[id].closed_at.is_none());
        jobs.retain(|_, job| {
            job.closed_at
                .is_none_or(|at| now.saturating_sub(at) <= CLOSED_JOB_RETENTION_SECS)
        });
        nodes.retain(|_, node| node.job.is_some() || is_online(node, now));
    }
}

impl Job {
    /// If the job is a share of a private job: whose, and the name its shares
    /// carry. An account names its own shares, so one account cannot use
    /// another's name to keep that account's shares from being handed out.
    fn shares(&self) -> Option<(AccountId, ShareSet)> {
        match (&self.kind, self.manifest.share) {
            (Kind::Submitted { submitter }, Some(share)) => Some((*submitter, share.set)),
            _ => None,
        }
    }

    fn new(kind: Kind, manifest: Manifest, now: u64) -> Self {
        Self {
            kind,
            manifest,
            kept_from: HashSet::new(),
            submitted_at: now,
            shard: None,
            workers: Vec::new(),
            draw: None,
            results: Vec::new(),
            status: JobStatus::Waiting,
            closed_at: None,
            in_dispute: false,
        }
    }
}

fn is_online(node: &Node, now: u64) -> bool {
    now.saturating_sub(node.last_seen) <= ONLINE_WINDOW_SECS
}

/// Whether a node with this standing may be handed this job: its tier must be
/// within the node's reach, and from tier 3 up the node's account must have
/// staked enough to cover the job (spec section 3, "Tiers").
fn may_take(standing: &Standing, manifest: &Manifest) -> bool {
    let tier = manifest.tier();
    tier <= standing.max_tier
        && (tier < FIRST_STAKED_TIER || standing.stake >= fee(manifest.compute_cap, 1))
}

/// The idle nodes that may be drawn for a job, as its draw lists them: those
/// that may take it and are not of a `barred` account, in the order of `idle`.
fn eligible(idle: &[Candidate], manifest: &Manifest, barred: &HashSet<AccountId>) -> Vec<Eligible> {
    // Nodes of one account get one number, counted in the order they come.
    let mut owners: Vec<AccountId> = Vec::new();
    let mut owner = |account: AccountId| {
        let known = owners.iter().position(|other| *other == account);
        known.unwrap_or_else(|| {
            owners.push(account);
            owners.len() - 1
        }) as u32
    };
    idle.iter()
        .filter(|candidate| may_take(&candidate.standing, manifest))
        .filter(|candidate| !barred.contains(&candidate.account))
        .map(|candidate| Eligible {
            node: candidate.node,
            owner: owner(candidate.account),
            weight: (candidate.standing.weight() * WEIGHT_SCALE).round() as u64,
            proven: candidate.standing.proven,
        })
        .collect()
}
