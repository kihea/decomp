//! DeComp protocol rules: keys, signed envelopes, account state, and job state.
//!
//! This crate does no I/O. Services (the coordinator and the node agent) own
//! networking and storage and call into it, so every party applies the same rules.

mod account;
mod app;
pub mod controller;
mod draw;
#[cfg(feature = "encrypted")]
pub mod encrypted;
mod envelope;
mod hosts;
mod job;
mod jobs;
mod keys;
mod ledger;
pub mod market;
pub mod merkle;
pub mod privacy;
pub mod records;
pub mod registry;
mod relay;
mod request;
mod scores;
pub mod seal;
pub mod settlement;
pub mod shares;
mod splits;
mod state;
mod storage;
mod value_keys;

pub use account::{Account, AccountOp, Accounts, Device, OpError, Recovery};
pub use app::{
    AppManifest, MAX_APP_MODULES, MAX_APP_NAME_BYTES, SponsorError, Sponsored, Sponsorship,
};
pub use controller::{
    Approval, Controller, MAX_APPROVALS, MAX_CONTROLLERS, MAX_GUARDIANS, MAX_WAITING_RECOVERIES,
    MOST_METHODS_REQUIRED, RECOVERY_WAIT_SECS,
};
pub use draw::{
    BEACON_CHAIN, BEACON_MAX_LAG_ROUNDS, BEACON_PERIOD_SECS, Beacon, Draw, DrawError, Eligible,
    Pickup, RandomnessSource, SPOT_CHECK_RATE, is_audited, round_at,
};
#[cfg(feature = "encrypted")]
pub use encrypted::{EncryptError, Encrypted, EncryptionKey};
pub use envelope::{Envelope, EnvelopeError};
pub use hosts::{Host, Hosts};
pub use job::{
    Failure, Fallback, JobResult, JobStatus, Lease, MAX_APART_FROM, MAX_JOB_SLOTS, MAX_KEPT_FROM,
    MAX_PIECES, MAX_READS, MAX_RUNS, MAX_SPAWNED, MAX_TIER, Manifest, Outcome, Receipt, Share,
    Spawn, Split, Verification, Verify,
};
pub use jobs::{
    CONFIRMATION_WINDOW_SECS, ClosedJob, Closure, Dispute, FallBackJob, FollowUps, Granted,
    JobError, Jobs, LEASE_SECS, REVEAL_WINDOW_SECS, Standings,
};
pub use keys::{
    AccountId, ContentHash, JobId, LedgerAddress, OpeningKey, PasskeyKey, PublicKey, SealingKey,
    ShareSet, Signature, SigningKey, WalletKey, WalletSignature,
};
pub use ledger::{
    Balance, CashOut, Credits, FeeSplit, HOLDBACK_PERCENT, HOLDBACK_SECS, Holder, Ledger,
    LedgerEntry, LedgerError, MAX_PAYMENT_NAME_BYTES, PROBATION_FORFEIT_PERCENT, Payout,
    STARTER_GRANT, compute_fee,
};
pub use market::{
    AccountStanding, Bid, BidError, BidOrder, JobClass, LeasePrice, Price, Rate, WorkPreferences,
};
pub use privacy::{
    ENCRYPTED_COMPUTE_MAX_UNITS, NoPrivateRoute, Privacy, PrivacyError, PrivateRoute,
};
pub use records::{Record, RecordError, Records};
pub use registry::{NAME_DEPOSIT, NameError, NameRecord, Registry, RegistryEntry};
pub use relay::{
    CHANNEL_SECS, Checked, FRAME_BYTES, Frame, FrameError, FrameHeader, Incoming,
    LISTEN_WINDOW_SECS, MAX_CHANNEL_BYTES, MICROS_PER_GIB_RELAYED, Opening, REDEEM_GRACE_SECS,
    RelayEntry, RelayError, RelayOrder, Relays, Route, Take, Taken, channel_id, relay_fee,
};
pub use request::{
    AccountStatus, Activity, Bids, Blob, BlobData, BlobRef, Call, CashOutAsked, CashOuts,
    Directory, IncomingChannels, LeasesGranted, MAX_CLOCK_SKEW_SECS, MAX_MESSAGE_BYTES, Placement,
    PublishedKey, Quote, Rates, RecordList, Refusal, Request, Resolved, ShardNow, Submitted,
    ValueKey,
};
pub use scores::{Conduct, Penalty, ScoreEvent, Scores, Standing};
pub use seal::{SEAL_OVERHEAD_BYTES, SealingKeys};
pub use settlement::{
    BATCH_PERIOD_SECS, Batch, Change, Inclusion, LogEntry, LogError, LogPage, MAX_LOG_PAGE, Posted,
    SettlementLog, Through, fingerprint,
};
pub use shares::{SHARES, ShareError};
pub use splits::{Merge, Splits};
pub use state::{MAX_SHARD_NAME_BYTES, ShardRef, StateEntry, States};
pub use storage::{
    Addressed, Challenge, Located, Storage, StorageEntry, StorageError, StoreOrder, Storing,
    Waiting, placement_order, storage_fee,
};
pub use value_keys::{MAX_VALUE_KEY_LABEL_BYTES, VALUE_KEY_CONTEXT, value_key_input};
