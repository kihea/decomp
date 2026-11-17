/**
 * Getting data from this device to another, and collecting what was sent to this
 * one. Mirrors the node agent's `deliver`, `relay`, and `store` modules
 * (crates/decomp-node/src), which say how the three ways across work and why.
 *
 * Everything sent is sealed to the receiving device's key, so the nodes that
 * carry or hold it cannot read it.
 */

import { concat, equal, fromBase64, fromHex, toBase64, toHex, utf8 } from './bytes';
import { type Coordinator, CoordinatorError } from './client';
import { root as merkleRoot } from './merkle';
import {
  type Envelope,
  type Frame,
  type FrameHeader,
  FRAME_BYTES,
  type IncomingChannel,
  type IncomingChannels,
  type Located,
  now,
  type Opener,
  type Placement,
  type Primitives,
  type PublishedKey,
  type Request,
  type Route,
  seal,
  SEAL_OVERHEAD_BYTES,
  SIGNING_DOMAIN,
  type Signer,
  type Take,
  type Taken,
} from './protocol';

/** A transfer that did not go through, with a reason a person can read. */
export class TransferError extends Error {}

/** What a device needs to send and receive. */
export type Transfers = {
  coordinator: Coordinator;
  primitives: Primitives;
  /** This device's key for signing. */
  signer: Signer;
  /** This device's key for opening what is sealed to it. */
  opener: Opener;
};

/** Where a node's server keeps channels and blobs, in its web addresses. */
const CHANNEL_PATH = '/v1/relay/';
const BLOB_PATH = '/v1/stored/';

/** Mixed into what a sender signs inside a seal (`SENDER_PURPOSE`). */
const SENDER_PURPOSE = utf8('decomp/v1 sealed from\n');

/**
 * How long to wait before offering a frame or a blob again to a node that is not
 * ready for it, and how many times to offer. A node is not ready until it has
 * heard from the coordinator, at its next check-in, what it is to carry or hold.
 */
const RETRY_MS = 500;
const SEND_ATTEMPTS = 120;
const UPLOAD_ATTEMPTS = 30;

/** How often a receiver asks a relay for more when none is waiting, and how
 * many times in a row it may find nothing before giving up on a channel. */
const TAKE_POLL_MS = 500;
const TAKE_ATTEMPTS = 120;

const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

/** Publishes the key that data for this device should be sealed to. */
export async function publishSealingKey({ coordinator, signer, opener }: Transfers): Promise<void> {
  await coordinator.request(signer, { type: 'publish_sealing_key', key: opener.sealingKey });
}

/**
 * The sealing key `device` has published, checked against that device's own
 * signature so that nobody's word has to be taken for it. `null` if it has
 * published none.
 */
async function sealingKeyOf(transfers: Transfers, device: string): Promise<string | null> {
  const { coordinator, primitives, signer } = transfers;
  const call = { type: 'get_sealing_key', device } as const;
  const { proof } = await coordinator.request<PublishedKey>(signer, call);
  if (!proof || proof.signer !== device || !(await signedBy(primitives, proof))) return null;
  const request = JSON.parse(proof.payload) as Request;
  return request.call.type === 'publish_sealing_key' ? request.call.key : null;
}

/** Whether an envelope's signature is its signer's. */
function signedBy(primitives: Primitives, envelope: Envelope): Promise<boolean> {
  const signed = utf8(SIGNING_DOMAIN + envelope.payload);
  return primitives.verify(envelope.signer, signed, fromHex(envelope.signature));
}

/** How data got on its way. */
export type Delivered =
  /** Over a channel, to the receiving device itself or to a relay for it. */
  | { how: 'sent'; route: Route }
  /** Left with this many nodes, to wait for the device. */
  | { how: 'stored'; root: string; nodes: number };

/**
 * Sends `data` to the device `to`. If the device is not waiting to receive,
 * leaves the data in storage for it, paid for `keepDays` days.
 */
