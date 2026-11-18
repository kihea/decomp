//! Relays as node work (spec section 1, "Device-to-device communication", and
//! section 2, the data class of jobs).
//!
//! Two devices that cannot connect to each other both connect to a relay: a node
//! that accepts connections. The sender opens a channel to the receiving device,
//! which sets aside the fee for the most it may send. It then sends frames to the
//! relay, and the receiver collects them from it.
//!
//! Every frame carries a header signed by the sender. It names the frame's place
//! on the channel and the hash of its data, so the receiver can tell that nothing
//! was changed, dropped, or reordered on the way, and it counts every byte sent
//! so far. Each time the receiver asks for more, it signs how many frames it
//! already has.
//!
//! A relay is paid for delivery, not for taking data in. Its claim is a pair: a
//! header the sender signed and a request in which the receiver signed for that
//! frame. Together they show how many bytes went the whole way, and the
//! coordinator pays for that many. So a relay that drops what it is given earns
//! nothing, a sender pays only for what arrived, and the most a relay can be
//! cheated of is what it hands over in one answer before the receiver signs.
//!
//! A channel is opened only to a device that is waiting to receive, which it
//! shows by asking what is addressed to it. If that device can itself be
//! connected to, it is its own relay: the sender connects straight to it and
//! nobody is paid. Data for a device that is not waiting goes into storage
//! instead (see `storage`).
//!
//! A channel carries bytes and does not look inside them. What devices send each
//! other is sealed to the receiving device's key first (see `seal`), so a relay
//! cannot read it.
//!
//! What must last (the channels and what has been paid) changes only by applying
//! entries, so a log of entries is a complete copy.

use std::collections::HashMap;

use rand::Rng;
use rand::seq::IndexedRandom;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::envelope::Envelope;
use crate::hosts::Host;
use crate::keys::{AccountId, ContentHash, PublicKey, SigningKey};
use crate::ledger::{Credits, LedgerEntry};
use crate::request::Blob;

/// What a relay is paid, before the fee split, to carry one gibibyte, in
/// microcredits. A starting value: the spec leaves relay rates to testing. A
/// credit buys about one slot-hour, which the spec's economics put near 1.5
/// cents, and bandwidth bought from a hosting company costs about a cent a
/// gigabyte, which comes to about 0.7 credits a gibibyte.
pub const MICROS_PER_GIB_RELAYED: u64 = 715_000;

/// The most one channel may carry. Also keeps the fee within what `Credits` holds.
pub const MAX_CHANNEL_BYTES: u64 = 1 << 40;

/// The most data one frame may carry.
pub const FRAME_BYTES: usize = 256 * 1024;

/// How long a channel stays open for sending and receiving.
pub const CHANNEL_SECS: u64 = 3600;

/// How long a device counts as waiting to receive after it last asked what is
/// addressed to it. A waiting device asks every second or so.
pub const LISTEN_WINDOW_SECS: u64 = 15;

/// How long after a channel closes its relay may still claim its pay.
pub const REDEEM_GRACE_SECS: u64 = 300;

const BYTES_PER_GIB: u64 = 1 << 30;

/// What a relay is paid, before the fee split, to carry `bytes` bytes.
pub fn relay_fee(bytes: u64) -> Credits {
    let micros_per_gib = u128::from(MICROS_PER_GIB_RELAYED);
    let micros = (u128::from(bytes) * micros_per_gib).div_ceil(u128::from(BYTES_PER_GIB));
    Credits::from_micros(micros as u64)
}

/// The name of the channel opened by the request with this signer and payload.
pub fn channel_id(signer: &PublicKey, payload: &str) -> ContentHash {
    let mut hasher = Sha256::new();
    hasher.update(signer.as_bytes());
    hasher.update(payload);
    ContentHash::from_bytes(hasher.finalize().into())
}

/// What the sender signs for each frame.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FrameHeader {
    pub channel: ContentHash,
    /// The frame's place on the channel, counting from zero.
    pub seq: u64,
    /// All the data sent on the channel up to and including this frame. This is
    /// what the sender agrees to pay the relay for.
    pub bytes: u64,
    /// The hash of this frame's data.
    pub hash: ContentHash,
    /// Whether this is the channel's last frame.
    pub end: bool,
}

