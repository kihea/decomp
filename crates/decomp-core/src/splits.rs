//! Jobs cut into pieces (spec section 2, "Decomposition").
//!
//! Each piece is an ordinary job: it is assigned, checked, paid for, and given a
//! receipt on its own. What is kept here is only which pieces make up which job,
//! so that the job's submitter sees one status and one result. Like `Jobs`, none
//! of it is durable.

use std::collections::HashMap;

use crate::job::{JobStatus, Manifest, Outcome, Receipt, Verification};
use crate::jobs::{CLOSED_JOB_RETENTION_SECS, ClosedJob, Closure};
use crate::keys::{AccountId, JobId};
use crate::ledger::FeeSplit;

/// What became of one piece.
enum PieceEnd {
    Verified {
        units: u64,
        fee: FeeSplit,
        outcome: Outcome,
        verified: Verification,
        runs: u32,
        seconds: u64,
    },
    Rejected,
    Expired,
}

struct SplitJob {
    submitter: AccountId,
    manifest: Manifest,
    pieces: Vec<(JobId, Option<PieceEnd>)>,
    status: JobStatus,
    closed_at: Option<u64>,
}

#[derive(Default)]
pub struct Splits {
    jobs: HashMap<JobId, SplitJob>,
    /// The split job each piece belongs to.
    parent_of: HashMap<JobId, JobId>,
}

impl Splits {
    /// Records a job that has been cut into `pieces`, each already submitted as a
    /// job of its own.
    pub fn add(&mut self, id: JobId, submitter: AccountId, manifest: Manifest, pieces: Vec<JobId>) {
        for piece in &pieces {
            self.parent_of.insert(*piece, id);
        }
        let job = SplitJob {
            submitter,
            manifest,
            pieces: pieces.into_iter().map(|piece| (piece, None)).collect(),
            status: JobStatus::Leased,
            closed_at: None,
        };
        self.jobs.insert(id, job);
    }

    /// Notes a closed job if it is a piece of a split job. Once the last piece is
    /// in, the split job closes too; if every piece completed, its outcome
    /// lists their outputs, for the submitter to join.
    pub fn piece_closed(&mut self, closed: &ClosedJob, now: u64) {
        let end = match &closed.how {
            Closure::Settled { receipt, .. } => PieceEnd::Verified {
                units: receipt.units,
                fee: receipt.fee,
                outcome: receipt.outcome.clone(),
                verified: receipt.verified,
                runs: receipt.runs,
                seconds: receipt.seconds,
            },
            Closure::Rejected => PieceEnd::Rejected,
            Closure::Expired => PieceEnd::Expired,
            Closure::FallBack(_)
            | Closure::CheckPassed { .. }
            | Closure::Release
            | Closure::AuditFailed { .. }
            | Closure::WrongInDispute { .. } => return,
        };
        let Some(job_id) = self.parent_of.remove(&closed.job_id) else {
            return;
        };
        let Some(job) = self.jobs.get_mut(&job_id) else {
            return;
        };
        let Some(piece) = job.pieces.iter_mut().find(|(id, _)| *id == closed.job_id) else {
            return;
        };
        piece.1 = Some(end);

        let ends: Option<Vec<&PieceEnd>> = job.pieces.iter().map(|(_, end)| end.as_ref()).collect();
        let Some(ends) = ends else {
            return; // some pieces are still open
        };
        let mut outputs = Vec::new();
        for end in &ends {
            let failed = match end {
                PieceEnd::Expired => Some(JobStatus::Expired),
                PieceEnd::Rejected => Some(JobStatus::Rejected),
                // A piece that failed the same way on every node fails the job.
                PieceEnd::Verified {
                    outcome: failure @ Outcome::Failed { .. },
                    ..
                } => Some(JobStatus::Settled {
                    receipt: Box::new(job.receipt(job_id, failure.clone())),
                }),
                PieceEnd::Verified {
                    outcome: Outcome::Completed { output, .. },
                    ..
                } => {
                    outputs.push(*output);
                    None
                }
                // A piece is never itself split.
                PieceEnd::Verified {
                    outcome: Outcome::Pieces { .. },
                    ..
                } => Some(JobStatus::Rejected),
            };
            if let Some(status) = failed {
                job.status = status;
                job.closed_at = Some(now);
                return;
            }
        }
        let receipt = Box::new(job.receipt(job_id, Outcome::Pieces { outputs }));
        job.status = JobStatus::Settled { receipt };
        job.closed_at = Some(now);
    }

    /// Where a split job stands, or `None` if `id` is not a split job submitted
    /// by `account`.
    pub fn status(&mut self, id: &JobId, account: &AccountId, now: u64) -> Option<JobStatus> {
        self.jobs.retain(|_, job| {
            job.closed_at
                .is_none_or(|at| now.saturating_sub(at) <= CLOSED_JOB_RETENTION_SECS)
        });
        let job = self.jobs.get(id).filter(|job| job.submitter == *account)?;
        Some(job.status.clone())
    }
}

impl SplitJob {
    /// The receipt for the whole job: the totals of its pieces' receipts. The
    /// pieces are submitted together, so the job took as long as its slowest
    /// piece. A job is given a receipt only once a piece of it is settled.
    fn receipt(&self, id: JobId, outcome: Outcome) -> Receipt {
        let pieces = || {
            self.pieces.iter().filter_map(|(_, end)| match end {
                Some(PieceEnd::Verified {
                    units,
                    fee,
                    verified,
                    runs,
                    seconds,
                    ..
                }) => Some((*units, *fee, *verified, *runs, *seconds)),
                _ => None,
            })
        };
        let weakest = pieces().map(|(_, _, verified, ..)| verified).min();
        Receipt {
            job_id: id,
            module: self.manifest.module,
            input: self.manifest.input,
            units: pieces().map(|(units, ..)| units).sum(),
            fee: pieces().map(|(_, fee, ..)| fee).sum(),
            outcome,
            state: None,
            handed: None,
            results: Vec::new(),
            pieces: self.pieces.iter().map(|(id, _)| *id).collect(),
            // A job that is split is never private.
            route: None,
            verified: weakest.unwrap_or(Verification::Unconfirmed),
            runs: pieces().map(|(_, _, _, runs, _)| runs).min().unwrap_or(0),
            seconds: pieces().map(|(.., seconds)| seconds).max().unwrap_or(0),
            enclave: false,
        }
    }
}
