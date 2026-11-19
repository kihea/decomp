/**
 * Talks to the control plane: a local devnet (`crates/decomp-devnet`) or a
 * gateway to the network's canisters. Both take the same signed calls.
 */

import { type Account, type AccountOp, type Call, type Envelope, now, seal, type Signer } from './protocol';

/** A request the control plane refused or that never reached it. */
export class ControlPlaneError extends Error {
  /** The API's error code, `unreachable` if no response came, or `unexpected`. */
  readonly code: string;

  constructor(code: string, message: string) {
    super(message);
    this.code = code;
  }
}

/** The control plane, as a device sees it. Every call is signed by the device. */
export type ControlPlane = {
  /** Signs and submits a change to an account; resolves with the changed account. */
  submitOp(signer: Signer, op: AccountOp): Promise<Account>;
  /** Fetches the account `signer` belongs to. */
  fetchAccount(signer: Signer): Promise<Account>;
  /** Signs and sends a request; resolves with the answer `decomp-core` names for that call. */
  request<Answer>(signer: Signer, call: Call): Promise<Answer>;
};

/** Makes a client for the control plane at `url`, such as `http://127.0.0.1:8787`. */
export function connect(url: string): ControlPlane {
  // An address may carry a query, as a canister's does on a gateway that names
  // it as `?canisterId=...`; it stays on every request.
  const [base, query] = url.includes('?') ? [url.slice(0, url.indexOf('?')), url.slice(url.indexOf('?'))] : [url, ''];
  const root = base.replace(/\/+$/, '');

  async function post<Answer>(path: string, envelope: Envelope): Promise<Answer> {
    let response: Response;
    try {
      response = await fetch(root + path + query, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify(envelope),
      });
    } catch {
      throw new ControlPlaneError('unreachable', 'The network could not be reached.');
    }
    const body = await response.json().catch(() => null);
    if (!response.ok) {
      throw new ControlPlaneError(
        body?.error ?? 'unexpected',
        body?.message ?? `The network answered with status ${response.status}.`,
      );
    }
    return body;
  }

  async function request<Answer>(signer: Signer, call: Call): Promise<Answer> {
    return post('/v1/request', await seal(signer, { issued_at: now(), call }));
  }

  return {
    submitOp: async (signer, op) => post('/v1/ops', await seal(signer, op)),
    fetchAccount: (signer) => request(signer, { type: 'get_account' }),
    request,
  };
}