/// One piece of what a channel carries.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Frame {
    /// A `FrameHeader`, signed by the sender.
    pub header: Envelope,
    pub data: Blob,
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("the frame was not signed by the channel's sender")]
    NotFromSender,
    #[error("the frame is not the next one on its channel")]
    OutOfOrder,
    #[error("the frame's data is not what its header describes")]
    WrongData,
}

impl Frame {
    /// Makes frame `seq` of `channel`, following `sent_before` bytes of data,
    /// signed by the sender.
    pub fn signed(
        key: &SigningKey,
        channel: ContentHash,
        seq: u64,
        sent_before: u64,
        data: Vec<u8>,
        end: bool,
    ) -> Self {
        let header = FrameHeader {
            channel,
            seq,
            bytes: sent_before + data.len() as u64,
            hash: ContentHash::of(&data),
            end,
        };
        Self {
            header: Envelope::seal(key, &header),
            data: Blob(data),
        }
    }

    /// Checks this frame as one that `from` sent on `channel`, for someone who
    /// already has the channel's first `frames` frames, carrying `bytes` bytes.
    /// Both the relay and the receiver check every frame this way.
    pub fn check(
        &self,
        channel: &ContentHash,
        from: &PublicKey,
        frames: u64,
        bytes: u64,
    ) -> Result<Checked, FrameError> {
        let header: FrameHeader = self.header.open().map_err(|_| FrameError::NotFromSender)?;
        if self.header.signer != *from {
            return Err(FrameError::NotFromSender);
        }
        if header.channel != *channel || header.seq > frames {
            return Err(FrameError::OutOfOrder);
        }
        if header.seq < frames {
            return Ok(Checked::Repeat);
        }
        let data = &self.data.0;
        if data.len() > FRAME_BYTES
            || header.hash != ContentHash::of(data)
            || header.bytes != bytes + data.len() as u64
        {
            return Err(FrameError::WrongData);
        }
        Ok(Checked::Next(header))
    }
}

/// What a frame that passed its check is.
pub enum Checked {
    /// The channel's next frame.
    Next(FrameHeader),
    /// A frame already held, sent again. There is nothing to do with it.
    Repeat,
}

/// A receiver asking a relay for the frames waiting on a channel. Signed by the
/// receiving device, so only it can collect them. It is also the receiver's
/// receipt for the frames before `after`, which the relay shows to be paid.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Take {
    pub channel: ContentHash,
    /// The receiver has every frame before this one, and the relay may forget them.
    pub after: u64,
    /// Seconds since 1970. Must be close to the relay's clock.
    pub issued_at: u64,
}

/// A relay's answer to a `Take`: the frames from `after` on that it has so far.
#[derive(Debug, Serialize, Deserialize)]
pub struct Taken {
    pub frames: Vec<Frame>,
}

/// A channel a node is asked to carry.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RelayOrder {
    pub channel: ContentHash,
    pub from: PublicKey,
    pub to: PublicKey,
    /// The most the channel may carry.
    pub bytes: u64,
    /// When the channel closes, in seconds since 1970.
    pub until: u64,
}

/// Where a sender sends a channel's frames.
#[derive(Debug, Serialize, Deserialize)]
pub struct Route {
    pub channel: ContentHash,
    /// The relay's address.
    pub address: String,
}

/// A channel open to a device, which it can collect from the relay at `address`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Incoming {
    pub channel: ContentHash,
    pub from: PublicKey,
    pub address: String,
}

/// A change to the channels.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RelayEntry {
    /// A channel was opened and a node asked to carry it.
    Opened {
        channel: ContentHash,
        from: PublicKey,
        to: PublicKey,
        relay: Host,
        bytes: u64,
        until: u64,
    },
    /// The relay was paid for carrying the channel's first `bytes` bytes.
    Redeemed { channel: ContentHash, bytes: u64 },
    /// The channel is over. What was not paid to the relay returned to the sender.
    Closed { channel: ContentHash },
}

