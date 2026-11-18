//! Running a leased job, committing to its result, and revealing it later; and
//! the pace a node keeps between requests.

use std::time::Duration;

use decomp_core::{ContentHash, JobResult, Lease, Outcome, Spawn};
use decomp_sandbox::{Blobs, Job, Sandbox};
use decomp_schema::records::{self, Records};

use crate::client::{Client, ClientError};

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

/// Runs one leased job, stores what it produced, and commits to its result.
/// If this fails, the lease lapses and the job board grants the job again.
pub fn work(client: &Client, sandbox: &Sandbox, lease: &Lease) -> Result<Committed, ClientError> {
    let manifest = &lease.manifest;
    let module = client.get_blob(manifest.module)?;
    let input = client.get_blob(manifest.input)?;
    let mut blobs = Blobs::new();
    for hash in &manifest.reads {
        blobs.insert(*hash, client.get_blob(*hash)?);
    }
    // A job handed a shard reads it as it was when the job was first leased. A
    // shard nothing has been written to is empty.
    let shard = match (&manifest.state, lease.shard) {
        (None, _) => None,
        (Some(_), Some(contents)) => Some(client.get_blob(contents)?),
        (Some(_), None) => Some(records::encode(&Records::new())),
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
    });

    let result = JobResult {
        job_id: lease.job_id,
        units: finding.units,
        outcome: match finding.produced {
            Ok(produced) => Outcome::Completed {
                output: client.put_blob(produced.output)?,
                blobs: produced
                    .blobs
                    .into_iter()
                    .map(|blob| client.put_blob(blob))
                    .collect::<Result<_, _>>()?,
                state: produced
                    .state
                    .map(|left| client.put_blob(left))
                    .transpose()?,
                spawned: produced
                    .spawned
                    .into_iter()
                    .map(|asked| {
                        Ok(Spawn {
                            module: asked.module,
                            input: client.put_blob(asked.input)?,
                            compute_cap: asked.compute_cap,
                        })
                    })
                    .collect::<Result<_, ClientError>>()?,
            },
            Err(failure) => Outcome::Failed { failure },
        },
        checkpoints: client.put_blob(finding.checkpoints)?,
    };
    let salt = ContentHash::from_bytes(rand::random());
    let commitment = result.commitment(&client.device_key(), &salt);
    client.commit_result(lease.job_id, commitment)?;
    Ok(Committed { result, salt })
}

/// Reveals a committed result, once the job board asks for it.
pub fn reveal(client: &Client, committed: &Committed) -> Result<(), ClientError> {
    client.reveal_result(committed.result.clone(), committed.salt)
}
