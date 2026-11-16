/**
 * The cryptography a device needs, done with WebCrypto, for platforms that have
 * it: browsers and desktop web views. Sealing mirrors `decomp-core`'s `seal`
 * module byte for byte, which says what each step is for.
 */

import { concat, fromHex, toHex, utf8 } from './bytes';
import type { Opener, Primitives } from './protocol';

/** Mixed into every cipher key (`KEY_PURPOSE`). */
const KEY_PURPOSE = utf8('decomp/v1 seal');

/** The cipher's nonce. It may be fixed because each cipher key seals one message. */
const NONCE = new Uint8Array(12);

/** Bytes in an X25519 public key, which is what a sealed message starts with. */
const PUBLIC_KEY_BYTES = 32;

// TypeScript's WebCrypto types want buffers that cannot be shared between threads.
const buffer = (bytes: Uint8Array) => bytes as BufferSource;

/** The cipher key for one message, from what its two sides share. */
async function cipherKey(shared: ArrayBuffer, once: Uint8Array, to: Uint8Array): Promise<CryptoKey> {
  const material = await crypto.subtle.importKey('raw', shared, 'HKDF', false, ['deriveKey']);
  const derivation = { name: 'HKDF', hash: 'SHA-256', salt: buffer(concat(once, to)), info: buffer(KEY_PURPOSE) };
  const cipher = { name: 'AES-GCM', length: 256 };
  return crypto.subtle.deriveKey(derivation, material, cipher, false, ['encrypt', 'decrypt']);
}

function sharedWith(secret: CryptoKey, theirs: Uint8Array): Promise<ArrayBuffer> {
  return crypto.subtle
    .importKey('raw', buffer(theirs), 'X25519', false, [])
    .then((key) => crypto.subtle.deriveBits({ name: 'X25519', public: key }, secret, 256));
}

export const webPrimitives: Primitives = {
  sha256: async (data) => new Uint8Array(await crypto.subtle.digest('SHA-256', buffer(data))),

  verify: async (publicKey, message, signature) => {
    try {
      const key = await crypto.subtle.importKey('raw', buffer(fromHex(publicKey)), 'Ed25519', false, ['verify']);
      return await crypto.subtle.verify('Ed25519', key, buffer(signature), buffer(message));
    } catch {
      return false;
    }
  },

  sealTo: async (sealingKey, message) => {
    const to = fromHex(sealingKey);
    // A new key pair for each message, used once.
    const once = (await crypto.subtle.generateKey('X25519', false, ['deriveBits'])) as CryptoKeyPair;
    const oncePublic = new Uint8Array(await crypto.subtle.exportKey('raw', once.publicKey));
    const key = await cipherKey(await sharedWith(once.privateKey, to), oncePublic, to);
    const encrypted = await crypto.subtle.encrypt({ name: 'AES-GCM', iv: NONCE }, key, buffer(message));
    return concat(oncePublic, new Uint8Array(encrypted));
  },
};

/** Makes an X25519 key pair to open with. The secret half cannot be read back out. */
export async function createOpeningKeys(): Promise<CryptoKeyPair> {
  return (await crypto.subtle.generateKey('X25519', false, ['deriveBits'])) as CryptoKeyPair;
}

/** An opener that works with a WebCrypto key pair, wherever the pair is kept. */
export async function webOpener(keys: CryptoKeyPair): Promise<Opener> {
  const own = new Uint8Array(await crypto.subtle.exportKey('raw', keys.publicKey));
  return {
    sealingKey: toHex(own),
    open: async (sealed) => {
      try {
        const once = sealed.subarray(0, PUBLIC_KEY_BYTES);
        const key = await cipherKey(await sharedWith(keys.privateKey, once), once, own);
        const encrypted = buffer(sealed.subarray(PUBLIC_KEY_BYTES));
        return new Uint8Array(await crypto.subtle.decrypt({ name: 'AES-GCM', iv: NONCE }, key, encrypted));
      } catch {
        return null;
      }
    },
  };
}