#[derive(Debug, thiserror::Error)]
pub enum RelayError {
    #[error("this channel is already open")]
    ChannelExists,
    #[error("a channel carries between 1 and {MAX_CHANNEL_BYTES} bytes")]
    BadSize,
    #[error("no node that can relay is online")]
    NoRelay,
    #[error("the receiving device is not waiting to receive")]
    ReceiverAway,
    #[error("no such open channel")]
    UnknownChannel,
    #[error("this node does not carry that channel")]
    NotTheRelay,
    #[error("the header was not signed by the channel's sender")]
    NotFromSender,
    #[error("the header counts more bytes than the channel may carry")]
    OverBudget,
    #[error("the channel's receiver has not signed for that frame")]
    NotCollected,
}

/// A channel a device asks to open.
pub struct Opening {
    pub channel: ContentHash,
    /// The sending device, and the account that pays.
    pub from: PublicKey,
    pub payer: AccountId,
    /// The receiving device.
    pub to: PublicKey,
    /// The most the channel may carry.
    pub bytes: u64,
}

struct Channel {
    from: PublicKey,
    to: PublicKey,
    relay: Host,
    bytes: u64,
    until: u64,
    /// The relay has been paid for this many bytes.
    redeemed: u64,
}

impl Channel {
    /// What carrying the channel's first `bytes` bytes is worth: nothing, if the
    /// far end of the channel is the receiving device itself.
    fn fee(&self, bytes: u64) -> Credits {
        fee_through(&self.relay, &self.to, bytes)
    }
}

/// What `relay` is paid to carry `bytes` bytes to the device `to`.
fn fee_through(relay: &Host, to: &PublicKey, bytes: u64) -> Credits {
    if relay.node == *to {
        Credits::ZERO
    } else {
        relay_fee(bytes)
    }
}

/// The open channels.
#[derive(Default)]
pub struct Relays {
    channels: HashMap<ContentHash, Channel>,
    /// When each device last asked what is addressed to it. Not kept across a
    /// restart: a waiting device asks again within a second or so.
    listening: HashMap<PublicKey, u64>,
}

impl Relays {
    /// Applies one entry. Entries come from this type's own methods, so they
    /// always fit the state they are applied to.
    pub fn apply(&mut self, entry: &RelayEntry) {
        match entry {
            RelayEntry::Opened {
                channel,
                from,
                to,
                relay,
                bytes,
                until,
            } => {
                let opened = Channel {
                    from: *from,
                    to: *to,
                    relay: relay.clone(),
                    bytes: *bytes,
                    until: *until,
                    redeemed: 0,
                };
                self.channels.insert(*channel, opened);
            }
            RelayEntry::Redeemed { channel, bytes } => {
                if let Some(open) = self.channels.get_mut(channel) {
                    open.redeemed = *bytes;
                }
            }
            RelayEntry::Closed { channel } => {
                self.channels.remove(channel);
            }
        }
    }

    /// Notes that a device asked what is addressed to it, which is how it shows
    /// that it is waiting to receive.
    pub fn listening(&mut self, device: PublicKey, now: u64) {
        self.listening.insert(device, now);
    }

    /// Opens a channel to a device that is waiting to receive: straight to it if
    /// it is one of the `online` hosts, otherwise through one of them. Returns
    /// the entry that records it and the ledger entry that sets the whole fee
    /// aside.
    pub fn open(
        &self,
        opening: Opening,
        online: &[Host],
        now: u64,
        rng: &mut impl Rng,
    ) -> Result<(RelayEntry, LedgerEntry), RelayError> {
        let Opening {
            channel,
            from,
            payer,
            to,
            bytes,
        } = opening;
        if self.channels.contains_key(&channel) {
            return Err(RelayError::ChannelExists);
        }
        if bytes == 0 || bytes > MAX_CHANNEL_BYTES {
            return Err(RelayError::BadSize);
        }
        let last_asked = self.listening.get(&to);
        if last_asked.is_none_or(|at| now.saturating_sub(*at) > LISTEN_WINDOW_SECS) {
            return Err(RelayError::ReceiverAway);
        }
        let relay = match online.iter().find(|host| host.node == to) {
            Some(receiver) => receiver.clone(),
            None => online.choose(rng).ok_or(RelayError::NoRelay)?.clone(),
        };
        let amount = fee_through(&relay, &to, bytes);
        Ok((
            RelayEntry::Opened {
                channel,
                from,
                to,
                relay,
                bytes,
                until: now + CHANNEL_SECS,
            },
            LedgerEntry::DataEscrow {
                id: channel,
                account: payer,
                amount,
            },
        ))
    }

