//! Relaying, from both ends: what a node does to carry channels between devices
//! that cannot connect to each other, and how a device sends and receives through
//! such a node. The rules, and what a frame is, are in `decomp_core`'s relay module.
//!
//! A device seals what it sends to the receiving device's key, so the relay
//! carries bytes it cannot read.
//!
//! A relay keeps a channel's frames in memory until the receiver signs for
//! them, and claims its pay with what the receiver signed.

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read};
use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::Duration;

use decomp_core::{
    Checked, ContentHash, Envelope, FRAME_BYTES, Frame, Incoming, MAX_CLOCK_SKEW_SECS, OpeningKey,
    PublicKey, REDEEM_GRACE_SECS, RelayOrder, Route, SEAL_OVERHEAD_BYTES, Take, Taken, seal,
};
use tiny_http::{Method, Request, Response};

use crate::client::{Client, ClientError, unix_now};
use crate::peer;

/// Where a node's server keeps channels, in its web addresses. A channel's
/// frames are sent to `{channel}/frames` and collected from `{channel}/take`.
pub(crate) const CHANNEL_PATH: &str = "/v1/relay/";
const FRAMES: &str = "frames";
const TAKE: &str = "take";

/// The most frame data a relay keeps on one channel for a receiver that has not
/// collected it. A sender that gets further ahead is told to wait.
const BUFFER_BYTES: usize = 8 * 1024 * 1024;

/// The most frame data a relay hands a receiver in one answer.
const TAKE_BYTES: usize = 4 * 1024 * 1024;

/// The largest request a relay reads: a frame, whose data travels as base64, with
/// room for its header.
const MAX_REQUEST_BYTES: u64 = 2 * FRAME_BYTES as u64;

/// The largest answer to a `Take` a receiver reads.
const MAX_TAKEN_BYTES: u64 = 2 * TAKE_BYTES as u64 + MAX_REQUEST_BYTES;

/// How long a sender waits before offering a frame again to a relay that is not
/// ready for it, and how many times it offers one.
const SEND_RETRY: Duration = Duration::from_millis(500);
const SEND_ATTEMPTS: u32 = 120;

/// How often a receiver asks a relay for more frames when it has none waiting.
pub const TAKE_POLL: Duration = Duration::from_millis(500);

/// How many times in a row a receiver may find nothing waiting before it gives
/// up on a channel that has not ended.
const TAKE_ATTEMPTS: u32 = 120;

/// HTTP statuses a relay answers with, besides success.
const NOT_KNOWN: u16 = 404;
const REFUSED: u16 = 409;
const WAIT: u16 = 429;

/// A channel this node carries.
struct Carried {
    order: RelayOrder,
    /// Frames the receiver has not yet signed for, oldest first, each with the
    /// count of data bytes on the channel up to and including it.
    waiting: VecDeque<(Frame, u64)>,
    /// How many frames the receiver has signed for.
    collected: u64,
    /// How many frames have arrived, and how many bytes of data they carried.
    frames: u64,
    bytes: u64,
    /// Whether the channel's last frame has arrived.
    ended: bool,
    /// What this node can show to be paid, once the receiver has signed for
    /// anything.
    delivery: Option<Delivery>,
    /// How many bytes this node has claimed pay for.
    claimed: u64,
}

/// Proof that a channel's first `bytes` bytes reached the receiver.
#[derive(Clone)]
struct Delivery {
    bytes: u64,
    /// The header of the newest frame the receiver has signed for.
    header: Envelope,
    /// The receiver's request that signed for it.
    receipt: Envelope,
}

impl Carried {
    /// Whether the receiver has everything the sender will ever send.
    fn delivered(&self) -> bool {
        self.ended && self.collected == self.frames
    }
}

/// What a relay shows the coordinator to be paid for `bytes` bytes delivered.
pub struct Claim {
    pub channel: ContentHash,
    pub bytes: u64,
    /// The sender's header for the newest frame delivered.
    pub header: Envelope,
    /// The receiver's signed word that it has that frame.
    pub receipt: Envelope,
}

