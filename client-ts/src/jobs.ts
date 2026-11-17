/**
 * Submitting a job to the network and waiting for its result, as a device does
 * it (the command-line node's `submit` does the same in Rust).
 */

import { fromBase64, toBase64 } from './bytes';
import type { Coordinator } from './client';
import {
  type BlobData,
  type BlobRef,
  type Failure,
  highestFee,
  type JobStatus,
  type Manifest,
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

/** Stores bytes with the coordinator; resolves with their hash. */
export async function putBlob(
  coordinator: Coordinator,
  signer: Signer,
  data: Uint8Array,
): Promise<string> {
  const stored = await coordinator.request<BlobRef>(signer, {
    type: 'put_blob',
    data: toBase64(data),
  });
  return stored.hash;
}

export async function getBlob(
  coordinator: Coordinator,
  signer: Signer,
  hash: string,
): Promise<Uint8Array> {
  const blob = await coordinator.request<BlobData>(signer, { type: 'get_blob', hash });
  return fromBase64(blob.data);
}

/**
 * Submits a job, billed to the signer's account, and waits for a verified
 * result. The most it can cost is set aside when it is submitted, and what it
 * does not use comes back.
 */
export async function runJob(
  coordinator: Coordinator,
  signer: Signer,
  order: JobOrder,
): Promise<JobResult> {
  const manifest: Manifest = {
    module: order.module,
    input: await putBlob(coordinator, signer, order.input),
    compute_cap: order.computeCap,
    deadline: now() + order.deadlineSecs,
    max_fee: highestFee(order.computeCap),
  };
  const { job_id } = await coordinator.request<Submitted>(signer, { type: 'submit_job', manifest });
  for (;;) {
    const status = await coordinator.request<JobStatus>(signer, { type: 'get_job', job_id });
    switch (status.state) {
      case 'waiting':
      case 'running':
        await new Promise((resolve) => setTimeout(resolve, JOB_POLL_MS));
        continue;
      case 'disputed':
        throw new JobError('The nodes disagreed and none was right.');
      case 'expired':
        throw new JobError('The deadline passed before the job had a result.');
      case 'verified': {
        const { outcome, fee } = status.receipt;
        if (outcome.status === 'failed') throw new JobError(FAILURES[outcome.failure]);
        return { output: await getBlob(coordinator, signer, outcome.output), fee: fee.total };
      }
    }
  }
}

const FAILURES: Record<Failure, string> = {
  compute_cap_exceeded: 'The job used its whole compute cap.',
  trapped: 'The job trapped.',
  invalid_job: 'The module or its input cannot be run.',
};
