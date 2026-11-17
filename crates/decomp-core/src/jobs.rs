//! The coordinator's job rules: which nodes run a job, how their work is checked,
//! and when a result stands (spec section 2 "Lifecycle", section 3 "Assignment",
//! and section 4).
//!
//! Like `Accounts`, this does no I/O. The caller passes in the time, the source of
//! randomness, each node's standing, and the contents of each shard of state, so
//! the same calls always produce the same decisions. What the caller must act on comes back through three lists it
//! takes after each call: jobs that closed, score events, and disputes to referee.
//!
//! Nothing here is durable: assignments and results are needed only until a job
//! closes (spec section 6). What lasts is the receipt, which the caller stores.

use std::collections::{HashMap, HashSet, VecDeque};

use rand::{Rng, RngExt};

use crate::envelope::Envelope;
use crate::job::{
    Assignment, JobResult, JobStatus, Manifest, Outcome, Receipt, Spawn, UNPROVEN_REDUNDANCY,
    Verification,
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

/// The share of jobs given to one proven node that a second node is also asked
/// to run, without either being told. A starting value.
pub const SPOT_CHECK_RATE: f64 = 0.10;

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
    /// When the job was accepted, in seconds since 1970.
    submitted_at: u64,
    /// The contents its shard had when the job was handed out, which is what
    /// every node on it reads. `None` before then, and for an empty shard.
    shard: Option<ContentHash>,
    workers: Vec<Worker>,
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

    /// Accepts a job. The caller sets aside its highest possible fee first, so a
    /// job never runs without the credits to pay for it.
    pub fn submit(
        &mut self,
        id: JobId,
        submitter: AccountId,
        manifest: Manifest,
        now: u64,
    ) -> Result<(), JobError> {
        self.check(&id, &manifest, now)?;
        self.jobs
            .insert(id, Job::new(Kind::Submitted { submitter }, manifest, now));
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
        self.assign(now, rng, standing, contents);
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
            manifest: job.manifest.clone(),
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
    /// whoever checked in first.
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
    fn assign(
        &mut self,
        now: u64,
        rng: &mut impl Rng,
        standing: &dyn Fn(&PublicKey, &AccountId) -> Standing,
        contents: &dyn Fn(&ShardRef) -> Option<ContentHash>,
    ) {
        let Self {
            jobs,
            waiting,
            nodes,
            ..
        } = self;
        let mut idle: Vec<Candidate> = nodes
            .iter()
            .filter(|(_, node)| node.job.is_none() && is_online(node, now))
            .map(|(key, node)| Candidate {
                node: *key,
                account: node.account,
                standing: standing(key, &node.account),
            })
            .collect();

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
            let job = jobs.get_mut(id).expect("waiting jobs exist");
            let shares = job.shares();
            let barred = shares.and_then(|shares| holders.get(&shares));
            let barred = barred.cloned().unwrap_or_default();
            if job
                .manifest
                .state
                .as_ref()
                .is_some_and(|shard| held.contains(shard))
            {
                return true;
            }
            let Some(first) = draw(&idle, &job.manifest, &[], &barred, rng) else {
                return true;
            };
            // A proven node runs a job alone; a node on probation is always paired.
            let wanted = job
                .manifest
                .redundancy
                .unwrap_or(if idle[first].standing.proven {
                    1
                } else {
                    UNPROVEN_REDUNDANCY
                });
            let mut drawn = vec![first];
            while drawn.len() < wanted as usize {
                match draw(&idle, &job.manifest, &drawn, &barred, rng) {
                    Some(next) => drawn.push(next),
                    None => return true,
                }
            }
            let spot_check = job.manifest.redundancy.is_none() && wanted == 1;
            let checker = (spot_check && rng.random_bool(SPOT_CHECK_RATE))
                .then(|| draw(&idle, &job.manifest, &drawn, &barred, rng))
                .flatten();

            for &index in drawn.iter().chain(&checker) {
                let Candidate { node, account, .. } = idle[index];
                nodes.get_mut(&node).expect("idle nodes exist").job = Some(*id);
                if let Some(shares) = shares {
                    holders.entry(shares).or_default().insert(account);
                }
                job.workers.push(Worker {
                    node,
                    account,
                    checker: Some(index) == checker,
                });
            }
            let mut taken: Vec<usize> = drawn.into_iter().chain(checker).collect();
            // Remove from the back so earlier indexes stay valid.
            taken.sort_unstable_by(|a, b| b.cmp(a));
            for index in taken {
                idle.remove(index);
            }
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
        let usable: Vec<&Known> = self
            .known
            .iter()
            .filter(|known| may_take(standing, &known.manifest))
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
            ..
        } = self;
        for (id, job) in jobs.iter_mut() {
            if job.closed_at.is_some() || job.in_dispute || job.manifest.deadline >= now {
                continue;
            }
            job.status = JobStatus::Expired;
            job.closed_at = Some(now);
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
            submitted_at: now,
            shard: None,
            workers: Vec::new(),
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

/// Draws one candidate for a job, with chances in proportion to the candidates'
/// weights. Skips candidates already drawn and any from their accounts, and any
/// from the `barred` accounts.
fn draw(
    idle: &[Candidate],
    manifest: &Manifest,
    drawn: &[usize],
    barred: &HashSet<AccountId>,
    rng: &mut impl Rng,
) -> Option<usize> {
    let eligible: Vec<usize> = (0..idle.len())
        .filter(|&index| may_take(&idle[index].standing, manifest))
        .filter(|&index| !barred.contains(&idle[index].account))
        .filter(|&index| {
            drawn
                .iter()
                .all(|&other| idle[other].account != idle[index].account)
        })
        .collect();
    let total: f64 = eligible
        .iter()
        .map(|&index| idle[index].standing.weight())
        .sum();
    let mut point = rng.random::<f64>() * total;
    for &index in &eligible {
        point -= idle[index].standing.weight();
        if point <= 0.0 {
            return Some(index);
        }
    }
    // Rounding can leave `point` a hair above zero after the last candidate.
    eligible.last().copied()
}
