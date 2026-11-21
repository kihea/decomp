/**
 * Private values: an account's values encrypted with keys that no single
 * party holds (spec section 7, "Privacy"). A device asks the control plane for
 * the key for a label; the network's threshold key (ICP vetKeys) derives it
 * for the account and encrypts it to a transport key the device made for the
 * request. Every device of the account gets the same key for the same label,
 * so a value written on one device opens on the others.
 *
 * Each key costs the network to derive and an account may ask for a limited
 * number a day, so keep the key for as long as the app runs.
 */

import { DerivedKeyMaterial, DerivedPublicKey, EncryptedVetKey, TransportSecretKey } from '@dfinity/vetkeys';

import { concat, fromBase64, fromHex, toBase64, utf8 } from './bytes';
import type { ControlPlane } from './client';
import { readRecord, writeRecord, type RecordValue } from './records';
import type { AccountRecord, Signer, ValueKeyAnswer } from './protocol';

/** The longest label a value key may be asked for under (`MAX_VALUE_KEY_LABEL_BYTES`). */
export const MAX_VALUE_KEY_LABEL_BYTES = 256;

/** A value key: encrypts and opens values with AES-GCM, each under a purpose. */
export type ValueKey = DerivedKeyMaterial;

/**
 * The key for the signer's account's private values under `label`. `account`
 * is the account's ID (hex); the key is checked against the threshold key's
 * public key for it, so a wrong account or a forged answer is refused.
 */
export async function getValueKey(
  controlPlane: ControlPlane,
  signer: Signer,
  account: string,
  label: string,
): Promise<ValueKey> {
  const transport = TransportSecretKey.random();
  const answer = await controlPlane.request<ValueKeyAnswer>(signer, {
    type: 'get_value_key',
    label,
    transport_key: toBase64(transport.publicKeyBytes()),
  });
  // What the key was derived from (`value_key_input`): the account, then the label.
  const input = concat(fromHex(account), utf8(label));
  const publicKey = DerivedPublicKey.deserialize(fromBase64(answer.public_key));
  const vetKey = EncryptedVetKey.deserialize(fromBase64(answer.encrypted_key)).decryptAndVerify(
    transport,
    publicKey,
    input,
  );
  return vetKey.asDerivedKeyMaterial();
}

/** The purpose a record's value is encrypted under, so values cannot be swapped between keys. */
function recordPurpose(key: string): string {
  return `decomp/records/${key}`;
}

/**
 * Writes `value` to a record of the signer's account encrypted with
 * `valueKey`, if `version` (0 for a key never written) is still current.
 */
export async function writePrivateRecord(
  controlPlane: ControlPlane,
  signer: Signer,
  valueKey: ValueKey,
  key: string,
  version: number,
  value: Uint8Array,
): Promise<AccountRecord> {
  const sealed = await valueKey.encryptMessage(value, recordPurpose(key));
  return writeRecord(controlPlane, signer, key, version, sealed);
}

/**
 * Reads a record of the signer's account written with `writePrivateRecord`,
 * and opens it with `valueKey`. Resolves with null if the key was never
 * written; rejects if the value was not encrypted with this key.
 */
export async function readPrivateRecord(
  controlPlane: ControlPlane,
  signer: Signer,
  valueKey: ValueKey,
  key: string,
): Promise<RecordValue | null> {
  const read = await readRecord(controlPlane, signer, key);
  if (read === null) return null;
  return { record: read.record, value: await valueKey.decryptMessage(read.value, recordPurpose(key)) };
}
