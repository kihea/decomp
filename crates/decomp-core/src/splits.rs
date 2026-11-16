//! Jobs cut into pieces (spec section 2, "Decomposition").
//!
//! Each piece is an ordinary job: it is assigned, checked, paid for, and given a
//! receipt on its own. What is kept here is only which pieces make up which job,
//! so that the job's submitter sees one status and one result. Like `Jobs`, none
//! of it is durable.

use std::collections::HashMap;

use crate::job::{JobStatus, Manifest, Outcome, Receipt};
use crate::jobs::{CLOSED_JOB_RETENTION_SECS, ClosedJob, Closure};
use crate::keys::{AccountId, ContentHash, JobId};
use crate::ledger::FeeSplit;

/// What became of one piece.
enum PieceEnd {
    Verified {
        units: u64,
        fee: FeeSplit,
        outcome: Outcome,
    },
    Disputed,
    Expired,
}

struct SplitJob {
    submitter: AccountId,
    manifest: Manifest,
    pieces: Vec<(JobId, Option<PieceEnd>)>,
    status: JobStatus,
    closed_at: Option<u64>,
}

/// Every piece of a split job completed. The caller joins their outputs, in this
/// order, stores the result, and answers with [`Splits::merged`].
pub struct Merge {
    pub job_id: JobId,
    pub outputs: Vec<ContentHash>,
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
            status: JobStatus::Running,
            closed_at: None,
        };
        self.jobs.insert(id, job);
    }

    /// Notes a closed job if it is a piece of a split job. Once the last piece is
    /// in, the split job closes too, unless every piece completed: then its
    /// outputs still have to be joined, and this returns what to join.
    pub fn piece_closed(&mut self, closed: &ClosedJob, now: u64) -> Option<Merge> {
        let end = match &closed.how {
            Closure::Verified { receipt, .. } => PieceEnd::Verified {
                units: receipt.units,
                fee: receipt.fee,
                outcome: receipt.outcome.clone(),
            },
            Closure::Disputed => PieceEnd::Disputed,
            Closure::Expired => PieceEnd::Expired,
            Closure::KnownAnswerPassed { .. } => return None,
        };
        let job_id = self.parent_of.remove(&closed.job_id)?;
        let job = self.jobs.get_mut(&job_id)?;
        let piece = job.pieces.iter_mut().find(|(id, _)| *id == closed.job_id)?;
        piece.1 = Some(end);

        let ends: Option<Vec<&PieceEnd>> = job.pieces.iter().map(|(_, end)| end.as_ref()).collect();
        let ends = ends?; // some pieces are still open
        let mut outputs = Vec::new();
        for end in &ends {
            let failed = match end {
                PieceEnd::Expired => Some(JobStatus::Expired),
                PieceEnd::Disputed => Some(JobStatus::Disputed),
                // A piece that failed the same way on every node fails the job.
                PieceEnd::Verified {
                    outcome: failure @ Outcome::Failed { .. },
                    ..
                } => Some(JobStatus::Verified {
                    receipt: Box::new(job.receipt(job_id, failure.clone())),
                }),
                PieceEnd::Verified {
                    outcome: Outcome::Completed { output, .. },
                    ..
                } => {
                    outputs.push(*output);
                    None
                }
            };
            if let Some(status) = failed {
                job.status = status;
                job.closed_at = Some(now);
                return None;
            }
        }
        Some(Merge { job_id, outputs })
    }

    /// Closes a split job whose pieces' outputs have been joined into `output`.
    pub fn merged(&mut self, id: JobId, output: ContentHash, now: u64) {
        if let Some(job) = self.jobs.get_mut(&id) {
            let receipt = Box::new(job.receipt(
                id,
                Outcome::Completed {
                    output,
                    blobs: Vec::new(),
                    state: None,
                    spawned: Vec::new(),
                },
            ));
            job.status = JobStatus::Verified { receipt };
            job.closed_at = Some(now);
        }
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
    /// The receipt for the whole job: the totals of its pieces' receipts.
    fn receipt(&self, id: JobId, outcome: Outcome) -> Receipt {
        let verified = || {
            self.pieces.iter().filter_map(|(_, end)| match end {
                Some(PieceEnd::Verified { units, fee, .. }) => Some((*units, *fee)),
                _ => None,
            })
        };
        Receipt {
            job_id: id,
            module: self.manifest.module,
            input: self.manifest.input,
            units: verified().map(|(units, _)| units).sum(),
            fee: verified().map(|(_, fee)| fee).sum(),
            outcome,
            state: None,
            handed: None,
            results: Vec::new(),
            pieces: self.pieces.iter().map(|(id, _)| *id).collect(),
        }
    }
}
