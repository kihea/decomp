/**
 * The DeComp wire format, as a client speaks it. The rules live in the Rust crate
 * `decomp-core`; this file mirrors its message shapes and signing convention and
 * must change together with it.
 */

/** A key this device can sign with. The secret never leaves the key store. */
export type Signer = {
  /** Ed25519 public key, as lowercase hex. */
  publicKey: string;
  sign(message: Uint8Array): Promise<Uint8Array>;
};

export type Device = { key: string; revoked: boolean };

/** Credit amounts travel as whole millionths of a credit. */
export const MICROS_PER_CREDIT = 1_000_000;

export type Balance = {
  /** Starter credits: they can be spent but never cashed out. */
  granted: number;
  earned: number;
  /** Earned credits put up as a guarantee for the account's nodes. Not spendable. */
  staked: number;
};

export type Account = {
  id: string;
  account_key: string;
  /** Number of changes so far. The next change must carry this value. */
  seq: number;
  devices: Device[];
  balance: Balance;
};

export type AccountOp =
  | { type: 'create_account'; device_key: string }
  | { type: 'approve_device'; account_id: string; seq: number; device_key: string }
  | { type: 'revoke_device'; account_id: string; seq: number; device_key: string };

/**
 * A signed request for anything other than changing an account. Of the calls
 * `decomp-core` defines, this client makes one so far; the rest are added here as
 * something needs them.
 */
export type Request = { issued_at: number; call: { type: 'get_account' } };

/** A JSON payload and a signature over its exact text. */
export type Envelope = { payload: string; signer: string; signature: string };

const SIGNING_DOMAIN = 'decomp/v1\n';

export async function seal(signer: Signer, message: AccountOp | Request): Promise<Envelope> {
  const payload = JSON.stringify(message);
  const signature = await signer.sign(new TextEncoder().encode(SIGNING_DOMAIN + payload));
  return { payload, signer: signer.publicKey, signature: bytesToHex(signature) };
}

function bytesToHex(bytes: Uint8Array): string {
  return Array.from(bytes, (byte) => byte.toString(16).padStart(2, '0')).join('');
}

/** Whether `text` has the form of a public key: 64 lowercase hex characters. */
export function isPublicKey(text: string): boolean {
  return /^[0-9a-f]{64}$/.test(text);
}