/// The channels this node carries.
#[derive(Default)]
pub struct Relay {
    channels: Mutex<HashMap<ContentHash, Carried>>,
}

impl Relay {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Notes the channels the coordinator has asked this node to carry, and
    /// forgets those that are over.
    pub fn expect(&self, orders: &[RelayOrder], now: u64) {
        let mut channels = self.channels.lock().expect("relay lock");
        for order in orders {
            channels.entry(order.channel).or_insert_with(|| Carried {
                order: order.clone(),
                waiting: VecDeque::new(),
                collected: 0,
                frames: 0,
                bytes: 0,
                ended: false,
                delivery: None,
                claimed: 0,
            });
        }
        channels.retain(|_, carried| {
            let settled = carried.delivered() && carried.claimed == carried.bytes;
            let lapsed = now >= carried.order.until + REDEEM_GRACE_SECS;
            !settled && !lapsed
        });
    }

    /// The pay this node can claim now: for what receivers have signed for since
    /// it last claimed.
    pub fn claims(&self) -> Vec<Claim> {
        let channels = self.channels.lock().expect("relay lock");
        channels
            .iter()
            .filter_map(|(channel, carried)| {
                let delivery = carried.delivery.clone()?;
                (delivery.bytes > carried.claimed).then_some(Claim {
                    channel: *channel,
                    bytes: delivery.bytes,
                    header: delivery.header,
                    receipt: delivery.receipt,
                })
            })
            .collect()
    }

    /// Notes that a claim has been answered, so it is not made again.
    pub fn claimed(&self, claim: &Claim) {
        let mut channels = self.channels.lock().expect("relay lock");
        if let Some(carried) = channels.get_mut(&claim.channel) {
            carried.claimed = carried.claimed.max(claim.bytes);
        }
    }

    /// Takes in a frame from a channel's sender.
    fn accept(&self, channel: &ContentHash, frame: Frame, now: u64) -> Result<(), Refusal> {
        let mut channels = self.channels.lock().expect("relay lock");
        let carried = channels.get_mut(channel).ok_or(NOT_ASKED)?;
        if now >= carried.order.until {
            return Err((REFUSED, "the channel has closed".into()));
        }
        let checked = frame
            .check(channel, &carried.order.from, carried.frames, carried.bytes)
            .map_err(|error| (REFUSED, error.to_string()))?;
        let Checked::Next(header) = checked else {
            return Ok(());
        };
        if carried.ended {
            return Err((REFUSED, "the channel has ended".into()));
        }
        if header.bytes > carried.order.bytes {
            return Err((REFUSED, "more than the channel was opened for".into()));
        }
        let waiting: usize = carried
            .waiting
            .iter()
            .map(|(frame, _)| frame.data.0.len())
            .sum();
        if waiting + frame.data.0.len() > BUFFER_BYTES {
            let message = "the receiver has not collected what was already sent";
            return Err((WAIT, message.into()));
        }

        carried.frames += 1;
        carried.bytes = header.bytes;
        carried.ended = header.end;
        carried.waiting.push_back((frame, header.bytes));
        Ok(())
    }

    /// Hands a channel's receiver the frames waiting for it.
    fn take(&self, channel: &ContentHash, asked: &Envelope, now: u64) -> Result<Taken, Refusal> {
        let take: Take = asked.open().map_err(|error| (REFUSED, error.to_string()))?;
        if take.channel != *channel || now.abs_diff(take.issued_at) > MAX_CLOCK_SKEW_SECS {
            return Err((
                REFUSED,
                "the request is stale or for another channel".into(),
            ));
        }
        let mut channels = self.channels.lock().expect("relay lock");
        let carried = channels.get_mut(channel).ok_or(NOT_ASKED)?;
        if asked.signer != carried.order.to {
            return Err((REFUSED, "only the receiving device can collect".into()));
        }

        while carried.collected < take.after.min(carried.frames) {
            let Some((frame, bytes)) = carried.waiting.pop_front() else {
                break;
            };
            carried.collected += 1;
            carried.delivery = Some(Delivery {
                bytes,
                header: frame.header,
                receipt: asked.clone(),
            });
        }
        let mut frames = Vec::new();
        let mut size = 0;
        for (frame, _) in &carried.waiting {
            size += frame.data.0.len();
            if !frames.is_empty() && size > TAKE_BYTES {
                break;
            }
            frames.push(frame.clone());
        }
        Ok(Taken { frames })
    }
}

