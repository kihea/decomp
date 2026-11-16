//! Everything the coordinator knows, and where each part is kept.
//!
//! | Part | Kept in | Survives a restart |
//! | --- | --- | --- |
//! | Account records | `ops.jsonl`, replayed at startup | Yes |
//! | Balances | `ops.jsonl` (starter grants) and `ledger.jsonl`, replayed at startup | Yes |
//! | Node scores | `scores.jsonl`, replayed at startup | Yes |
//! | Blobs | `blobs/`, one file per hash | Yes |
//! | Receipts | `receipts.jsonl` | Yes |
//! | Open jobs and online nodes | Memory | No, by design (spec section 6) |
//!
//! The log files are the "local signed log" that stands in for the ledger during
//! development (spec section 6).

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;

use decomp_core::{
    Account, AccountId, AccountStatus, Accounts, Assignment, Closure, ContentHash, Credits,
    Envelope, JobError, JobId, JobStatus, Jobs, Ledger, LedgerEntry, LedgerError, MAX_PIECES,
    Manifest, Merge, OpError, PROBATION_FORFEIT_PERCENT, Penalty, PublicKey, ScoreEvent, Scores,
    Splits, Standing, fee,
};
use decomp_sandbox::Sandbox;
use decomp_schema::{Mismatch, Signature};
use serde::de::DeserializeOwned;

use crate::blobs::Blobs;
use crate::log::Log;
use crate::referee::{Case, Verdict};

/// Why a job was not accepted.
#[derive(Debug, thiserror::Error)]
pub enum SubmitError {
    #[error(transparent)]
    Job(#[from] JobError),
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error(transparent)]
    Storage(#[from] io::Error),
    #[error("no blob with hash {0} has been stored")]
    UnknownBlob(ContentHash),
    #[error("max_fee must cover the highest possible fee, {0} credits")]
    FeeCapTooLow(Credits),
    #[error("the split rule would cut the job into more than {MAX_PIECES} pieces")]
    TooManyPieces,
    #[error("{0}")]
    TypeMismatch(Mismatch),
    #[error("a job with typed input cannot be split by byte count")]
    TypedJobSplitByBytes,
}

pub struct Coordinator {
    pub accounts: Accounts,
    ops: Log,
    ledger: Ledger,
    ledger_log: Log,
    scores: Scores,
    scores_log: Log,
    pub blobs: Blobs,
    pub jobs: Jobs,
    splits: Splits,
    /// What each module says its job takes and returns, read once per module.
    /// `None` for a module that does not describe itself.
    signatures: HashMap<ContentHash, Option<Signature>>,
    receipts: Log,
}

impl Coordinator {
    pub fn open(data_dir: &Path) -> io::Result<Self> {
        fs::create_dir_all(data_dir)?;

        let mut accounts = Accounts::default();
        let mut ledger = Ledger::default();
        let ops = replay(&data_dir.join("ops.jsonl"), |envelope: Envelope| {
            let account = accounts.apply(&envelope).map_err(|e| e.to_string())?;
            ledger.open(account.id);
            Ok(())
        })?;
        let mut ledger_log = replay(&data_dir.join("ledger.jsonl"), |entry: LedgerEntry| {
            ledger.apply(&entry).map_err(|e| e.to_string())
        })?;
        let mut scores = Scores::default();
        let scores_log = replay(&data_dir.join("scores.jsonl"), |event: ScoreEvent| {
            scores.apply(&event);
            Ok(())
        })?;

        // Open jobs do not survive a restart, so hand back what was set aside for
        // any job that was still open when the coordinator last stopped.
        for job_id in ledger.escrowed_jobs() {
            let refund = LedgerEntry::Refund { job_id };
            ledger
                .apply(&refund)
                .expect("the job has credits set aside");
            ledger_log.append(&refund);
        }

        Ok(Self {
            accounts,
            ops,
            ledger,
            ledger_log,
            scores,
            scores_log,
            blobs: Blobs::open(data_dir.join("blobs"))?,
            jobs: Jobs::default(),
            splits: Splits::default(),
            signatures: HashMap::new(),
            receipts: Log::open(&data_dir.join("receipts.jsonl"))?,
        })
    }

    /// Applies a change to an account and records it. A new account gets its
    /// starter grant.
    pub fn apply_op(&mut self, envelope: &Envelope) -> Result<AccountStatus, OpError> {
        let account = self.accounts.apply(envelope)?.clone();
        self.ledger.open(account.id);
        self.ops.append(envelope);
        Ok(self.status(account))
    }

    pub fn status(&self, account: Account) -> AccountStatus {
        let balance = self.ledger.balance(&account.id);
        AccountStatus { account, balance }
    }

