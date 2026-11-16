//! The byte form of a set of records: keys with a value each (spec section 7,
//! "State").
//!
//! A shard of an app's state is a set of records, and so is what `kv.scan` hands a
//! job. Each set has exactly one byte form, so two nodes that end a job with the
//! same records produce the same bytes and so the same hash:
//!
//! - the number of records, as a 32-bit little-endian integer;
//! - then each record, in order of its key's bytes: the key's length and the key,
//!   the value's length and the value. Lengths are 32-bit little-endian integers.

use std::collections::BTreeMap;

/// Records by key. A `BTreeMap` keeps them in the order they are written in.
pub type Records = BTreeMap<Vec<u8>, Vec<u8>>;

/// Bytes in a length or count.
const LENGTH_BYTES: usize = 4;

/// How many bytes the record with this key and value takes in the byte form.
pub fn record_bytes(key: &[u8], value: &[u8]) -> usize {
    2 * LENGTH_BYTES + key.len() + value.len()
}

pub fn encode(records: &Records) -> Vec<u8> {
    let mut bytes = (records.len() as u32).to_le_bytes().to_vec();
    for (key, value) in records {
        for part in [key, value] {
            bytes.extend((part.len() as u32).to_le_bytes());
            bytes.extend(part);
        }
    }
    bytes
}

/// Reads records back. `None` if `bytes` is not the one byte form of some set:
/// cut short, with bytes left over, or with keys repeated or out of order.
pub fn decode(bytes: &[u8]) -> Option<Records> {
    let mut rest = bytes;
    let count = take_length(&mut rest)?;
    let mut records = Records::new();
    for _ in 0..count {
        let key = take_part(&mut rest)?;
        let value = take_part(&mut rest)?;
        let in_order = records
            .last_key_value()
            .is_none_or(|(last, _)| last.as_slice() < key);
        if !in_order {
            return None;
        }
        records.insert(key.to_vec(), value.to_vec());
    }
    rest.is_empty().then_some(records)
}

/// Takes a length off the front of `rest`.
fn take_length(rest: &mut &[u8]) -> Option<usize> {
    let (length, after) = rest.split_at_checked(LENGTH_BYTES)?;
    *rest = after;
    Some(u32::from_le_bytes(length.try_into().ok()?) as usize)
}

/// Takes a key or a value, with the length before it, off the front of `rest`.
fn take_part<'a>(rest: &mut &'a [u8]) -> Option<&'a [u8]> {
    let length = take_length(rest)?;
    let (part, after) = rest.split_at_checked(length)?;
    *rest = after;
    Some(part)
}
