/** Turning bytes into the text forms the wire format uses, and back. */

export function toHex(bytes: Uint8Array): string {
  return Array.from(bytes, (byte) => byte.toString(16).padStart(2, '0')).join('');
}

/** Reads lowercase or uppercase hex. Throws if `text` is not hex. */
export function fromHex(text: string): Uint8Array {
  if (text.length % 2 !== 0 || /[^0-9a-f]/i.test(text)) throw new Error('not hex');
  const bytes = new Uint8Array(text.length / 2);
  for (let index = 0; index < bytes.length; index += 1) {
    bytes[index] = Number.parseInt(text.slice(index * 2, index * 2 + 2), 16);
  }
  return bytes;
}

const BASE64 = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';

/** Standard base64 with padding, which is how raw bytes travel inside JSON. */
export function toBase64(bytes: Uint8Array): string {
  const groups: string[] = [];
  for (let index = 0; index < bytes.length; index += 3) {
    const [first, second, third] = [bytes[index], bytes[index + 1], bytes[index + 2]];
    const bits = (first << 16) | ((second ?? 0) << 8) | (third ?? 0);
    groups.push(
      BASE64[bits >> 18] +
        BASE64[(bits >> 12) & 63] +
        (second === undefined ? '=' : BASE64[(bits >> 6) & 63]) +
        (third === undefined ? '=' : BASE64[bits & 63]),
    );
  }
  return groups.join('');
}

/** Throws if `text` is not standard base64. */
export function fromBase64(text: string): Uint8Array {
  const digits = text.replace(/=+$/, '');
  const bytes = new Uint8Array(Math.floor((digits.length * 3) / 4));
  let bits = 0;
  let held = 0;
  let written = 0;
  for (const digit of digits) {
    const value = BASE64.indexOf(digit);
    if (value < 0) throw new Error('not base64');
    bits = (bits << 6) | value;
    held += 6;
    if (held >= 8) {
      held -= 8;
      bytes[written] = (bits >> held) & 255;
      written += 1;
    }
  }
  return bytes;
}

export function concat(...parts: Uint8Array[]): Uint8Array {
  const joined = new Uint8Array(parts.reduce((length, part) => length + part.length, 0));
  let offset = 0;
  for (const part of parts) {
    joined.set(part, offset);
    offset += part.length;
  }
  return joined;
}

export function utf8(text: string): Uint8Array {
  return new TextEncoder().encode(text);
}

export function equal(left: Uint8Array, right: Uint8Array): boolean {
  return left.length === right.length && left.every((byte, index) => byte === right[index]);
}
