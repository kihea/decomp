//! Naming stored data so that any part of it can be proven.
//!
//! A stored blob is cut into chunks and named by the root of a hash tree over
//! them. Someone who knows only the root can then check one chunk, given the
//! hashes that link it to the root. That is what lets the coordinator challenge a
//! node to show it still holds a blob without holding the blob itself.

use sha2::{Digest, Sha256};

use crate::keys::ContentHash;

/// The size of every chunk of a stored blob except, usually, its last.
pub const CHUNK_BYTES: usize = 64 * 1024;

// A chunk and a pair of hashes are hashed with different first bytes, so a pair
// of hashes can never be passed off as a chunk.
const CHUNK_MARK: u8 = 0;
const PAIR_MARK: u8 = 1;

/// How many chunks a blob of `size` bytes has. An empty blob has one, empty.
pub fn chunk_count(size: u64) -> u64 {
    size.div_ceil(CHUNK_BYTES as u64).max(1)
}

/// The name of `data` as a stored blob: the root of its hash tree.
pub fn root(data: &[u8]) -> ContentHash {
    let mut level = chunk_hashes(data);
    while level.len() > 1 {
        level = pair_up(&level);
    }
    ContentHash::from_bytes(level[0])
}

/// The chunk of `data` at `index`, and the hashes that link it to the root.
pub fn prove(data: &[u8], index: u64) -> Option<(&[u8], Vec<ContentHash>)> {
    let chunk = chunks(data).nth(usize::try_from(index).ok()?)?;
    let mut proof = Vec::new();
    let mut level = chunk_hashes(data);
    let mut position = index as usize;
    while level.len() > 1 {
        // The hash beside this one, unless this one is the odd one out.
        if let Some(beside) = level.get(position ^ 1) {
            proof.push(ContentHash::from_bytes(*beside));
        }
        level = pair_up(&level);
        position /= 2;
    }
    Some((chunk, proof))
}

/// Whether `chunk` is the chunk at `index` of the blob named `root`, which is
/// `size` bytes long.
pub fn verify(
    root: &ContentHash,
    size: u64,
    index: u64,
    chunk: &[u8],
    proof: &[ContentHash],
) -> bool {
    let count = chunk_count(size);
    if index >= count {
        return false;
    }
    let expected_length = if index + 1 < count {
        CHUNK_BYTES as u64
    } else {
        size - index * CHUNK_BYTES as u64
    };
    if chunk.len() as u64 != expected_length {
        return false;
    }

    let mut hash = chunk_hash(chunk);
    let mut proof = proof.iter();
    let (mut position, mut width) = (index, count);
    while width > 1 {
        let on_the_right = position % 2 == 1;
        let has_neighbor = on_the_right || position + 1 < width;
        if has_neighbor {
            let Some(beside) = proof.next() else {
                return false;
            };
            hash = if on_the_right {
                pair_hash(beside.as_bytes(), &hash)
            } else {
                pair_hash(&hash, beside.as_bytes())
            };
        }
        position /= 2;
        width = width.div_ceil(2);
    }
    proof.next().is_none() && hash == *root.as_bytes()
}

fn chunks(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    // `chunks` yields nothing for empty data, but an empty blob has one chunk.
    let empty = data.is_empty().then_some(data);
    data.chunks(CHUNK_BYTES).chain(empty)
}

fn chunk_hashes(data: &[u8]) -> Vec<[u8; 32]> {
    chunks(data).map(chunk_hash).collect()
}

fn chunk_hash(chunk: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update([CHUNK_MARK]);
    hasher.update(chunk);
    hasher.finalize().into()
}

fn pair_hash(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update([PAIR_MARK]);
    hasher.update(left);
    hasher.update(right);
    hasher.finalize().into()
}

/// The next level up: each pair hashed together, an odd one carried up as it is.
fn pair_up(level: &[[u8; 32]]) -> Vec<[u8; 32]> {
    level
        .chunks(2)
        .map(|pair| match pair {
            [left, right] => pair_hash(left, right),
            [odd] => *odd,
            _ => unreachable!("chunks(2) yields one or two items"),
        })
        .collect()
}
