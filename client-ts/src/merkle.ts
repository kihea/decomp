/**
 * The name of a stored blob: the root of a hash tree over its chunks. Mirrors
 * `decomp-core`'s `merkle` module, which says why blobs are named this way.
 */

import { concat } from './bytes';
import { CHUNK_BYTES, type Primitives } from './protocol';

// A chunk and a pair of hashes are hashed with different first bytes, so a pair
// of hashes can never be passed off as a chunk.
const CHUNK_MARK = new Uint8Array([0]);
const PAIR_MARK = new Uint8Array([1]);

/** The root of `data`'s hash tree. An empty blob has one chunk, empty. */
export async function root(data: Uint8Array, sha256: Primitives['sha256']): Promise<Uint8Array> {
  let level: Uint8Array[] = [];
  for (let start = 0; start < Math.max(data.length, 1); start += CHUNK_BYTES) {
    level.push(await sha256(concat(CHUNK_MARK, data.subarray(start, start + CHUNK_BYTES))));
  }
  while (level.length > 1) {
    const above: Uint8Array[] = [];
    for (let index = 0; index < level.length; index += 2) {
      // Each pair is hashed together; an odd one out is carried up as it is.
      const [left, right] = [level[index], level[index + 1]];
      above.push(right ? await sha256(concat(PAIR_MARK, left, right)) : left);
    }
    level = above;
  }
  return level[0];
}