/// Why a relay did not do what it was asked: an HTTP status and a reason.
type Refusal = (u16, String);

const NOT_ASKED: Refusal = (NOT_KNOWN, String::new());

/// Answers a request to this node's server about the channel named in `path`,
/// which is the rest of the address after `CHANNEL_PATH`.
pub(crate) fn answer(relay: &Relay, path: &str, mut request: Request) -> io::Result<()> {
    let named = path.split_once('/').and_then(|(channel, action)| {
        let channel: ContentHash = channel.parse().ok()?;
        Some((channel, action))
    });
    let Some((channel, action)) = named.filter(|_| *request.method() == Method::Post) else {
        return request.respond(Response::from_string("no such address").with_status_code(404));
    };
    let body = request.as_reader().take(MAX_REQUEST_BYTES);
    let now = unix_now();
    let outcome = match action {
        FRAMES => serde_json::from_reader(body)
            .map_err(|_| (REFUSED, "not a frame".to_string()))
            .and_then(|frame| relay.accept(&channel, frame, now))
            .map(|()| None),
        TAKE => serde_json::from_reader(body)
            .map_err(|_| (REFUSED, "not a signed request".to_string()))
            .and_then(|asked| relay.take(&channel, &asked, now))
            .map(Some),
        _ => Err((NOT_KNOWN, String::new())),
    };
    match outcome {
        Ok(None) => request.respond(Response::empty(204)),
        Ok(Some(taken)) => {
            let body = serde_json::to_vec(&taken).expect("frames serialize");
            request.respond(Response::from_data(body))
        }
        Err((status, reason)) => {
            request.respond(Response::from_string(reason).with_status_code(status))
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TransferError {
    #[error(transparent)]
    Coordinator(#[from] ClientError),
    #[error("could not reach the relay: {0}")]
    Unreachable(String),
    #[error("the relay refused: {0}")]
    Refused(String),
    #[error("the relay was not ready to carry the channel in time")]
    RelayNotReady,
    #[error("the relay handed over something the sender did not send: {0}")]
    Tampered(String),
    #[error("the sender stopped before the end")]
    Incomplete,
    #[error("the receiving device has not published a key to seal to")]
    NoSealingKey,
    #[error("what arrived was not sealed to this device")]
    NotForThisDevice,
    #[error("no node took the data to hold")]
    NotStored,
}

/// Sends `data` to the device `to` through a relay node, sealed so that only
/// that device can read it. Returns the channel it went over once the relay has
/// all of it; the receiver collects it from there.
pub fn send(client: &Client, to: PublicKey, data: &[u8]) -> Result<Route, TransferError> {
    let sealing_key = client.sealing_key(to)?.ok_or(TransferError::NoSealingKey)?;
    // Each frame is sealed on its own, so it carries a little less than a frame.
    let mut pieces: Vec<&[u8]> = data.chunks(FRAME_BYTES - SEAL_OVERHEAD_BYTES).collect();
    if pieces.is_empty() {
        pieces.push(&[]);
    }
    let sealed_bytes = data.len() + pieces.len() * SEAL_OVERHEAD_BYTES;
    let route = client.open_relay(to, sealed_bytes as u64)?;
    let address = format!("{}{CHANNEL_PATH}{}/{FRAMES}", route.address, route.channel);
    let http = peer::agent();

    let last = pieces.len() - 1;
    let mut sent = 0;
    for (seq, piece) in pieces.into_iter().enumerate() {
        let sealed = seal::seal(&sealing_key, piece).ok_or(TransferError::NoSealingKey)?;
        let carried = sealed.len() as u64;
        let frame = Frame::signed(
            client.key(),
            route.channel,
            seq as u64,
            sent,
            sealed,
            seq == last,
        );
        offer(&http, &address, &frame)?;
        sent += carried;
    }
    Ok(route)
}

/// Offers a frame to a relay until it takes it. A relay is not ready for a
/// channel until it has heard of it from the coordinator, which it does at its
/// next check-in, and it makes a sender wait while the receiver is behind.
fn offer(http: &ureq::Agent, address: &str, frame: &Frame) -> Result<(), TransferError> {
    for _ in 0..SEND_ATTEMPTS {
        let response = http
            .post(address)
            .send_json(frame)
            .map_err(|error| TransferError::Unreachable(error.to_string()))?;
        match response.status().as_u16() {
            NOT_KNOWN | WAIT => sleep(SEND_RETRY),
            status if (200..300).contains(&status) => return Ok(()),
            _ => {
                let reason = response.into_body().read_to_string().unwrap_or_default();
                return Err(TransferError::Refused(reason));
            }
        }
    }
    Err(TransferError::RelayNotReady)
}

/// Collects everything sent on a channel addressed to this device, checking each
/// frame against the sender's signature and opening it with this device's
/// opening key. Returns `None` if nothing is waiting on the channel, which is
/// the case for one already collected.
pub fn receive(
    client: &Client,
    opening_key: &OpeningKey,
    incoming: &Incoming,
) -> Result<Option<Vec<u8>>, TransferError> {
    let Incoming {
        channel,
        from,
        address,
    } = incoming;
    let address = format!("{address}{CHANNEL_PATH}{channel}/{TAKE}");
    let http = peer::agent();

    let mut data = Vec::new();
    let mut frames = 0;
    // How many bytes the frames so far carried, which is more than they held.
    let mut carried = 0;
    let mut idle = 0;
    loop {
        let take = Take {
            channel: *channel,
            after: frames,
            issued_at: unix_now(),
        };
        let taken = ask(&http, &address, &Envelope::seal(client.key(), &take))?;
        let Some(taken) = taken.filter(|taken| !taken.frames.is_empty()) else {
            if frames == 0 {
                return Ok(None);
            }
            idle += 1;
            if idle >= TAKE_ATTEMPTS {
                return Err(TransferError::Incomplete);
            }
            sleep(TAKE_POLL);
            continue;
        };
        idle = 0;
        for frame in taken.frames {
            let checked = frame
                .check(channel, from, frames, carried)
                .map_err(|error| TransferError::Tampered(error.to_string()))?;
            let Checked::Next(header) = checked else {
                continue;
            };
            let opened = seal::open(opening_key, &frame.data.0);
            data.extend(opened.ok_or(TransferError::NotForThisDevice)?);
            carried = header.bytes;
            frames += 1;
            if header.end {
                // Sign for the last frames, so the relay can be paid for them and
                // forget the channel. The data is whole whether or not this gets
                // through.
                let done = Take {
                    after: frames,
                    ..take
                };
                let _ = ask(&http, &address, &Envelope::seal(client.key(), &done));
                return Ok(Some(data));
            }
        }
    }
}

/// Asks a relay for frames. `None` means the relay does not know the channel.
fn ask(http: &ureq::Agent, address: &str, take: &Envelope) -> Result<Option<Taken>, TransferError> {
    let unreachable = |error: ureq::Error| TransferError::Unreachable(error.to_string());
    let mut response = http.post(address).send_json(take).map_err(unreachable)?;
    match response.status().as_u16() {
        NOT_KNOWN => Ok(None),
        status if (200..300).contains(&status) => {
            let body = response.body_mut().with_config().limit(MAX_TAKEN_BYTES);
            body.read_json().map(Some).map_err(unreachable)
        }
        _ => {
            let reason = response.into_body().read_to_string().unwrap_or_default();
            Err(TransferError::Refused(reason))
        }
    }
}
