/**
 * Records: small pieces of an account's state kept on the network, each a key
 * that points to a blob (spec, "Data management", "Records"). A write names
 * the version it read and is refused if another write landed first, so two
 * devices writing the same record cannot both win.
 */

import { ControlPlaneError, type ControlPlane } from './client';
import { getBlob, putBlob } from './jobs';
import type { AccountRecord, Signer } from './protocol';

/** How many times `updateRecord` reads again after losing to another write. */
const MAX_RETRIES = 5;

/** A record and the bytes of its value. */
export type RecordValue = { record: AccountRecord; value: Uint8Array };

/**
 * Reads a record of `owner`'s account, the signer's own if `owner` is left
 * out. Resolves with null if the key was never written.
 */
export async function readRecord(
  controlPlane: ControlPlane,
  signer: Signer,
  key: string,
  owner?: string,
): Promise<RecordValue | null> {
  try {
    const record = await controlPlane.request<AccountRecord>(signer, { type: 'get_record', owner, key });
    return { record, value: await getBlob(controlPlane, signer, record.value) };
  } catch (error) {
    if (error instanceof ControlPlaneError && error.code === 'unknown_record') return null;
    throw error;
  }
}

/**
 * Writes `value` to a record of the signer's account, if `version` (0 for a
 * key never written) is still current. Rejects with `stale_version` otherwise.
 */
export async function writeRecord(
  controlPlane: ControlPlane,
  signer: Signer,
  key: string,
  version: number,
  value: Uint8Array,
): Promise<AccountRecord> {
  const hash = await putBlob(controlPlane, signer, value);
  return controlPlane.request<AccountRecord>(signer, { type: 'write_record', key, version, value: hash });
}

/**
 * Reads a record, works out its next value, and writes it, reading again and
 * retrying if another write lands in between.
 */
export async function updateRecord(
  controlPlane: ControlPlane,
  signer: Signer,
  key: string,
  next: (current: Uint8Array | null) => Uint8Array,
): Promise<AccountRecord> {
  for (let attempt = 0; ; attempt += 1) {
    const current = await readRecord(controlPlane, signer, key);
    try {
      return await writeRecord(controlPlane, signer, key, current?.record.version ?? 0, next(current?.value ?? null));
    } catch (error) {
      const lost = error instanceof ControlPlaneError && error.code === 'stale_version';
      if (!lost || attempt >= MAX_RETRIES) throw error;
    }
  }
}
