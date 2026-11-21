/**
 * Storing blobs on storage nodes and fetching them back (spec, "Blobs"). The
 * control plane holds hashes, not data: it chooses the nodes that hold a blob
 * and says which do, and the bytes travel between this device and those nodes.
 * Mirrors the node agent's `store` module (crates/decomp-node/src/store.rs).
 */

import { fromBase64, toHex } from './bytes';
import { type ControlPlane, ControlPlaneError } from './client';
import { root as merkleRoot } from './merkle';
import type { BlobData, Located, Placement, Primitives, Signer } from './protocol';

/** A transfer that did not go through, with a reason a person can read. */
export class TransferError extends Error {}

/** What a device needs to store and fetch blobs. */
export type Storage = {
  controlPlane: ControlPlane;
  /** This device's key for signing; it pays for what it stores. */
  signer: Signer;
  primitives: Primitives;
};

/**
 * How long a device keeps a job's input on storage nodes: long enough for the
 * job, the checks that may follow it, and its reissue as a known-answer job
 * (`JOB_DATA_DAYS`). A starting value.
 */
export const JOB_DATA_DAYS = 7;

/** The longest a blob can be stored for, in days (`MAX_STORAGE_DAYS`). */
export const MAX_STORAGE_DAYS = 365;

/** Where a node's server keeps blobs, in its web addresses. */
export const BLOB_PATH = '/v1/stored/';

/**
 * How long to wait before offering a body again to a node that is not ready for
 * it. A node is not ready until it has heard from the control plane, at its next
 * check-in, what it is to carry or hold.
 */
const RETRY_MS = 500;
/** How many times a blob is offered to a node before it is given up on. */
export const UPLOAD_ATTEMPTS = 30;

/**
 * How many times `putBlob` asks whether a node holds a blob it took in. Nodes
 * say so when they next ask for work, seconds apart.
 */
const HELD_ATTEMPTS = 60;

export const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

/**
 * Stores `data` on storage nodes for `days` days, paid by the signer's account,
 * and resolves with its root once a node has said it holds it, so that from
 * then on anyone can fetch it. A blob is stored once whoever stores it: if it
 * already is, it is not stored again.
 */
export async function putBlob({ controlPlane, signer, primitives }: Storage, data: Uint8Array, days: number): Promise<string> {
  const root = toHex(await merkleRoot(data, primitives.sha256));
  let placement: Placement | null = null;
  try {
    placement = await controlPlane.request<Placement>(signer, { type: 'store_blob', root, size: data.length, days });
  } catch (error) {
    if (!(error instanceof ControlPlaneError && error.code === 'already_stored')) throw error;
  }
  if (placement) {
    let nodes = 0;
    for (const address of placement.addresses) {
      if (await offer(`${address}${BLOB_PATH}${root}`, 'PUT', data, UPLOAD_ATTEMPTS)) nodes += 1;
    }
    if (nodes === 0) throw new TransferError('No node took the data to hold.');
  }
  for (let attempt = 0; attempt < HELD_ATTEMPTS; attempt += 1) {
    const located = await controlPlane.request<Located>(signer, { type: 'locate_blob', root }).catch(() => null);
    if (located && located.addresses.length > 0) return root;
    await sleep(RETRY_MS);
  }
  throw new TransferError('No node said it holds the data in time.');
}

/**
 * Fetches a blob from the first storage node that gives a true copy of it. A
 * blob no storage node holds may be one the control plane keeps for always-on
 * work, such as what a job that fell back produced; it is fetched from there.
 */
export async function getBlob({ controlPlane, signer, primitives }: Storage, root: string): Promise<Uint8Array> {
  const located = await controlPlane.request<Located>(signer, { type: 'locate_blob', root }).catch(() => null);
  if (located && located.addresses.length > 0) return fetchStored(primitives, root, located);
  const blob = await controlPlane.request<BlobData>(signer, { type: 'get_blob', hash: root });
  const bytes = fromBase64(blob.data);
  if (toHex(await merkleRoot(bytes, primitives.sha256)) !== root) {
    throw new TransferError('The control plane sent something other than the blob.');
  }
  return bytes;
}

/** Fetches a stored blob from the first node that gives a true copy of it. */
export async function fetchStored(primitives: Primitives, root: string, located: Located): Promise<Uint8Array> {
  for (const address of located.addresses) {
    const response = await fetch(`${address}${BLOB_PATH}${root}`).catch(() => null);
    if (!response?.ok) continue;
    const bytes = new Uint8Array(await response.arrayBuffer());
    const named = bytes.length === located.size && toHex(await merkleRoot(bytes, primitives.sha256)) === root;
    if (named) return bytes;
  }
  throw new TransferError('No node that holds the data gave it.');
}

/**
 * Offers a body to a node until it takes it. Resolves with `false` if the node
 * was still not ready after `attempts` tries; throws if it refuses outright.
 */
export async function offer(
  url: string,
  method: 'POST' | 'PUT',
  body: string | Uint8Array,
  attempts: number,
): Promise<boolean> {
  const headers = typeof body === 'string' ? { 'content-type': 'application/json' } : undefined;
  for (let attempt = 0; attempt < attempts; attempt += 1) {
    const response = await fetch(url, { method, headers, body: body as BodyInit }).catch(() => {
      throw new TransferError('A node could not be reached.');
    });
    if (response.ok) return true;
    // Not yet asked to carry or hold this (404, 409 for a blob), or told to
    // wait while the receiver catches up (429).
    const notReady = [404, 429].includes(response.status) || (method === 'PUT' && response.status === 409);
    if (!notReady) throw new TransferError(`A node refused: ${await response.text()}`);
    await sleep(RETRY_MS);
  }
  return false;
}