export async function deliver(
  transfers: Transfers,
  to: string,
  data: Uint8Array,
  keepDays: number,
): Promise<Delivered> {
  const sealingKey = await sealingKeyOf(transfers, to);
  if (!sealingKey) throw new TransferError('That device has not published a key to seal to.');
  try {
    return { how: 'sent', route: await sendOverChannel(transfers, to, sealingKey, data) };
  } catch (error) {
    if (!(error instanceof CoordinatorError && error.code === 'receiver_away')) throw error;
  }

  const { coordinator, primitives, signer } = transfers;
  // Stored data has no channel to say who it is from, so the sender signs it
  // inside the seal. The signature covers the receiving device.
  const signature = await signer.sign(concat(SENDER_PURPOSE, fromHex(to), data));
  const signed = concat(fromHex(signer.publicKey), signature, data);
  const sealed = await primitives.sealTo(sealingKey, signed);
  const root = toHex(await merkleRoot(sealed, primitives.sha256));
  const call = { type: 'store_blob', root, size: sealed.length, days: keepDays, to } as const;
  const placement = await coordinator.request<Placement>(signer, call);

  let nodes = 0;
  for (const address of placement.addresses) {
    if (await offer(`${address}${BLOB_PATH}${root}`, 'PUT', sealed, UPLOAD_ATTEMPTS)) nodes += 1;
  }
  if (nodes === 0) throw new TransferError('No node took the data to hold.');
  return { how: 'stored', root, nodes };
}

async function sendOverChannel(
  { coordinator, primitives, signer }: Transfers,
  to: string,
  sealingKey: string,
  data: Uint8Array,
): Promise<Route> {
  // Each frame is sealed on its own, so it carries a little less than a frame.
  const pieceBytes = FRAME_BYTES - SEAL_OVERHEAD_BYTES;
  const pieces: Uint8Array[] = [];
  for (let start = 0; start < Math.max(data.length, 1); start += pieceBytes) {
    pieces.push(data.subarray(start, start + pieceBytes));
  }
  const bytes = data.length + pieces.length * SEAL_OVERHEAD_BYTES;
  const route = await coordinator.request<Route>(signer, { type: 'open_relay', to, bytes });
  const address = `${route.address}${CHANNEL_PATH}${route.channel}/frames`;

  let sent = 0;
  for (const [seq, piece] of pieces.entries()) {
    const sealed = await primitives.sealTo(sealingKey, piece);
    sent += sealed.length;
    const header: FrameHeader = {
      channel: route.channel,
      seq,
      bytes: sent,
      hash: toHex(await primitives.sha256(sealed)),
      end: seq === pieces.length - 1,
    };
    const frame: Frame = { header: await seal(signer, header), data: toBase64(sealed) };
    if (!(await offer(address, 'POST', JSON.stringify(frame), SEND_ATTEMPTS))) {
      throw new TransferError('The relay was not ready to carry the data in time.');
    }
  }
  return route;
}

/**
 * Offers a body to a node until it takes it. Resolves with `false` if the node
 * was still not ready after `attempts` tries; throws if it refuses outright.
 */