    /// How far a node is trusted, given its scores and its account's stake.
    pub fn standing(&self, node: &PublicKey, account: &AccountId) -> Standing {
        self.scores
            .standing(node, self.ledger.balance(account).staked)
    }

    /// Records that a node is online and returns the job it should be running.
    pub fn check_in(
        &mut self,
        node: PublicKey,
        account: AccountId,
        now: u64,
    ) -> Option<Assignment> {
        let Self {
            jobs,
            scores,
            ledger,
            ..
        } = self;
        let standing = |node: &PublicKey, account: &AccountId| {
            scores.standing(node, ledger.balance(account).staked)
        };
        jobs.check_in(node, account, now, &mut rand::rng(), &standing)
    }

    /// Accepts a job: checks it, sets its highest possible fee aside, and queues
    /// it. A job with a split rule is cut into pieces here, each queued as a job
    /// of its own on its own part of the input.
    pub fn submit_job(
        &mut self,
        signer: &PublicKey,
        account: AccountId,
        payload: &str,
        manifest: Manifest,
        now: u64,
        sandbox: &Sandbox,
    ) -> Result<JobId, SubmitError> {
        for hash in [manifest.module, manifest.input] {
            if !self.blobs.contains(&hash) {
                return Err(SubmitError::UnknownBlob(hash));
            }
        }
        self.check_types(&manifest, sandbox)?;
        let job_id = JobId::of(signer, payload);

        // The jobs that will actually run: this one, or its pieces.
        let mut runs = vec![(job_id, manifest.clone())];
        if let Some(split) = manifest.split {
            let input = self.blobs.get(&manifest.input)?.unwrap_or_default();
            if split.pieces(input.len() as u64) > MAX_PIECES {
                return Err(SubmitError::TooManyPieces);
            }
            let mut chunks: Vec<&[u8]> = input.chunks(split.chunk_bytes.max(1) as usize).collect();
            if chunks.is_empty() {
                chunks.push(&[]);
            }
            runs.clear();
            for (index, chunk) in chunks.into_iter().enumerate() {
                let piece = Manifest {
                    input: self.blobs.put(chunk)?,
                    split: None,
                    max_fee: manifest.highest_fee(),
                    ..manifest.clone()
                };
                runs.push((
                    JobId::of(signer, &format!("{payload} piece {index}")),
                    piece,
                ));
            }
        }

        let total = |amount: fn(&Manifest) -> Credits| {
            runs.iter()
                .fold(Credits::ZERO, |sum, (_, run)| sum + amount(run))
        };
        let highest = total(Manifest::highest_fee);
        if manifest.max_fee < highest {
            return Err(SubmitError::FeeCapTooLow(highest));
        }
        for (id, run) in &runs {
            self.jobs.check(id, run, now)?;
        }
        let to_set_aside = total(|run| run.max_fee);
        if self.ledger.balance(&account).spendable() < to_set_aside {
            return Err(LedgerError::InsufficientCredits.into());
        }

        for (id, run) in &runs {
            self.record(LedgerEntry::Escrow {
                job_id: *id,
                account,
                amount: run.max_fee,
            })?;
            self.jobs.submit(*id, account, run.clone(), now)?;
        }
        if manifest.split.is_some() {
            let pieces = runs.iter().map(|(id, _)| *id).collect();
            self.splits.add(job_id, account, manifest, pieces);
        }
        Ok(job_id)
    }

    /// Refuses an input that does not fit the types the module says its job takes
    /// (spec section 7, "Typing"). A module that does not describe itself takes
    /// any input.
    fn check_types(&mut self, manifest: &Manifest, sandbox: &Sandbox) -> Result<(), SubmitError> {
        if !self.signatures.contains_key(&manifest.module) {
            let module = self.blobs.get(&manifest.module)?.unwrap_or_default();
            let described = sandbox.signature(&module).ok().flatten();
            let signature = described.and_then(|bytes| serde_json::from_slice(&bytes).ok());
            self.signatures.insert(manifest.module, signature);
        }
        let Some(signature) = &self.signatures[&manifest.module] else {
            return Ok(());
        };
        // Cutting typed input at byte boundaries would hand each piece broken data.
        if manifest.split.is_some() {
            return Err(SubmitError::TypedJobSplitByBytes);
        }
        let input = self.blobs.get(&manifest.input)?.unwrap_or_default();
        signature
            .check_input(&input)
            .map_err(SubmitError::TypeMismatch)
    }

    /// Where a job stands, for the account that submitted it.
    pub fn job_status(
        &mut self,
        job_id: JobId,
        account: AccountId,
        now: u64,
    ) -> Result<JobStatus, JobError> {
        match self.splits.status(&job_id, &account, now) {
            Some(status) => Ok(status),
            None => self.jobs.status(job_id, account, now),
        }
    }

