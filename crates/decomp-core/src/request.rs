//! Requests a device sends to the coordinator and the shapes of its answers.
//! Changes to an account are not here: those are `AccountOp`s, which are logged.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::account::Account;
use crate::envelope::Envelope;
use crate::job::{Assignment, JobResult, Manifest};
use crate::keys::{AccountId, ContentHash, JobId, PublicKey, SealingKey};
use crate::ledger::{Balance, CashOut, Credits};
use crate::relay::{Incoming, RelayOrder};
use crate::settlement::{Change, Through};
use crate::storage::{Challenge, StoreOrder, Waiting};

/// The largest request or answer either side accepts. A blob travels inside one
/// as base64, so this allows blobs of up to about 48 MB.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

/// How far a signed request's `issued_at` may be from the clock of whoever
/// receives it. This bounds how long a captured request can be replayed.
pub const MAX_CLOCK_SKEW_SECS: u64 = 300;

/// A signed request. `issued_at` (seconds since 1970) must be close to the
/// coordinator's clock, which bounds how long a captured request can be replayed.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub issued_at: u64,
    pub call: Call,
}

/// What a request asks for. The comment on each variant names the answer's type.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Call {
    /// The account the signer belongs to. Answer: `AccountStatus`.
    GetAccount,
    /// Stores bytes and names them by their hash. Answer: `BlobRef`.
    PutBlob { data: Blob },
    /// Answer: `BlobData`.
    GetBlob { hash: ContentHash },
    /// Submits a job, billed to the signer's account. Answer: `Submitted`.
    SubmitJob { manifest: Manifest },
    /// Answer: `JobStatus`.
    GetJob { job_id: JobId },
    /// The contents a shard of an app's state has now, to be fetched as a blob
    /// and read as records (see `state`). Answer: `ShardNow`.
    GetState { app: AccountId, shard: String },
    /// Puts earned credits up as stake behind the account's nodes.
    /// Answer: `AccountStatus`.
    Stake { amount: Credits },
    /// Takes stake back. Refused while one of the account's nodes is on a job.
    /// Answer: `AccountStatus`.
    Unstake { amount: Credits },
    /// How far the signing node is trusted. Answer: `Standing`.
    GetStanding,
    /// A node reporting that it is online and asking for its work. `address` is
    /// where other devices can connect to it, if they can; a node that gives one
    /// may be asked to hold data. Answer: `CheckedIn`.
    CheckIn {
        #[serde(default)]
        address: Option<String>,
    },
    /// Pays to have a blob held by nodes for `days` days. The blob is named by its
    /// hash-tree root (see `merkle`) and is then uploaded to the nodes in the
    /// answer. With `to`, the blob is left for that device, which is told it is
    /// waiting when it asks what is addressed to it. Answer: `Placement`.
    StoreBlob {
        root: ContentHash,
        size: u64,
        days: u32,
        #[serde(default)]
        to: Option<PublicKey>,
    },
    /// A node saying it has received a blob it was asked to hold. Answer: an empty
    /// object.
    ConfirmStored { root: ContentHash },
    /// Where a stored blob can be fetched from. Answer: `Located`.
    LocateBlob { root: ContentHash },
    /// A node answering a storage challenge with the chunk asked for and the
    /// hashes that link it to the blob's root. Answer: an empty object.
    AnswerChallenge {
        challenge: Challenge,
        chunk: Blob,
        proof: Vec<ContentHash>,
    },
    /// A device publishing the key that data for it should be sealed to (see
    /// `seal`). The signed request itself is kept, and handed to whoever asks, as
    /// proof that the device chose the key. Answer: an empty object.
    PublishSealingKey { key: SealingKey },
    /// The sealing key a device has published. Answer: `PublishedKey`.
    GetSealingKey { device: PublicKey },
    /// Opens a channel to the device `to` through a relay node, for up to `bytes`
    /// bytes, and sets the fee for that much aside (see `relay`). Answer: `Route`.
    OpenRelay { to: PublicKey, bytes: u64 },
    /// What is addressed to the signing device: open channels, and stored blobs
    /// left for it. Asking is also how a device shows that it is waiting to
    /// receive. Answer: `IncomingChannels`.
    ListIncoming,
    /// A relay node claiming its pay: a frame header from a channel's sender, and
    /// a `Take` in which the receiver signed for that frame. Answer: an empty
    /// object.
    RedeemRelay { header: Envelope, receipt: Envelope },
    /// A node reporting what a job produced. Answer: an empty object.
    SubmitResult { result: JobResult },
    /// The coordinator's operator recording that `account` bought `amount`
    /// credits with the payment named `payment` (spec section 5: "Credits can
    /// be bought and cashed out for money"). Only the operator's own key is
    /// answered. Answer: an empty object.
    CreditPurchase {
        account: AccountId,
        amount: Credits,
        payment: String,
    },
    /// Asks for earned credits to be cashed out, and sets them aside. Signed
    /// by the account key itself, not a device (spec section 1: the account
    /// key signs "cash-outs"). Answer: `CashOutAsked`.
    CashOut { amount: Credits },
    /// The cash-outs that wait to be paid or refused. Only the operator's own
    /// key is answered. Answer: `CashOuts`.
    GetCashOuts,
    /// The operator recording that a cash-out was paid, by the payment named
    /// `payout`: its credits are gone for good. Answer: an empty object.
    PayCashOut { id: ContentHash, payout: String },
    /// The operator refusing a cash-out: its credits go back to the account.
    /// Answer: an empty object.
    RefuseCashOut { id: ContentHash },
    /// The settlement log from entry number `from` on, counting from zero, at
    /// most `MAX_LOG_PAGE` entries of it (see `settlement`). Answer: `LogPage`.
    GetLog { from: u64 },
    /// Proof that the receipt of a job is in a batch. Whoever knows a job's ID
    /// may ask: its submitter and the nodes that ran it. Answer: `Inclusion`.
    GetInclusion { job_id: JobId },
    /// The draws the operator has published, from number `from` on, counting
    /// from zero, at most `MAX_LOG_PAGE` of them. Each is a `Draw` signed by
    /// the operator (see `draw`). Answer: `LogPage`.
    GetDraws { from: u64 },
    /// An operator asking for the records behind another operator's batches,
    /// from the places `from` gives, at most `MAX_LOG_PAGE` of each kind.
    /// Account records are not public, so only an operator is answered: a
    /// device of an account that has stake. Answer: `BatchRecords`.
    GetRecords { from: Through },
    /// An operator disputing a batch it has audited: `fingerprint` and
    /// `changes` are what the records behind batch number `batch` give. If the
    /// keeper of the log finds the same, the signed request goes into the log
    /// and the operator that posted the batch forfeits its stake. Answer: an
    /// empty object.
    DisputeBatch {
        batch: u64,
        fingerprint: ContentHash,
        changes: Vec<Change>,
    },
}

