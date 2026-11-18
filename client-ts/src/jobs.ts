/**
 * Submitting a job to the network and waiting for its result, as a device does
 * it (the command-line node's `submit` does the same in Rust).
 */

import { fromBase64, toBase64 } from './bytes';
import type { ControlPlane } from './client';
import {
  type BlobData,
  type BlobRef,
  type Failure,
  type JobStatus,
  type Manifest,
  type Quote,
  now,
  type Signer,
  type Submitted,
} from './protocol';

/** How often a submitter asks whether its job is done. */
const JOB_POLL_MS = 500;

/** A job that ended without an output. */
export class JobError extends Error {}

/** A job to run, with the limits its submitter sets. */
export type JobOrder = {
  /** The module's hash; the module must already be stored (see `putBlob`). */
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

/** Stores bytes with the network; resolves with their hash. */
export async function putBlob(
  controlPlane: ControlPlane,
  signer: Signer,
  data: Uint8Array,
): Promise<string> {
  const stored = await controlPlane.request<BlobRef>(signer, {
    type: 'put_blob',
    data: toBase64(data),
  });
  return stored.hash;
}

export async function getBlob(
  controlPlane: ControlPlane,
  signer: Signer,
  hash: string,
): Promise<Uint8Array> {
  const blob = await controlPlane.request<BlobData>(signer, { type: 'get_blob', hash });
  return fromBase64(blob.data);
}

/**
 * Submits a job, billed to the signer's account, and waits for its settled
 * result. The most it can cost is set aside when it is submitted, and what it
 * does not use comes back.
 */
export async function runJob(
  controlPlane: ControlPlane,
  signer: Signer,
  order: JobOrder,
): Promise<JobResult> {
  const manifest: Manifest = {
    module: order.module,
    input: await putBlob(controlPlane, signer, order.input),
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
        return { output: await getBlob(controlPlane, signer, outcome.output), fee: fee.total };
      }
    }
  }
}

const FAILURES: Record<Failure, string> = {
  compute_cap_exceeded: 'The job used its whole compute cap.',
  trapped: 'The job trapped.',
  invalid_job: 'The module or its input cannot be run.',
};
