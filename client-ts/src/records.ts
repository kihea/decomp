/**
 * Records: small pieces of an account's state kept on the network, each a key
 * that points to a blob (spec, "Data management", "Records"). A write names
 * the version it read and is refused if another write landed first, so two
 * devices writing the same record cannot both win.
 */

import { ControlPlaneError } from './client';
import type { AccountRecord } from './protocol';
import { getBlob, MAX_STORAGE_DAYS, putBlob, type Storage } from './storage';

/** How many times `updateRecord` reads again after losing to another write. */
const MAX_RETRIES = 5;

/**
 * How long a record's value is kept on storage nodes. The record points at it
 * for as long as nothing writes over it, so it is kept as long as a blob can be.
 */
const RECORD_VALUE_DAYS = MAX_STORAGE_DAYS;

/** A record and the bytes of its value. */
export type RecordValue = { record: AccountRecord; value: Uint8Array };

/**
 * Reads a record of `owner`'s account, the signer's own if `owner` is left
 * out. Resolves with null if the key was never written.
 */
export async function readRecord(storage: Storage, key: string, owner?: string): Promise<RecordValue | null> {
  try {
    const { controlPlane, signer } = storage;
    const record = await controlPlane.request<AccountRecord>(signer, { type: 'get_record', owner, key });
    return { record, value: await getBlob(storage, record.value) };
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
  storage: Storage,
  key: string,
  version: number,
  value: Uint8Array,
): Promise<AccountRecord> {
  const root = await putBlob(storage, value, RECORD_VALUE_DAYS);
  const call = { type: 'write_record', key, version, value: root } as const;
  return storage.controlPlane.request<AccountRecord>(storage.signer, call);
}

/**
 * Reads a record, works out its next value, and writes it, reading again and
 * retrying if another write lands in between.
 */
export async function updateRecord(
  storage: Storage,
  key: string,
  next: (current: Uint8Array | null) => Uint8Array,
): Promise<AccountRecord> {
  for (let attempt = 0; ; attempt += 1) {
    const current = await readRecord(storage, key);
    try {
      return await writeRecord(storage, key, current?.record.version ?? 0, next(current?.value ?? null));
    } catch (error) {
      const lost = error instanceof ControlPlaneError && error.code === 'stale_version';
      if (!lost || attempt >= MAX_RETRIES) throw error;
    }
  }
}
