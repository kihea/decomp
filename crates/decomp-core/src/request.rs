//! Requests a device sends to the control plane and the shapes of its answers
//! (spec, "Interfaces"). Changes to an account are not here: those are
//! `AccountOp`s, which are logged.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::account::Account;
use crate::app::AppManifest;
use crate::envelope::Envelope;
use crate::job::{JobResult, Lease, Manifest};
use crate::keys::{AccountId, ContentHash, JobId, PublicKey, SealingKey};
use crate::ledger::{Balance, CashOut, Credits};
use crate::market::{Bid, BidOrder, Hardware, Rate, WorkPreferences};
use crate::probe::Probe;
use crate::records::Record;
use crate::registry::NameRecord;
use crate::relay::{Incoming, RelayOrder};
use crate::storage::{Challenge, StoreOrder, Waiting};

/// The largest request or answer either side accepts. A blob travels inside one
/// as base64, so this allows blobs of up to about 48 MB.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

/// How far a signed request's `issued_at` may be from the clock of whoever
/// receives it. This bounds how long a captured request can be replayed.
pub const MAX_CLOCK_SKEW_SECS: u64 = 300;

/// A signed request. `issued_at` (seconds since 1970) must be close to the
/// control plane's clock, which bounds how long a captured request can be
/// replayed.
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
    /// Keeps bytes in the control plane, named by their root (`merkle::root`),
    /// for always-on work: the module, input, and blobs to read of a job with
    /// an always-on fallback, which the control plane runs if no node finishes
    /// it. Everything else is stored on storage nodes (`StoreBlob`), since the
    /// control plane holds hashes, not data. Answer: `BlobRef`.
    PutBlob { data: Blob },
    /// Bytes the control plane keeps: always-on work's, and what jobs that
    /// fell back to always-on produced. Anyone who knows the root may ask.
    /// Answer: `BlobData`.
    GetBlob { hash: ContentHash },
    /// Submits a job, billed to the signer's account. Answer: `Submitted`.
    SubmitJob { manifest: Manifest },
    /// Where a job stands, and its receipt once it has one. Answer: `JobStatus`.
    GetReceipt { job_id: JobId },
    /// What a job would cost if submitted now: the posted rate for its class
    /// and tier, and the most it can cost. Answer: `Quote`.
    QuoteJob { manifest: Manifest },
    /// The contents a shard of an app's state has now, to be fetched as a blob
    /// and read as records (see `state`). Answer: `ShardNow`.
    GetState { app: AccountId, shard: String },
    /// Writes a record of the signer's account: `key` now points to the blob
    /// `value`. `version` is the version the writer read, 0 for a new key; the
    /// write is refused if another has landed since (spec, "Records").
    /// Answer: `Record`.
    WriteRecord {
        key: String,
        version: u64,
        value: ContentHash,
    },
    /// One record of an account, the signer's own if `owner` is left out.
    /// Answer: `Record`.
    GetRecord {
        #[serde(default)]
        owner: Option<AccountId>,
        key: String,
    },
    /// An account's records whose keys start with `prefix`, in order of key,
    /// after `from`. Answer: `RecordList`.
    ListRecords {
        #[serde(default)]
        owner: Option<AccountId>,
        #[serde(default)]
        prefix: String,
        #[serde(default)]
        from: Option<String>,
    },
    /// Puts earned credits up as stake behind the account's nodes.
    /// Answer: `AccountStatus`.
    Stake { amount: Credits },
    /// Takes stake back. Refused while one of the account's nodes is on a job.
    /// Answer: `AccountStatus`.
    Unstake { amount: Credits },
    /// How far the signing node is trusted. Answer: `Standing`.
    GetStanding,
    /// A node asking for work. It is granted leases at random among the nodes
    /// asking, within its owner's work preferences. `address` is where other
    /// devices can connect to it, if they can; a node that gives one may be
    /// asked to hold data and carry relays. Answer: `LeasesGranted`.
    RequestLease {
        #[serde(default)]
        address: Option<String>,
        /// The device's hardware, as the agent read it.
        #[serde(default)]
        hardware: Option<Hardware>,
    },
    /// A node reporting a measurement it was asked to make (see `probe`):
    /// how long fetching the test data took, and the data's hash. Answer: an
    /// empty object.
    ReportProbe {
        seed: ContentHash,
        millis: u64,
        digest: ContentHash,
    },
    /// A node committing the hash of its result for a job it holds a lease on
    /// (see `JobResult::commitment`). Answer: an empty object.
    CommitResult {
        job_id: JobId,
        commitment: ContentHash,
    },
    /// A node revealing the result it committed to, with the salt it committed
    /// with. Answer: an empty object.
    RevealResult {
        result: JobResult,
        salt: ContentHash,
    },
    /// A device's work preferences. Any device of the same account may ask.
    /// Answer: `WorkPreferences`.
    GetWorkPreferences { device: PublicKey },
    /// Sets a device's work preferences. Any device of the same account may set
    /// them. Answer: `WorkPreferences`.
    SetWorkPreferences {
        device: PublicKey,
        preferences: WorkPreferences,
    },
    /// Places a standing bid for the signer's account. Answer: `Bid`.
    PlaceBid { bid: BidOrder },
    /// Withdraws one of the account's bids. Answer: an empty object.
    CancelBid { id: ContentHash },
    /// The account's live bids. Answer: `Bids`.
    ListBids,
    /// Every posted rate. Answer: `Rates`.
    GetRates,
    /// What the account earned and spent since `since`. Answer: `Activity`.
    GetActivity { since: u64 },
    /// Registers a name for the signer's app (see `registry`). The deposit is
    /// set aside from the account. Answer: `NameRecord`.
    RegisterName { name: String },
    /// Starts a name's term again. Answer: `NameRecord`.
    RenewName { name: String },
    /// Returns a name to the pool and its deposit to the account. Answer: an
    /// empty object.
    ReleaseName { name: String },
    /// What a name points to. Anyone may ask. Answer: `Resolved`.
    ResolveName { name: String },
    /// Names in order, after `from`; with `query`, only names that contain it.
    /// Anyone may ask. Answer: `Directory`.
    ListDirectory {
        #[serde(default)]
        verified_only: bool,
        #[serde(default)]
        query: Option<String>,
        #[serde(default)]
        from: Option<String>,
        #[serde(default)]
        limit: Option<u32>,
    },
    /// A reviewer marking an app reviewed or not. Answer: `NameRecord`.
    SetVerified { name: String, verified: bool },
    /// A reviewer assigning a held brand name to the app whose owner proved
    /// they own the brand. Answer: `NameRecord`.
    AssignName { name: String, app: AccountId },
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
    /// Deletes a stored blob the signer's account stored: its nodes drop it,
    /// and what is left of its fee returns. Answer: an empty object.
    DeleteBlob { root: ContentHash },
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
    /// Stores a blob a job produced (its output, a blob it stored, its shard's
    /// new contents, or a follow-up's input), signed by a node that holds a
    /// lease on the job. It is kept for `PRODUCED_RETENTION_DAYS` days, paid by
    /// the job's payer; the node then uploads it to the nodes chosen. Answer:
    /// `Placement`, with no addresses if the blob is already stored.
    StoreProduced {
        job_id: JobId,
        root: ContentHash,
        size: u64,
    },
    /// The sealing key a device has published. Answer: `PublishedKey`.
    GetSealingKey { device: PublicKey },
    /// The key for the signer's account's private values under `label`,
    /// derived by the network's threshold key and encrypted to
    /// `transport_key`, a BLS12-381 key the device made for this request (see
    /// `value_keys`). Only the control canister can answer it. Answer:
    /// `ValueKey`.
    GetValueKey { label: String, transport_key: Blob },
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
    /// The treasury recording that `account` bought `amount` credits with the
    /// payment named `payment` (spec section 5: "Credits can be bought and
    /// cashed out for money"). Only the treasury's key is answered. Answer: an
    /// empty object.
    CreditPurchase {
        account: AccountId,
        amount: Credits,
        payment: String,
    },
    /// Asks for earned credits to be cashed out, and sets them aside. Signed
    /// by the account key itself, not a device (spec section 1: the account
    /// key signs "cash-outs"). Answer: `CashOutAsked`.
    CashOut { amount: Credits },
    /// The cash-outs that wait to be paid or refused. Only the treasury's key
    /// is answered. Answer: `CashOuts`.
    GetCashOuts,
    /// The treasury recording that a cash-out was paid, by the payment named
    /// `payout`: its credits are gone for good. Answer: an empty object.
    PayCashOut { id: ContentHash, payout: String },
    /// The treasury refusing a cash-out: its credits go back to the account.
    /// Answer: an empty object.
    RefuseCashOut { id: ContentHash },
    /// The settlement log from entry number `from` on, counting from zero, at
    /// most `MAX_LOG_PAGE` entries of it (see `settlement`). Answer: `LogPage`.
    GetLog { from: u64 },
    /// Proof that the receipt of a job is in a batch. Whoever knows a job's ID
    /// may ask: its submitter and the nodes that ran it. Answer: `Inclusion`.
    GetInclusion { job_id: JobId },
    /// The draws the job board has published, from number `from` on, counting
    /// from zero, at most `MAX_LOG_PAGE` of them, each a `Draw` as the text it
    /// was published as. The batch of the settlement log that covers a draw
    /// fingerprints it (see `settlement`). Answer: `DrawPage`.
    GetDraws { from: u64 },
}