    /// Joins the outputs of a split job whose pieces have all completed.
    fn merge(&mut self, merge: Merge, now: u64) {
        let mut output = Vec::new();
        for hash in &merge.outputs {
            let piece = self.blobs.get(hash).expect("blobs can be read back");
            output.extend(piece.expect("a verified output was stored"));
        }
        let hash = self.blobs.put(&output).expect("blobs can be stored");
        self.splits.merged(merge.job_id, hash, now);
    }

    /// Moves credits and records the move.
    pub fn record(&mut self, entry: LedgerEntry) -> Result<(), LedgerError> {
        self.ledger.apply(&entry)?;
        self.ledger_log.append(&entry);
        Ok(())
    }

    /// Acts on what the job rules decided: updates node scores, pays for jobs
    /// whose result stood, and returns the credits set aside for jobs that have
    /// none. Call after anything that can change a job.
    pub fn settle(&mut self, now: u64) {
        for event in self.jobs.take_events() {
            let penalty = self.scores.apply(&event);
            self.scores_log.append(&event);
            if penalty == Penalty::ReturnedToProbation {
                let lost = |staked: Credits| staked.percent(PROBATION_FORFEIT_PERCENT);
                self.forfeit(&event.node, lost);
            }
        }
        for closed in self.jobs.take_closed() {
            let job_id = closed.job_id;
            if let Some(merge) = self.splits.piece_closed(&closed, now) {
                self.merge(merge, now);
            }
            // Every accepted job has its highest possible fee set aside, results
            // above the compute cap are refused, and the other entries here cannot
            // fail for an account that exists.
            let entries = match closed.how {
                Closure::Verified {
                    receipt,
                    node_accounts,
                    checker,
                } => {
                    self.receipts.append(&receipt);
                    let amount = receipt.fee.per_node;
                    let settle = LedgerEntry::Settle {
                        job_id,
                        split: receipt.fee,
                        node_accounts,
                    };
                    let pay = checker.map(|account| LedgerEntry::PayChecker { account, amount });
                    vec![Some(settle), pay]
                }
                Closure::Disputed | Closure::Expired => {
                    vec![Some(LedgerEntry::Refund { job_id })]
                }
                Closure::KnownAnswerPassed { account, reward } => {
                    vec![Some(LedgerEntry::PayChecker {
                        account,
                        amount: reward,
                    })]
                }
            };
            for entry in entries.into_iter().flatten() {
                self.record(entry).expect("a closed job can be settled");
            }
        }
    }

    /// The disputes that have arisen, each with the blobs needed to decide it.
    pub fn take_cases(&mut self) -> Vec<Case> {
        let disputes = self.jobs.take_disputes();
        let blob = |hash: &ContentHash| -> Vec<u8> {
            self.blobs.get(hash).ok().flatten().unwrap_or_default()
        };
        disputes
            .into_iter()
            .map(|dispute| Case {
                job_id: dispute.job_id,
                module: blob(&dispute.manifest.module),
                input: blob(&dispute.manifest.input),
                compute_cap: dispute.manifest.compute_cap,
                checkpoint_lists: dispute
                    .reports
                    .iter()
                    .map(|r| blob(&r.1.checkpoints))
                    .collect(),
                reports: dispute.reports,
            })
            .collect()
    }

    /// Closes a dispute. Nodes that were wrong pay what refereeing cost, out of
    /// their accounts' stake.
    pub fn resolve(&mut self, case: &Case, verdict: &Verdict, now: u64) {
        self.jobs.resolve(case.job_id, &verdict.right, now);
        for node in &verdict.wrong {
            self.forfeit(node, |_| fee(verdict.units, 1));
        }
        self.settle(now);
    }

    /// Takes stake from the account a node belongs to.
    fn forfeit(&mut self, node: &PublicKey, amount: impl Fn(Credits) -> Credits) {
        let Some(account) = self.accounts.account_of(node).map(|account| account.id) else {
            return;
        };
        let amount = amount(self.ledger.balance(&account).staked);
        self.record(LedgerEntry::Forfeit { account, amount })
            .expect("a forfeit takes only what is staked");
    }
}

/// Opens a log and applies every entry already in it.
fn replay<T: DeserializeOwned>(
    path: &Path,
    mut apply: impl FnMut(T) -> Result<(), String>,
) -> io::Result<Log> {
    let log = Log::open(path)?;
    for (index, line) in log.lines().enumerate() {
        let applied = serde_json::from_str(&line?)
            .map_err(|e| e.to_string())
            .and_then(&mut apply);
        applied.map_err(|reason| {
            let message = format!("{}, line {}: {reason}", path.display(), index + 1);
            io::Error::new(io::ErrorKind::InvalidData, message)
        })?;
    }
    Ok(log)
}
