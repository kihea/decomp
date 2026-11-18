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
  /** Credits the account earned or bought. */
  earned: number;
  /** Earned credits put up as a guarantee for the account's nodes. Not spendable. */
  staked: number;
  /** Earned credits set aside for cash-outs that are neither paid nor refused. */
  cashing_out: number;
};

/**
 * One way to prove control of an account, as the account record holds it
 * (`Controller`). `key` is hex: 32 bytes for a recovery key or a recovery
 * service's key, and the 65 bytes of a curve point for a passkey (P-256) or a
 * wallet (secp256k1).
 */
export type Controller =
  | { type: 'key'; key: string }
  | { type: 'passkey'; key: string }
  | { type: 'wallet'; key: string }
  | { type: 'guardians'; accounts: string[]; required: number };

/** One signature over the statement that approves a device (`Approval`). */
export type Approval =
  | { type: 'key'; signer: string; signature: string }
  | {
      type: 'passkey';
      /** Base64. */
      authenticator_data: string;
      /** The client data exactly as the browser made it. */
      client_data: string;
      /** Base64. */
      signature: string;
    }
  /** Hex: the 65 bytes an Ethereum wallet's `personal_sign` gives. */
  | { type: 'wallet'; signature: string };

/** A device the account's controllers approved, which has not joined yet. */
export type Recovery = {
  device_key: string;
  /** When the device may join, in seconds since 1970. */
  effective_at: number;
};

/** The most controllers an account can require to agree (`MOST_METHODS_REQUIRED`). */
export const MOST_METHODS_REQUIRED = 2;

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
  /** The hash of the app's client files, which a gateway serves at its name. */
  client?: string;
  /** The hash of a blob of metadata the application layer defines. */
  metadata?: string;
};

export type Account = {
  id: string;
  account_key: string;
  /**
   * Where the account's balance sits on the ledger. Only the account's own
   * devices are shown it; what the control posts names this, never `id`.
   */
  ledger_address: string;
  /** Number of changes so far. The next change must carry this value. */
  seq: number;
  devices: Device[];
  /** The ways the user added to sign in and to recover the account. */
  controllers: Controller[];
  /** How many of `controllers` must approve a device for its recovery to start. */
  methods_required: number;
  /** Recoveries that are waiting to take effect. */
  recoveries: Recovery[];
  /** The app this account has published, if any. */
  app?: AppManifest;
  balance: Balance;
  /** The combined reliability of the account's devices, from 0 to 1. */
  reputation: number;
  /** When the account first appeared, in seconds since 1970. */
  created: number;
};

export type AccountOp =
  | { type: 'create_account'; device_key: string }
  | { type: 'approve_device'; account_id: string; seq: number; device_key: string }
  | { type: 'revoke_device'; account_id: string; seq: number; device_key: string }
  | { type: 'publish_app'; account_id: string; seq: number; app: AppManifest }
  /** Signed by the account key itself. */
  | {
      type: 'set_controllers';
      account_id: string;
      seq: number;
      controllers: Controller[];
      methods_required: number;
    }
  /** Signed by the device to recover. `at` is the time now, in seconds since 1970. */
  | { type: 'recover_device'; account_id: string; at: number; approvals: Approval[] }
  /** Signed by the device being recovered, once its wait is over. */
  | { type: 'complete_recovery'; account_id: string; at: number }
  | { type: 'cancel_recovery'; account_id: string; seq: number; device_key: string };

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
  | { type: 'locate_blob'; root: string }
  | { type: 'put_blob'; data: string }
  | { type: 'get_blob'; hash: string }
  | { type: 'submit_job'; manifest: Manifest }
  | { type: 'get_receipt'; job_id: string }
  | { type: 'quote_job'; manifest: Manifest }
  | { type: 'get_state'; app: string; shard: string }
  | { type: 'get_work_preferences'; device: string }
  | { type: 'set_work_preferences'; device: string; preferences: WorkPreferences }
  | { type: 'place_bid'; bid: BidOrder }
  | { type: 'cancel_bid'; id: string }
  | { type: 'list_bids' }
  | { type: 'get_rates' }
  | { type: 'get_activity'; since: number }
  | { type: 'register_name'; name: string }
  | { type: 'renew_name'; name: string }
  | { type: 'release_name'; name: string }
  | { type: 'resolve_name'; name: string }
  | {
      type: 'list_directory';
      verified_only?: boolean;
      /** Only names that contain this text. */
      query?: string | null;
      from?: string | null;
      limit?: number;
    };

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

/** The three kinds of job (`JobClass`). */
export type JobClass = 'menial' | 'data' | 'inference';

/**
 * How a job's result is confirmed (`Verify`): accepted after one result and
 * confirmed if a second node picks it up in time; not accepted until a second
 * node confirms; or one result with no confirmation.
 */
export type Verify = 'opportunistic' | 'required' | 'none';

/**
 * A job as its submitter describes it (`Manifest`). Of its fields, these are
 * the ones this client sets; the rest are left to their defaults.
 */
export type Manifest = {
  module: string;
  input: string;
  /** Compute units the job may use before it is stopped. */
  compute_cap: number;
  /** Latest acceptable finish time, in seconds since 1970. */
  deadline: number;
  /** Credits set aside for the job, in millionths. Ask `quote_job` for the least that is accepted. */
  max_fee: number;
  /** Left out, menial. */
  class?: JobClass;
  /** Slots needed at once on one machine. Left out, one. */
  slots?: number;
  /** Left out, opportunistic. */
  verify?: Verify;
  /** Run the job in the control plane if no node finishes it in time. */
  fallback?: 'always_on';
  /** Left out, open. It must be what the job's module declares. */
  privacy?: Privacy;
  /** Set when the job is one share of a private job. */
  share?: Share;
  /**
   * Keys, an account's or a device's, as hex. No node of an account one of
   * them belongs to is handed the job.
   */
  keep_from?: string[];
  /**
   * IDs of earlier jobs. No node of an account that was handed one of them is
   * handed this job.
   */
  apart_from?: string[];
};

