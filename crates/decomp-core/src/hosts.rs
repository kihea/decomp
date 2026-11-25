//! Nodes that other devices can connect to. Holding data and relaying it both mean
//! accepting connections, so only these nodes are given that work (spec section 2,
//! the data class of jobs). Which nodes are online is not kept across a restart.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::jobs::ONLINE_WINDOW_SECS;
use crate::keys::{AccountId, PublicKey};

/// A node other devices can connect to.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Host {
    pub node: PublicKey,
    pub account: AccountId,
    /// Where other devices reach the node, such as `http://203.0.113.5:9000`.
    pub address: String,
}

/// The nodes that say other devices can connect to them, and when each last
/// checked in.
#[derive(Default)]
pub struct Hosts {
    last_seen: HashMap<PublicKey, (Host, u64)>,
}

impl Hosts {
    /// Where `node` is reached, if it said.
    pub fn address(&self, node: &PublicKey) -> Option<String> {
        self.last_seen
            .get(node)
            .map(|(host, _)| host.address.clone())
    }

    /// Notes that a node checked in, and whether other devices can reach it.
    pub fn seen(&mut self, node: PublicKey, account: AccountId, address: Option<String>, now: u64) {
        match address {
            Some(address) => {
                let host = Host {
                    node,
                    account,
                    address,
                };
                self.last_seen.insert(node, (host, now));
            }
            None => {
                self.last_seen.remove(&node);
            }
        }
    }

    /// The hosts that have checked in recently enough to count as online.
    pub fn online(&self, now: u64) -> Vec<Host> {
        self.last_seen
            .values()
            .filter(|(_, last_seen)| now.saturating_sub(*last_seen) <= ONLINE_WINDOW_SECS)
            .map(|(host, _)| host.clone())
            .collect()
    }
}
