/**
 * How an account's controllers approve a device, as a client makes the
 * approvals. The rules are in `decomp-core`'s `controller`; this file must
 * change together with it.
 */

import { toBase64, toHex, utf8 } from './bytes';
import type { Approval, Primitives, Signer } from './protocol';

/** Starts the text a controller signs to approve a device (`APPROVAL_DOMAIN`). */
const APPROVAL_DOMAIN = 'decomp/v1/approve-device\n';

/** The text a controller signs to approve `device` into `account` (`controller::statement`). */
export function approvalStatement(account: string, device: string): string {
  return `${APPROVAL_DOMAIN}${account}\n${device}`;
}

/**
 * An approval by a key that signs as account and device keys do: a recovery
 * key, or a device of a guardian account.
 */
export async function keyApproval(signer: Signer, statement: string): Promise<Approval> {
  const signature = await signer.sign(utf8(statement));
  return { type: 'key', signer: signer.publicKey, signature: toHex(signature) };
}

/** The challenge a passkey is asked to sign to approve a device: the hash of the statement. */
export function passkeyChallenge(primitives: Primitives, statement: string): Promise<Uint8Array> {
  return primitives.sha256(utf8(statement));
}

/** What a passkey answers a challenge with (WebAuthn's `AuthenticatorAssertionResponse`). */
export type PasskeyAssertion = {
  authenticatorData: ArrayBuffer;
  clientDataJSON: ArrayBuffer;
  signature: ArrayBuffer;
};

/** An approval by a passkey, from its answer to `passkeyChallenge`. */
export function passkeyApproval(assertion: PasskeyAssertion): Approval {
  return {
    type: 'passkey',
    authenticator_data: toBase64(new Uint8Array(assertion.authenticatorData)),
    client_data: new TextDecoder().decode(assertion.clientDataJSON),
    signature: toBase64(new Uint8Array(assertion.signature)),
  };
}
