/**
 * Reading an app's state as a device does (the command-line node's `state` does
 * the same in Rust). Anyone can read a shard; only a job changes one.
 */

import type { ControlPlane } from './client';
import { getBlob } from './jobs';
import type { ShardNow, Signer } from './protocol';

/** Bytes in a length or count in the byte form of a set of records. */
const LENGTH_BYTES = 4;

/** A shard's records: each key, as text, with its value. */
export type Records = Map<string, Uint8Array>;

/**
 * The records a shard of an app's state holds now. A shard no job has written
 * to holds none.
 */
export async function readShard(
  control: ControlPlane,
  signer: Signer,
  app: string,
  shard: string,
): Promise<Records> {
  const now = await control.request<ShardNow>(signer, { type: 'get_state', app, shard });
  if (now.contents === null) return new Map();
  return decodeRecords(await getBlob(control, signer, now.contents));
}

/**
 * Reads records from their byte form, which `decomp-schema`'s `records` module
 * defines: a count, then each record's key and value, each with its length
 * before it. Counts and lengths are 32-bit little-endian integers.
 */
export function decodeRecords(bytes: Uint8Array): Records {
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  let offset = 0;
  const takeLength = () => {
    const length = view.getUint32(offset, true);
    offset += LENGTH_BYTES;
    return length;
  };
  const takePart = () => {
    const length = takeLength();
    if (offset + length > bytes.length) throw new Error('not a set of records');
    const part = bytes.subarray(offset, offset + length);
    offset += length;
    return part;
  };
  const records: Records = new Map();
  for (let left = takeLength(); left > 0; left -= 1) {
    const key = new TextDecoder().decode(takePart());
    records.set(key, takePart());
  }
  return records;
}