/// An account record together with its credits. Changing an account answers
/// with this too.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AccountStatus {
    #[serde(flatten)]
    pub account: Account,
    pub balance: Balance,
}

/// The name a cash-out was given, to pay or refuse it by.
#[derive(Debug, Serialize, Deserialize)]
pub struct CashOutAsked {
    pub id: ContentHash,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CashOuts {
    pub waiting: Vec<CashOut>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BlobRef {
    pub hash: ContentHash,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BlobData {
    pub data: Blob,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ShardNow {
    /// The hash of the shard's contents, or nothing if the shard is empty.
    pub contents: Option<ContentHash>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Submitted {
    pub job_id: JobId,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CheckedIn {
    /// The job the node should be running, if any.
    pub assignment: Option<Assignment>,
    /// Blobs the node has been asked to hold and has not yet confirmed.
    #[serde(default)]
    pub store: Vec<StoreOrder>,
    /// A storage challenge to answer.
    #[serde(default)]
    pub challenge: Option<Challenge>,
    /// The open channels the node is asked to carry.
    #[serde(default)]
    pub relay: Vec<RelayOrder>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PublishedKey {
    /// The device's signed request publishing its key, to be read with
    /// `seal::published_key`, or nothing if it has published none.
    pub proof: Option<Envelope>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct IncomingChannels {
    pub channels: Vec<Incoming>,
    /// Stored blobs left for the device.
    #[serde(default)]
    pub stored: Vec<Waiting>,
}

/// The nodes a blob should be uploaded to.
#[derive(Debug, Serialize, Deserialize)]
pub struct Placement {
    pub addresses: Vec<String>,
}

/// The body of every error answer. Programs branch on `error`, a stable code;
/// `message` is for people.
#[derive(Debug, Serialize, Deserialize)]
pub struct Refusal {
    pub error: String,
    pub message: String,
}

/// Raw bytes, which travel as base64 text inside JSON.
#[derive(Clone, PartialEq, Eq)]
pub struct Blob(pub Vec<u8>);

impl std::fmt::Debug for Blob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Blob({} bytes)", self.0.len())
    }
}

impl Serialize for Blob {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&BASE64.encode(&self.0))
    }
}

impl<'de> Deserialize<'de> for Blob {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        BASE64
            .decode(text)
            .map(Blob)
            .map_err(serde::de::Error::custom)
    }
}