/// An account record together with its credits. Changing an account answers
/// with this too.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AccountStatus {
    #[serde(flatten)]
    pub account: Account,
    pub balance: Balance,
    /// The combined reliability of the account's devices, from 0 to 1.
    #[serde(default)]
    pub reputation: f64,
    /// When the account first appeared, in seconds since 1970.
    #[serde(default)]
    pub created: u64,
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

/// What a job would cost if submitted now.
#[derive(Debug, Serialize, Deserialize)]
pub struct Quote {
    pub rate: Rate,
    /// What the fallback runs at, per unit, if the job asks for one.
    pub fallback_micros_per_unit: u64,
    /// The most the job (all its pieces, if it is split) can cost: what must
    /// be set aside as `max_fee`.
    pub highest_fee: Credits,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LeasesGranted {
    /// Every lease the node holds and has not committed a result for.
    pub leases: Vec<Lease>,
    /// Jobs whose results the node committed and should reveal now.
    #[serde(default)]
    pub reveal: Vec<JobId>,
    /// Blobs the node has been asked to hold and has not yet confirmed.
    #[serde(default)]
    pub store: Vec<StoreOrder>,
    /// Blobs the node should drop: their deal ended early.
    #[serde(default)]
    pub drop: Vec<ContentHash>,
    /// A storage challenge to answer.
    #[serde(default)]
    pub challenge: Option<Challenge>,
    /// The open channels the node is asked to carry.
    #[serde(default)]
    pub relay: Vec<RelayOrder>,
    /// A measurement of another node's bandwidth to make.
    #[serde(default)]
    pub probe: Option<Probe>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RecordList {
    pub records: Vec<Record>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Bids {
    pub bids: Vec<Bid>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Rates {
    pub rates: Vec<Rate>,
}

/// What an account earned and spent over a stretch of time.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Activity {
    /// Paid to the account for its nodes' work and checks.
    pub earned: Credits,
    /// Paid by the account for jobs and data services.
    pub spent: Credits,
}

/// A name, the app it points to, and that app's manifest.
#[derive(Debug, Serialize, Deserialize)]
pub struct Resolved {
    #[serde(flatten)]
    pub record: NameRecord,
    pub manifest: AppManifest,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Directory {
    pub entries: Vec<Resolved>,
    pub next: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PublishedKey {
    /// The device's signed request publishing its key, to be read with
    /// `seal::published_key`, or nothing if it has published none.
    pub proof: Option<Envelope>,
}

/// A value key, as the threshold key gave it.
#[derive(Debug, Serialize, Deserialize)]
pub struct ValueKey {
    /// The key, encrypted to the request's transport key.
    pub encrypted_key: Blob,
    /// The public key it can be checked against: the threshold key's, derived
    /// for the control canister and the value-key context.
    pub public_key: Blob,
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
