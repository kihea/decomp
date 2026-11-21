/**
 * Submitting a job to the network and waiting for its result, as a device does
 * it (the command-line node's `submit` does the same in Rust).
 */

import { concat } from './bytes';
import { type Failure, type JobStatus, type Manifest, now, type Quote, type Submitted } from './protocol';
import { getBlob, JOB_DATA_DAYS, putBlob, type Storage } from './storage';

/** How often a submitter asks whether its job is done. */
const JOB_POLL_MS = 500;

/** A job that ended without an output. */
export class JobError extends Error {}

/** A job to run, with the limits its submitter sets. */
export type JobOrder = {
  /** The module's root; the module must already be stored (see `putBlob`). */
  module: string;
  input: Uint8Array;
  /** Compute units the job may use before it is stopped. */
  computeCap: number;
  /** How long the network has to finish it. */
  deadlineSecs: number;
};

export type JobResult = {
  output: Uint8Array;
  /** What the job cost its submitter, in millionths of a credit. */
  fee: number;
};

/**
 * Submits a job, billed to the signer's account, and waits for its settled
 * result. Its input goes to storage nodes first, and its output comes from
 * them. The most it can cost is set aside when it is submitted, and what it
 * does not use comes back.
 */
export async function runJob(storage: Storage, order: JobOrder): Promise<JobResult> {
  const { controlPlane, signer } = storage;
  const manifest: Manifest = {
    module: order.module,
    input: await putBlob(storage, order.input, JOB_DATA_DAYS),
    compute_cap: order.computeCap,
    deadline: now() + order.deadlineSecs,
    max_fee: 0,
  };
  // Set aside the most the job can cost at the rate posted now.
  const quote = await controlPlane.request<Quote>(signer, { type: 'quote_job', manifest });
  manifest.max_fee = quote.highest_fee;
  const { job_id } = await controlPlane.request<Submitted>(signer, { type: 'submit_job', manifest });
  for (;;) {
    const status = await controlPlane.request<JobStatus>(signer, { type: 'get_receipt', job_id });
    switch (status.state) {
      case 'submitted':
      case 'leased':
      case 'committed':
      case 'disputed':
        await new Promise((resolve) => setTimeout(resolve, JOB_POLL_MS));
        continue;
      case 'rejected':
        throw new JobError('The nodes disagreed and none was right.');
      case 'expired':
        throw new JobError('The deadline passed before the job had a result.');
      case 'settled': {
        const { outcome, fee } = status.receipt;
        if (outcome.status === 'failed') throw new JobError(FAILURES[outcome.failure]);
        // A split job's output is its pieces' outputs, joined here.
        const roots = outcome.status === 'pieces' ? outcome.outputs : [outcome.output];
        const parts = await Promise.all(roots.map((root) => getBlob(storage, root)));
        return { output: concat(...parts), fee: fee.total };
      }
    }
  }
}

const FAILURES: Record<Failure, string> = {
  compute_cap_exceeded: 'The job used its whole compute cap.',
  trapped: 'The job trapped.',
  invalid_job: 'The module or its input cannot be run.',
};
