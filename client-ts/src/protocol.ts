/**
 * The DeComp wire format, as a client speaks it. The rules live in the Rust crate
 * `decomp-core`; this file mirrors its message shapes and signing convention and
 * must change together with it.
 */

import { toHex, utf8 } from './bytes';

/** A key this device can sign with. The secret never leaves the key store. */
export type Signer = {
  /** Ed25519 public key, as lowercase hex. */
  publicKey: string;
  sign(message: Uint8Array): Promise<Uint8Array>;
};

/**
 * The key this device opens sealed data with (`decomp-core`'s `seal`). The
 * secret never leaves the key store.
 */
export type Opener = {
  /** The X25519 public key others seal to, as lowercase hex. */
  sealingKey: string;
  /** Resolves with `null` if `sealed` was not sealed to this key or was changed. */
  open(sealed: Uint8Array): Promise<Uint8Array | null>;
};

/**
 * The cryptography a device needs besides its own keys. Each platform supplies
 * it in its own way; all must agree with `decomp-core`.
 */
export type Primitives = {
  sha256(data: Uint8Array): Promise<Uint8Array>;
  /** Whether `signature` is the Ed25519 signature of `message` by `publicKey` (hex). */
  verify(publicKey: string, message: Uint8Array, signature: Uint8Array): Promise<boolean>;
  /** Seals `message` to a sealing key (hex), as `seal::seal` does. */
  sealTo(sealingKey: string, message: Uint8Array): Promise<Uint8Array>;
};

/** Bytes a sealed message is longer than the message (`SEAL_OVERHEAD_BYTES`). */
export const SEAL_OVERHEAD_BYTES = 48;

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

/** What an account's app is made of. The account's ID is the app's ID. */
export type AppManifest = {
  name: string;
  /** The app's job modules, by hash. */
  modules: string[];
  /** Left out, the app pays for no jobs. */
  sponsorship?: {
    /** The most the app sets aside in a day for one account's jobs, in millionths. */
    per_account_daily: number;
  };
};

export type Account = {
  id: string;
  account_key: string;
  /** Number of changes so far. The next change must carry this value. */
  seq: number;
  devices: Device[];
  /** The app this account has published, if any. */
  app?: AppManifest;
  balance: Balance;
};

export type AccountOp =
  | { type: 'create_account'; device_key: string }
  | { type: 'approve_device'; account_id: string; seq: number; device_key: string }
  | { type: 'revoke_device'; account_id: string; seq: number; device_key: string }
  | { type: 'publish_app'; account_id: string; seq: number; app: AppManifest };

/**
 * What a request asks for. Of the calls `decomp-core` defines, these are the ones
 * this client makes; the rest are added here as something needs them.
 */
export type Call =
  | { type: 'get_account' }
  | { type: 'publish_sealing_key'; key: string }
  | { type: 'get_sealing_key'; device: string }
  | { type: 'open_relay'; to: string; bytes: number }
  | { type: 'list_incoming' }
  | { type: 'store_blob'; root: string; size: number; days: number; to?: string }
  | { type: 'locate_blob'; root: string };

/** A signed request for anything other than changing an account. */
export type Request = { issued_at: number; call: Call };

/** A JSON payload and a signature over its exact text. */
export type Envelope = { payload: string; signer: string; signature: string };

/** Mixed into everything an envelope's signature covers. */
export const SIGNING_DOMAIN = 'decomp/v1\n';

/** Signs `message` into an envelope, as `Envelope::seal` does. */
export async function seal(signer: Signer, message: object): Promise<Envelope> {
  const payload = JSON.stringify(message);
  const signature = await signer.sign(utf8(SIGNING_DOMAIN + payload));
  return { payload, signer: signer.publicKey, signature: toHex(signature) };
}

/** Where a sender sends a channel's frames (`Route`). */
export type Route = { channel: string; address: string };

/** A channel open to this device (`Incoming`). */
export type IncomingChannel = { channel: string; from: string; address: string };

/** A stored blob left for this device (`Waiting`). */
export type Waiting = { root: string; size: number; from: string };

export type IncomingChannels = { channels: IncomingChannel[]; stored: Waiting[] };

/** The nodes a blob should be uploaded to. */
export type Placement = { addresses: string[] };

/** Where a stored blob can be fetched from. */
export type Located = { size: number; addresses: string[] };

/** A device's signed request publishing its sealing key, if it has published one. */
export type PublishedKey = { proof: Envelope | null };

/** What the sender signs for each frame of a channel (`FrameHeader`). */
export type FrameHeader = {
  channel: string;
  /** The frame's place on the channel, counting from zero. */
  seq: number;
  /** All the data sent on the channel up to and including this frame. */
  bytes: number;
  /** The SHA-256 hash of this frame's data, as hex. */
  hash: string;
  /** Whether this is the channel's last frame. */
  end: boolean;
};

/** One piece of what a channel carries: a signed header and base64 data. */
export type Frame = { header: Envelope; data: string };

/** A receiver asking a relay for frames, and signing for those before `after`. */
export type Take = { channel: string; after: number; issued_at: number };

export type Taken = { frames: Frame[] };

/** The most data one frame may carry (`FRAME_BYTES`). */
export const FRAME_BYTES = 256 * 1024;

/** The size of every chunk of a stored blob except, usually, its last (`CHUNK_BYTES`). */
export const CHUNK_BYTES = 64 * 1024;

/** Whether `text` has the form of a public key: 64 lowercase hex characters. */
export function isPublicKey(text: string): boolean {
  return /^[0-9a-f]{64}$/.test(text);
}

/** Seconds since 1970, which is how requests are dated. */
export function now(): number {
  return Math.floor(Date.now() / 1000);
}