/** Which share of which private job a job is (`Share`). */
export type Share = {
  /**
   * The name the submitter's device gave the private job's shares: 32 random
   * bytes, as hex. No account is handed two jobs that carry the same name.
   */
  set: string;
  /** The share's place among the job's shares, counting from zero. */
  index: number;
};

/** The mode a job runs in (`Privacy`). The nodes that run an open job can read its data. */
export type Privacy = 'open' | 'private';

/** The base rate for compute, in microcredits per compute unit (`BASE_MICROS_PER_UNIT`). */
export const BASE_MICROS_PER_UNIT = 250;

/** The rate posted for one class and tier (`Rate`). */
export type Rate = {
  class: JobClass;
  tier: number;
  micros_per_unit: number;
  /** The tier's premium, in thousandths. */
  premium: number;
  waiting: number;
  asking: number;
};

/** The answer to `quote_job`. `highest_fee` is the least `max_fee` that is accepted. */
export type Quote = { rate: Rate; fallback_micros_per_unit: number; highest_fee: number };

/** What a device's owner lets it be offered (`WorkPreferences`). */
export type WorkPreferences = {
  contributing: boolean;
  classes: JobClass[];
  min_tier: number;
  max_tier: number;
  /** The lowest posted rate worked for, in microcredits per unit. 0 takes any. */
  floor_micros_per_unit: number;
  slots: number;
  /** Local hours of the day, start inclusive, end exclusive. Null is any hour. */
  hours: [number, number] | null;
  on_battery: boolean;
  on_metered: boolean;
  /** Take work in tiers 3 and 4 only through the account's bids. */
  market_only: boolean;
};

/** What a bid asks to be paid, in microcredits (`Price`). */
export type Price = { per_unit: number } | { per_job: number };

/** A standing order to run jobs at a price (`BidOrder`). */
export type BidOrder = {
  class: JobClass;
  min_tier: number;
  max_tier: number;
  quantity: number;
  price: Price;
  /** When the bid lapses, in seconds since 1970. */
  expires_at: number;
};

/** A bid as the job board holds it (`Bid`). */
export type Bid = BidOrder & { id: string; account: string; filled: number };

/** What an account earned and spent, in microcredits (`Activity`). */
export type Activity = { earned: number; spent: number };

/** A name and the app it points to (`NameRecord`). */
export type NameRecord = {
  name: string;
  app: string;
  registered_at: number;
  expires_at: number;
  /** The app was reviewed and may be listed in a directory. */
  verified: boolean;
};

/** The answer to `resolve_name`, and one entry of a directory (`Resolved`). */
export type Resolved = NameRecord & { manifest: AppManifest };

/** The answer to `list_directory` (`Directory`). */
export type Directory = { entries: Resolved[]; next: string | null };

/** The answer to `put_blob`. */
export type BlobRef = { hash: string };

/** The answer to `get_blob`: the bytes, as base64. */
export type BlobData = { data: string };

/** The answer to `get_state`: the hash of the shard's contents, or null if it is empty. */
export type ShardNow = { contents: string | null };

/** The answer to `submit_job`. */
export type Submitted = { job_id: string };

/** Why a job produced no output (`Failure`). */
export type Failure = 'compute_cap_exceeded' | 'trapped' | 'invalid_job';

/** How a job ended (`Outcome`), as far as this client reads it. */
export type Outcome =
  | { status: 'completed'; output: string }
  | { status: 'failed'; failure: Failure };

/**
 * The private route a job was run by (`PrivateRoute`). A private job sent to
 * nodes without a `share` is on the encrypted compute route: its private values
 * must be encrypted as `decomp-core`'s `encrypted` module does, which this
 * client does not do yet.
 */
export type PrivateRoute = 'local' | 'private_totals' | 'encrypted_compute';

/**
 * The verification level a result got (`Verification`): a second node's
 * result matched; a checkpoint dispute named the right one; the control plane
 * ran it as an always-on fallback; or one node's result was accepted on its
 * reputation.
 */
export type Verification = 'confirmed' | 'refereed' | 'always_on' | 'unconfirmed';

/**
 * The parts of a job's receipt this client reads (`Receipt`). With the fee,
 * the last four are what every result reports.
 */
export type Receipt = {
  units: number;
  fee: { total: number };
  outcome: Outcome;
  /** Left out for an open job. */
  route?: PrivateRoute;
  verified: Verification;
  /** How many paid runs the fee covers: 1 unconfirmed, 2 confirmed. */
  runs: number;
  /** Seconds from when the job was submitted to when its result stood. */
  seconds: number;
  /** Whether the job ran inside a hardware enclave. Always false so far. */
  enclave: boolean;
};

/** Where a job stands, as its submitter sees it (`JobStatus`). */
export type JobStatus =
  | { state: 'submitted' | 'leased' | 'committed' | 'disputed' | 'rejected' | 'expired' }
  | { state: 'settled'; receipt: Receipt };

/** Whether `text` has the form of a public key: 64 lowercase hex characters. */
export function isPublicKey(text: string): boolean {
  return /^[0-9a-f]{64}$/.test(text);
}

/** Seconds since 1970, which is how requests are dated. */
export function now(): number {
  return Math.floor(Date.now() / 1000);
}
