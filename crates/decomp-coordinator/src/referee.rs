//! Works out which nodes were right when they disagree about a job (spec section
//! 4, "Checkpoint disputes").
//!
//! The nodes' checkpoint lists show where their runs first parted. The referee
//! runs the job itself only as far as that checkpoint: a node whose checkpoint
//! matches the referee's is right, so one honest node is enough and no majority
//! is needed. The whole job is run only when checkpoints cannot tell the nodes
//! apart.
//!
//! The spec has the referee re-run just the one segment between two checkpoints.
//! That needs a way to restore a job's state mid-run, which a compiled engine does
//! not offer, so the job is re-run from its start up to the checkpoint instead.

use decomp_core::{ContentHash, JobId, JobResult, Outcome, PublicKey};
use decomp_sandbox::{
    Blobs, Checkpoint, INSTRUCTIONS_PER_CHECKPOINT, INSTRUCTIONS_PER_UNIT, Job, Sandbox,
};

/// A dispute with everything needed to decide it.
pub struct Case {
    pub job_id: JobId,
    pub module: Vec<u8>,
    pub input: Vec<u8>,
    /// The blobs the job may read.
    pub blobs: Blobs,
    /// The records of the shard the job was handed, if it was handed one.
    pub shard: Option<Vec<u8>>,
    pub compute_cap: u64,
    pub reports: Vec<(PublicKey, JobResult)>,
    /// Each node's checkpoints, in the order of `reports`.
    pub checkpoint_lists: Vec<Vec<u8>>,
}

pub struct Verdict {
    /// The nodes whose result stands. Empty if none was right.
    pub right: Vec<PublicKey>,
    /// The nodes that reported something else.
    pub wrong: Vec<PublicKey>,
    /// Compute units the referee spent deciding.
    pub units: u64,
}

pub fn referee(sandbox: &Sandbox, case: &Case) -> Verdict {
    let lists: Vec<Vec<&[u8]>> = case
        .checkpoint_lists
        .iter()
        .map(|list| list.chunks(size_of::<Checkpoint>()).collect())
        .collect();
    let job = Job {
        module: &case.module,
        input: &case.input,
        compute_cap: case.compute_cap,
        seed: *case.job_id.as_bytes(),
        blobs: &case.blobs,
        state: case.shard.as_deref(),
    };
    let mut standing: Vec<usize> = (0..case.reports.len()).collect();
    let mut units = 0;

    // Find the first checkpoint the nodes do not all share, run the job that far,
    // and keep the nodes that have it right.
    let longest = lists.iter().map(Vec::len).max().unwrap_or(0);
    let parted_at = (0..longest).find(|&index| {
        let first = lists[0].get(index);
        lists.iter().any(|list| list.get(index) != first)
    });
    if let Some(index) = parted_at {
        let truth = sandbox.checkpoint(&job, index).ok().flatten();
        standing.retain(|&node| lists[node].get(index).copied() == truth.as_ref().map(|t| &t[..]));
        let instructions = (index as u64 + 1) * INSTRUCTIONS_PER_CHECKPOINT;
        units += instructions
            .div_ceil(INSTRUCTIONS_PER_UNIT)
            .min(case.compute_cap);
    }

    // Nodes that agree on every checkpoint can still report different results.
    // Only running the whole job can settle that.
    let result_of = |node: usize| &case.reports[node].1;
    let disagree = standing
        .iter()
        .any(|&node| result_of(node) != result_of(standing[0]));
    if disagree {
        let finding = sandbox.find(&job);
        let outcome = match &finding.produced {
            Ok(produced) => Outcome::Completed {
                output: ContentHash::of(&produced.output),
                blobs: produced.blobs.iter().map(|b| ContentHash::of(b)).collect(),
                state: produced.state.as_ref().map(|left| ContentHash::of(left)),
            },
            Err(failure) => Outcome::Failed { failure: *failure },
        };
        let checkpoints = ContentHash::of(&finding.checkpoints);
        standing.retain(|&node| {
            let reported = result_of(node);
            reported.units == finding.units
                && reported.outcome == outcome
                && reported.checkpoints == checkpoints
        });
        units += finding.units;
    }

    let node_key = |node: usize| case.reports[node].0;
    Verdict {
        right: standing.iter().map(|&node| node_key(node)).collect(),
        wrong: (0..case.reports.len())
            .filter(|node| !standing.contains(node))
            .map(node_key)
            .collect(),
        units,
    }
}
