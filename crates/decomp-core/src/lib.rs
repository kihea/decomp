//! DeComp protocol rules: keys, signed envelopes, account state, and job state.
//!
//! This crate does no I/O. Services (the coordinator and the node agent) own
//! networking and storage and call into it, so every party applies the same rules.

mod account;
mod app;
mod draw;
pub mod encrypted;
mod envelope;
mod hosts;
mod job;
mod jobs;
mod keys;
mod ledger;
pub mod merkle;
pub mod privacy;
mod relay;
mod request;
mod scores;
pub mod seal;
pub mod settlement;
pub mod shares;
mod splits;
mod state;
mod storage;

pub use account::{Account, AccountOp, Accounts, Device, OpError};
pub use app::{
    AppManifest, MAX_APP_MODULES, MAX_APP_NAME_BYTES, SponsorError, Sponsored, Sponsorship,
};
pub use draw::{
    BEACON_CHAIN, BEACON_MAX_LAG_ROUNDS, BEACON_PERIOD_SECS, Beacon, Draw, DrawError, Eligible,
    SPOT_CHECK_RATE, round_at,
};
pub use encrypted::{EncryptError, Encrypted, EncryptionKey};
pub use envelope::{Envelope, EnvelopeError};
pub use hosts::{Host, Hosts};
pub use job::{
    Assignment, Failure, JobResult, JobStatus, MAX_APART_FROM, MAX_KEPT_FROM, MAX_PIECES,
    MAX_READS, MAX_SPAWNED, MAX_TIER, Manifest, Outcome, Receipt, Share, Spawn, Split,
    UNPROVEN_REDUNDANCY, Verification,
};
pub use jobs::{ClosedJob, Closure, Dispute, FollowUps, JobError, Jobs};
pub use keys::{
    AccountId, ContentHash, JobId, LedgerAddress, OpeningKey, PublicKey, SealingKey, ShareSet,
    Signature, SigningKey,
};
pub use ledger::{
    BASE_PRICE, Balance, Credits, FeeSplit, Holder, Ledger, LedgerEntry, LedgerError,
    PROBATION_FORFEIT_PERCENT, STARTER_GRANT, fee,
};
pub use privacy::{
    ENCRYPTED_COMPUTE_MAX_UNITS, NoPrivateRoute, Privacy, PrivacyError, PrivateRoute,
};
pub use relay::{
    CHANNEL_SECS, CREDITS_PER_GIB_RELAYED, Checked, FRAME_BYTES, Frame, FrameError, FrameHeader,
    Incoming, LISTEN_WINDOW_SECS, MAX_CHANNEL_BYTES, Opening, REDEEM_GRACE_SECS, RelayEntry,
    RelayError, RelayOrder, Relays, Route, Take, Taken, channel_id, relay_fee,
};
pub use request::{
    AccountStatus, Blob, BlobData, BlobRef, Call, CheckedIn, IncomingChannels, MAX_CLOCK_SKEW_SECS,
    MAX_MESSAGE_BYTES, Placement, PublishedKey, Refusal, Request, ShardNow, Submitted,
};
pub use scores::{Conduct, Penalty, ScoreEvent, Scores, Standing};
pub use seal::{SEAL_OVERHEAD_BYTES, SealingKeys};
pub use settlement::{
    BATCH_PERIOD_SECS, Batch, Change, Inclusion, LogEntry, LogError, LogPage, MAX_LOG_PAGE, Posted,
    SettlementLog, Through,
};
pub use shares::{SHARES, ShareError};
pub use splits::{Merge, Splits};
pub use state::{MAX_SHARD_NAME_BYTES, ShardRef, StateEntry, States};
pub use storage::{
    Addressed, Challenge, Located, Storage, StorageEntry, StorageError, StoreOrder, Storing,
    Waiting, storage_fee,
};
