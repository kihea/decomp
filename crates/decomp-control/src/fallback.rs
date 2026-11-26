//! Running a job as an always-on fallback (spec, "Fallback without a server"):
//! if no node finishes a job whose manifest asks for it, the control plane runs
//! the same module itself. A job that may fall back leaves its module, input,
//! and blobs to read with the control plane when it is submitted, and what the
//! run produces is kept here too.

use decomp_core::{JobId, JobResult, PublicKey};
use decomp_sandbox::{Blobs, Job, Sandbox};

/// A job to run as a fallback, with everything it reads.
pub struct FallBackRun {
    /// What the job's randomness is seeded from: the job's ID.
    pub seed: JobId,
    pub module: Vec<u8>,
    pub input: Vec<u8>,
    /// The blobs the job may read.
    pub blobs: Blobs,
    /// The records of the shard the job was handed, if it was handed one.
    pub shard: Option<Vec<u8>>,
    pub compute_cap: u64,
    /// The key the settlement log is signed with, which device proofs rest on.
    pub control_key: PublicKey,
}

/// Runs a job whole and returns its result and every blob it produced.
pub fn run(sandbox: &Sandbox, run: &FallBackRun, job_id: JobId) -> (JobResult, Vec<Vec<u8>>) {
    let finding = sandbox.find(&Job {
        module: &run.module,
        input: &run.input,
        compute_cap: run.compute_cap,
        seed: *run.seed.as_bytes(),
        blobs: &run.blobs,
        state: run.shard.as_deref(),
        opening_key: None,
        // Inference jobs cannot fall back, so no fallback calls a model.
        models: &[],
        model_host: None,
        control_key: Some(run.control_key),
    });
    let produced = finding
        .produced_blobs()
        .into_iter()
        .map(<[u8]>::to_vec)
        .collect();
    (finding.result(job_id), produced)
}
