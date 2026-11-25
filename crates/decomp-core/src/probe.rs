//! Timed transfers of test data, which a node's bandwidth score is measured by
//! (spec section 1, "Node profile": "Bandwidth | Timed transfers of dummy
//! files").
//!
//! Now and then the job board asks a node to fetch test data from a node of
//! another account that holds data, and to say how long it took. The data is
//! made from a seed the job board chose, so the node serving it cannot have
//! had it ready, and the node fetching it must show the data's hash, so it
//! cannot report a transfer it did not make. It can still misstate the time;
//! the measurement is one of many in a moving average, and the measuring node
//! is drawn from another account at random.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::keys::{ContentHash, PublicKey};

/// How much test data one measurement moves: enough that the time is mostly
/// the transfer, little enough to cost nobody much.
pub const PROBE_BYTES: u64 = 1024 * 1024;

/// How often a node that asks for work is asked to measure another node.
pub const PROBE_INTERVAL_SECS: u64 = 3600;

/// How long a node has to report a measurement it was asked for.
pub const PROBE_TIMEOUT_SECS: u64 = 60;

/// Where a node that holds data serves test data, in its web addresses: the
/// seed and the length follow, as `<seed>/<bytes>`.
pub const PROBE_PATH: &str = "/v1/probe/";

/// A measurement a node is asked to make.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Probe {
    pub seed: ContentHash,
    /// The node measured, and the address it serves data at.
    pub target: PublicKey,
    pub address: String,
    pub bytes: u64,
}

/// The test data for `seed`: `bytes` bytes, each run of 32 the hash of the
/// seed and the run's number.
pub fn data(seed: &ContentHash, bytes: u64) -> Vec<u8> {
    let mut data = Vec::with_capacity(bytes as usize);
    let mut count: u64 = 0;
    while (data.len() as u64) < bytes {
        let mut hasher = Sha256::new();
        hasher.update(seed.as_bytes());
        hasher.update(count.to_le_bytes());
        let block: [u8; 32] = hasher.finalize().into();
        let take = (bytes - data.len() as u64).min(32) as usize;
        data.extend_from_slice(&block[..take]);
        count += 1;
    }
    data
}