    /// The open channels `node` is asked to carry.
    pub fn orders_for(&self, node: &PublicKey, now: u64) -> Vec<RelayOrder> {
        self.open_at(now)
            .filter(|(_, open)| open.relay.node == *node)
            .map(|(channel, open)| RelayOrder {
                channel: *channel,
                from: open.from,
                to: open.to,
                bytes: open.bytes,
                until: open.until,
            })
            .collect()
    }

    /// The open channels addressed to the device `to`.
    pub fn incoming(&self, to: &PublicKey, now: u64) -> Vec<Incoming> {
        self.open_at(now)
            .filter(|(_, open)| open.to == *to)
            .map(|(channel, open)| Incoming {
                channel: *channel,
                from: open.from,
                address: open.relay.address.clone(),
            })
            .collect()
    }

    /// A relay shows a frame header from the sender and a `Take` in which the
    /// receiver signed for that frame, to be paid for the bytes the header counts
    /// beyond those already paid for. Returns the entries that record this, each
    /// with the credits it moves; none if nothing new is owed. A header that
    /// ends the channel also closes it.
    pub fn redeem(
        &self,
        node: &PublicKey,
        header: &Envelope,
        receipt: &Envelope,
    ) -> Result<Vec<(RelayEntry, LedgerEntry)>, RelayError> {
        let signed: FrameHeader = header.open().map_err(|_| RelayError::NotFromSender)?;
        let channel = signed.channel;
        let open = self
            .channels
            .get(&channel)
            .ok_or(RelayError::UnknownChannel)?;
        if open.relay.node != *node {
            return Err(RelayError::NotTheRelay);
        }
        if header.signer != open.from {
            return Err(RelayError::NotFromSender);
        }
        if signed.bytes > open.bytes {
            return Err(RelayError::OverBudget);
        }
        let collected: Take = receipt.open().map_err(|_| RelayError::NotCollected)?;
        if receipt.signer != open.to
            || collected.channel != channel
            || collected.after <= signed.seq
        {
            return Err(RelayError::NotCollected);
        }

        let mut entries = Vec::new();
        if signed.bytes > open.redeemed {
            let bytes = signed.bytes;
            let pay = LedgerEntry::DataPay {
                id: channel,
                account: open.relay.account,
                amount: open.fee(bytes) - open.fee(open.redeemed),
            };
            entries.push((RelayEntry::Redeemed { channel, bytes }, pay));
        }
        if signed.end {
            let refund = LedgerEntry::DataRefund { id: channel };
            entries.push((RelayEntry::Closed { channel }, refund));
        }
        Ok(entries)
    }

    /// What time has decided: channels whose relays have had their chance to
    /// claim are closed, and what is left of their fees is refunded.
    pub fn sweep(&self, now: u64) -> Vec<(RelayEntry, LedgerEntry)> {
        self.channels
            .iter()
            .filter(|(_, open)| open.until + REDEEM_GRACE_SECS <= now)
            .map(|(channel, _)| {
                let refund = LedgerEntry::DataRefund { id: *channel };
                (RelayEntry::Closed { channel: *channel }, refund)
            })
            .collect()
    }

    fn open_at(&self, now: u64) -> impl Iterator<Item = (&ContentHash, &Channel)> {
        self.channels
            .iter()
            .filter(move |(_, open)| open.until > now)
    }
}
