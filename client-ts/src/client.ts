/** Talks to a coordinator's HTTP API (crates/decomp-coordinator/src/api.rs). */

import { type Account, type AccountOp, type Call, type Envelope, now, seal, type Signer } from './protocol';

/** A request the coordinator refused or that never reached it. */
export class CoordinatorError extends Error {
  /** The API's error code, `unreachable` if no response came, or `unexpected`. */
  readonly code: string;

  constructor(code: string, message: string) {
    super(message);
    this.code = code;
  }
}

/** One coordinator, as a device sees it. Every call is signed by the device. */
export type Coordinator = {
  /** Signs and submits a change to an account; resolves with the changed account. */
  submitOp(signer: Signer, op: AccountOp): Promise<Account>;
  /** Fetches the account `signer` belongs to. */
  fetchAccount(signer: Signer): Promise<Account>;
  /** Signs and sends a request; resolves with the answer `decomp-core` names for that call. */
  request<Answer>(signer: Signer, call: Call): Promise<Answer>;
};

/** Makes a client for the coordinator at `url`, such as `http://127.0.0.1:8787`. */
export function connect(url: string): Coordinator {
  async function post<Answer>(path: string, envelope: Envelope): Promise<Answer> {
    let response: Response;
    try {
      response = await fetch(url + path, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify(envelope),
      });
    } catch {
      throw new CoordinatorError('unreachable', 'The coordinator could not be reached.');
    }
    const body = await response.json().catch(() => null);
    if (!response.ok) {
      throw new CoordinatorError(
        body?.error ?? 'unexpected',
        body?.message ?? `The coordinator answered with status ${response.status}.`,
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
