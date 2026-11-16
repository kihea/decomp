//! Getting data from this device to another, and collecting what was sent to
//! this one (spec section 1, "Device-to-device communication").
//!
//! There are three ways across, and the sender does not choose:
//!
//! - straight to the receiving device, if it is waiting and other devices can
//!   connect to it, which costs nothing;
//! - through a relay node, if it is waiting but cannot be reached;
//! - into storage on nodes, if it is not waiting, where the data stays until the
//!   device collects it or the time paid for runs out.
//!
//! The first two are the same to a sender: a channel, whose far end the
//! coordinator picks. Either way only the receiving device can read what is
//! sent, and it can tell who sent it.

use std::collections::HashSet;

use decomp_core::{ContentHash, OpeningKey, PublicKey, Route, merkle, seal};

use crate::client::Client;
use crate::relay::{self, TransferError};
use crate::store;

/// How data got on its way.
pub enum Delivered {
    /// Over a channel, to the receiving device itself or to a relay for it.
    Sent(Route),
    /// Left with this many nodes, to wait for the device.
    Stored { root: ContentHash, nodes: usize },
}

/// Sends `data` to the device `to`. If the device is not waiting to receive,
/// leaves the data in storage for it, paid for `keep_days` days.
pub fn deliver(
    client: &Client,
    to: PublicKey,
    data: &[u8],
    keep_days: u32,
) -> Result<Delivered, TransferError> {
    match relay::send(client, to, data) {
        Ok(route) => return Ok(Delivered::Sent(route)),
        Err(TransferError::Coordinator(error)) if error.is("receiver_away") => {}
        Err(error) => return Err(error),
    }

    let sealing_key = client.sealing_key(to)?.ok_or(TransferError::NoSealingKey)?;
    // Stored data has no channel to say who it is from, so the sender signs it
    // inside the seal.
    let sealed = seal::seal_from(client.key(), &to, &sealing_key, data)
        .ok_or(TransferError::NoSealingKey)?;
    let root = merkle::root(&sealed);
    let placement = client.store_blob(root, sealed.len() as u64, keep_days, Some(to))?;
    match store::upload_to(&placement, &root, &sealed) {
        0 => Err(TransferError::NotStored),
        nodes => Ok(Delivered::Stored { root, nodes }),
    }
}

/// Something another device sent to this one.
pub struct Collected {
    /// The device that sent it, as its signature shows.
    pub from: PublicKey,
    pub data: Vec<u8>,
    /// The root it was stored under, if it had been left in storage.
    pub stored_as: Option<ContentHash>,
}

/// Collects one thing sent to this device, if anything is waiting: over a
/// channel first, then from storage.
///
/// `skip` names what not to try: channels that went wrong, and stored blobs
/// already collected, which stay listed until their paid time runs out. A
/// channel or blob that goes wrong here is added to it before the error is
/// returned, so calling again moves on to the next.
pub fn collect(
    client: &Client,
    opening_key: &OpeningKey,
    skip: &mut HashSet<ContentHash>,
) -> Result<Option<Collected>, TransferError> {
    let waiting = client.incoming()?;
    for incoming in &waiting.channels {
        if skip.contains(&incoming.channel) {
            continue;
        }
        match relay::receive(client, opening_key, incoming) {
            Ok(Some(data)) => {
                let from = incoming.from;
                return Ok(Some(Collected {
                    from,
                    data,
                    stored_as: None,
                }));
            }
            Ok(None) => {}
            Err(error) => {
                skip.insert(incoming.channel);
                return Err(error);
            }
        }
    }
    for stored in &waiting.stored {
        if !skip.insert(stored.root) {
            continue;
        }
        let sealed = store::fetch(client, stored.root).map_err(TransferError::Unreachable)?;
        let (from, data) = seal::open_from(opening_key, &client.device_key(), &sealed)
            .ok_or(TransferError::NotForThisDevice)?;
        return Ok(Some(Collected {
            from,
            data,
            stored_as: Some(stored.root),
        }));
    }
    Ok(None)
}