async function offer(
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

/** Something another device sent to this one. */
export type Collected = {
  /** The device that sent it, as its signature shows. */
  from: string;
  data: Uint8Array;
  /** The root it was stored under, if it had been left in storage. */
  storedAs?: string;
};

/**
 * Collects one thing sent to this device, if anything is waiting: over a channel
 * first, then from storage. Asking also shows that this device is waiting to
 * receive.
 *
 * `skip` names what not to try: channels that went wrong, and stored blobs
 * already collected, which stay listed until their paid time runs out. A channel
 * or blob that goes wrong here is added to it before the error is thrown, so
 * calling again moves on to the next.
 *
 * `accepts` says which senders to take from. What any other device sent is
 * left where it is: nothing of it is fetched, and a relay holding it is not paid.
 */
export async function collect(
  transfers: Transfers,
  skip: Set<string>,
  accepts: (from: string) => boolean = () => true,
): Promise<Collected | null> {
  const { coordinator, primitives, signer, opener } = transfers;
  const waiting = await coordinator.request<IncomingChannels>(signer, { type: 'list_incoming' });
  for (const incoming of waiting.channels) {
    if (skip.has(incoming.channel) || !accepts(incoming.from)) continue;
    try {
      const data = await receiveFromChannel(transfers, incoming);
      if (data) return { from: incoming.from, data };
    } catch (error) {
      skip.add(incoming.channel);
      throw error;
    }
  }
  for (const stored of waiting.stored) {
    if (skip.has(stored.root) || !accepts(stored.from)) continue;
    skip.add(stored.root);
    const located = await coordinator.request<Located>(signer, { type: 'locate_blob', root: stored.root });
    const sealed = await fetchStored(primitives, stored.root, located);
    const opened = await opener.open(sealed);
    if (!opened || opened.length < 96) throw new TransferError('What was stored was not sealed to this device.');
    const [from, signature, data] = [opened.subarray(0, 32), opened.subarray(32, 96), opened.subarray(96)];
    const signed = concat(SENDER_PURPOSE, fromHex(signer.publicKey), data);
    if (!(await primitives.verify(toHex(from), signed, signature))) {
      throw new TransferError('What was stored does not say who it is from.');
    }
    // Whoever stored it may not be who signed it, so the signer is asked about too.
    if (!accepts(toHex(from))) continue;
    return { from: toHex(from), data, storedAs: stored.root };
  }
  return null;
}

/** Fetches a stored blob from the first node that gives a true copy of it. */
async function fetchStored(primitives: Primitives, root: string, located: Located): Promise<Uint8Array> {
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
 * Collects everything on a channel, checking each frame against the sender's
 * signature and opening it. Resolves with `null` if nothing is waiting on the
 * channel, which is the case for one already collected.
 */
async function receiveFromChannel(
  { primitives, signer, opener }: Transfers,
  incoming: IncomingChannel,
): Promise<Uint8Array | null> {
  const { channel, from } = incoming;
  const address = `${incoming.address}${CHANNEL_PATH}${channel}/take`;
  const ask = async (after: number): Promise<Frame[]> => {
    const take: Take = { channel, after, issued_at: now() };
    const response = await fetch(address, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(await seal(signer, take)),
    }).catch(() => {
      throw new TransferError('The relay could not be reached.');
    });
    // The relay does not know the channel: it has not heard of it yet, or is done with it.
    if (response.status === 404) return [];
    if (!response.ok) throw new TransferError(`The relay refused: ${await response.text()}`);
    return ((await response.json()) as Taken).frames;
  };

  const pieces: Uint8Array[] = [];
  let carried = 0;
  let idle = 0;
  for (;;) {
    const frames = await ask(pieces.length);
    if (frames.length === 0) {
      if (pieces.length === 0) return null;
      idle += 1;
      if (idle >= TAKE_ATTEMPTS) throw new TransferError('The sender stopped before the end.');
      await sleep(TAKE_POLL_MS);
      continue;
    }
    idle = 0;
    for (const frame of frames) {
      const header = JSON.parse(frame.header.payload) as FrameHeader;
      // A frame already held, sent again.
      if (header.seq < pieces.length) continue;
      const sealed = fromBase64(frame.data);
      const genuine =
        frame.header.signer === from &&
        (await signedBy(primitives, frame.header)) &&
        header.channel === channel &&
        header.seq === pieces.length &&
        header.bytes === carried + sealed.length &&
        equal(fromHex(header.hash), await primitives.sha256(sealed));
      if (!genuine) throw new TransferError('The relay handed over something the sender did not send.');
      const opened = await opener.open(sealed);
      if (!opened) throw new TransferError('What arrived was not sealed to this device.');
      pieces.push(opened);
      carried = header.bytes;
      if (header.end) {
        // Sign for the last frames, so the relay can be paid for them. The data
        // is whole whether or not this gets through.
        await ask(pieces.length).catch(() => undefined);
        return concat(...pieces);
      }
    }
  }
}
