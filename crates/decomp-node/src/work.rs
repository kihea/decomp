//! Running an assigned job, and the pace a node keeps between jobs.

use std::time::Duration;

use decomp_core::{Assignment, JobResult, Outcome};
use decomp_sandbox::{Job, Sandbox};

use crate::client::{Client, ClientError};

/// How often an idle node checks in. Must stay well under the coordinator's
/// online window (`ONLINE_WINDOW_SECS`), or an idle node would look offline.
pub const IDLE_POLL: Duration = Duration::from_secs(2);

/// How long a node waits after a failure before trying again.
pub const RETRY_DELAY: Duration = Duration::from_secs(5);

/// Runs one assigned job and reports what it produced. If this fails, the
/// coordinator hands the same job back at the node's next check-in.
pub fn work(
    client: &Client,
    sandbox: &Sandbox,
    assignment: &Assignment,
) -> Result<JobResult, ClientError> {
    let manifest = &assignment.manifest;
    let module = client.get_blob(manifest.module)?;
    let input = client.get_blob(manifest.input)?;
    let finding = sandbox.find(&Job {
        module: &module,
        input: &input,
        compute_cap: manifest.compute_cap,
        seed: *assignment.seed.as_bytes(),
    });

    let result = JobResult {
        job_id: assignment.job_id,
        units: finding.units,
        outcome: match finding.output {
            Ok(output) => Outcome::Completed {
                output: client.put_blob(output)?,
            },
            Err(failure) => Outcome::Failed { failure },
        },
        checkpoints: client.put_blob(finding.checkpoints)?,
    };
    client.submit_result(result.clone())?;
    Ok(result)
}
