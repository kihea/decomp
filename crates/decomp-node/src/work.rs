//! Running a leased job, committing to its result, and revealing it later; and
//! the pace a node keeps between requests.

use std::sync::Arc;
use std::time::Duration;

use decomp_core::{ContentHash, Failure, JobResult, Lease, Outcome, Transcript, merkle, privacy};
use decomp_sandbox::{Blobs, Job, Models, Sandbox};
use decomp_schema::records::{self, Records};

use crate::client::{Client, ClientError};
use crate::models::{CallContext, Generator, Replayer};
use crate::store;

/// How often a node asks for work. Must stay well under the job board's
/// window for counting a node as asking (`ONLINE_WINDOW_SECS`).
pub const IDLE_POLL: Duration = Duration::from_secs(2);

/// How long a node waits after a failure before trying again.
pub const RETRY_DELAY: Duration = Duration::from_secs(5);

/// A result this node has committed to and not yet revealed. The salt is
/// secret until the reveal: without it, nobody can match the commitment.
#[derive(Clone, Debug)]
pub struct Committed {
    pub result: JobResult,
    pub salt: ContentHash,
}

/// Why a leased job was not run to a result. The lease lapses and the job
/// board grants the job again.
#[derive(Debug, thiserror::Error)]
pub enum WorkError {
    #[error(transparent)]
    Client(#[from] ClientError),
    /// A blob the job reads, or one it produced, could not be moved.
    #[error("{0}")]
    Data(String),
}

/// Runs one leased job, stores what it produced, and commits to its result.
/// The job's data comes from storage nodes, and what it produces goes to
/// them; the control plane sees only hashes.
pub fn work(client: &Client, sandbox: &Sandbox, lease: &Lease) -> Result<Committed, WorkError> {
    let manifest = &lease.manifest;
    let fetch = |root: ContentHash| store::fetch(client, root).map_err(WorkError::Data);
    let module = fetch(manifest.module)?;
    let input = fetch(manifest.input)?;
    let mut blobs = Blobs::new();
    for hash in &manifest.reads {
        blobs.insert(*hash, fetch(*hash)?);
    }
    // A job handed a shard reads it as it was when the job was first leased. A
    // shard nothing has been written to is empty.
    let shard = match (&manifest.state, lease.shard) {
        (None, _) => None,
        (Some(_), Some(contents)) => Some(fetch(contents)?),
        (Some(_), None) => Some(records::encode(&Records::new())),
    };
    // The control plane holds no module, so it is here that a job is checked
    // against what its module says of itself. Every honest node finds the same.
    let declared = sandbox.declared(&module);
    let fits = privacy::check(manifest, declared.as_ref()).is_ok()
        && declared
            .as_ref()
            .is_none_or(|signature| privacy::check_input(manifest, signature, &input).is_ok());
    if !fits {
        let result = JobResult {
            job_id: lease.job_id,
            units: 0,
            outcome: Outcome::Failed {
                failure: Failure::InvalidJob,
            },
            checkpoints: ContentHash::of(&[]),
            transcript: lease.transcript,
        };
        return commit(client, lease, result);
    }
    // A job that calls models: the first node generates and keeps a
    // transcript; a node after it replays the first node's transcript.
    let role = match lease.transcript {
        _ if manifest.models.is_empty() => Role::NoModels,
        None => Role::Generate(Arc::new(Generator::new(CallContext::new(client, lease)))),
        Some(root) => match serde_json::from_slice::<Transcript>(&fetch(root)?) {
            Ok(transcript) => Role::Replay(Arc::new(Replayer::new(
                transcript,
                CallContext::new(client, lease),
            ))),
            Err(_) => {
                let result = JobResult {
                    job_id: lease.job_id,
                    units: 0,
                    outcome: Outcome::Failed {
                        failure: Failure::ProofRejected,
                    },
                    checkpoints: ContentHash::of(&[]),
                    transcript: Some(root),
                };
                return commit(client, lease, result);
            }
        },
    };
    let model_host: Option<Arc<dyn Models>> = match &role {
        Role::NoModels => None,
        Role::Generate(generator) => Some(Arc::clone(generator) as Arc<dyn Models>),
        Role::Replay(replayer) => Some(Arc::clone(replayer) as Arc<dyn Models>),
    };
    let finding = sandbox.find(&Job {
        module: &module,
        input: &input,
        compute_cap: manifest.compute_cap,
        seed: *lease.seed.as_bytes(),
        blobs: &blobs,
        state: shard.as_deref(),
        // A node opens nothing: it holds no key of the user whose job this is.
        opening_key: None,
        models: &manifest.models,
        model_host,
    });
    // A model call this node could not answer, for a reason of its own, is no
    // result: the lease lapses and the job is granted again.
    if let Some(reason) = &finding.unavailable {
        return Err(WorkError::Data(reason.clone()));
    }
    let mut result = finding.result(lease.job_id);
    let mut transcript_bytes = None;
    match &role {
        Role::NoModels => {}
        Role::Generate(generator) => {
            let bytes =
                serde_json::to_vec(&generator.transcript()).expect("a transcript serializes");
            result.transcript = Some(merkle::root(&bytes));
            transcript_bytes = Some(bytes);
        }
        Role::Replay(replayer) => {
            result.transcript = lease.transcript;
            if !replayer.used_all()
                && result.outcome
                    != (Outcome::Failed {
                        failure: Failure::ProofRejected,
                    })
            {
                result.outcome = Outcome::Failed {
                    failure: Failure::ProofRejected,
                };
            }
        }
    }

    // What the job produced goes to storage nodes, paid by the job's payer,
    // before the result is committed to. A node that ran the job second
    // finds it already stored.
    let produced = finding
        .produced_blobs()
        .into_iter()
        .chain(transcript_bytes.as_deref());
    for bytes in produced {
        let root = merkle::root(bytes);
        let placement = client.store_produced(lease.job_id, root, bytes.len() as u64)?;
        if !placement.addresses.is_empty() && store::upload_to(&placement, &root, bytes) == 0 {
            return Err(WorkError::Data(format!(
                "no node chosen to hold {root} took it"
            )));
        }
    }
    commit(client, lease, result)
}

/// How a run answers the job's model calls.
enum Role {
    NoModels,
    Generate(Arc<Generator>),
    Replay(Arc<Replayer>),
}

/// Commits to a result, keeping the salt to reveal it with later.
fn commit(client: &Client, lease: &Lease, result: JobResult) -> Result<Committed, WorkError> {
    let salt = ContentHash::from_bytes(rand::random());
    let commitment = result.commitment(&client.device_key(), &salt);
    client.commit_result(lease.job_id, commitment, result.transcript)?;
    Ok(Committed { result, salt })
}

/// Reveals a committed result, once the job board asks for it.
pub fn reveal(client: &Client, committed: &Committed) -> Result<(), ClientError> {
    client.reveal_result(committed.result.clone(), committed.salt)
}
