//! DeComp protocol rules: keys, signed envelopes, account state, and job state.
//!
//! This crate does no I/O. Services (the coordinator and the node agent) own
//! networking and storage and call into it, so every party applies the same rules.

mod account;
mod app;
mod envelope;
mod hosts;
mod job;
mod jobs;
mod keys;
mod ledger;
pub mod merkle;
mod relay;
mod request;
mod scores;
pub mod seal;
mod splits;
mod state;
mod storage;

pub use account::{Account, AccountOp, Accounts, Device, OpError};
pub use app::{
    AppManifest, MAX_APP_MODULES, MAX_APP_NAME_BYTES, SponsorError, Sponsored, Sponsorship,
};
pub use envelope::{Envelope, EnvelopeError};
pub use hosts::{Host, Hosts};
pub use job::{
    Assignment, Failure, JobResult, JobStatus, MAX_PIECES, MAX_READS, MAX_SPAWNED, MAX_TIER,
    Manifest, Outcome, Receipt, Spawn, Split, UNPROVEN_REDUNDANCY,
};
pub use jobs::{ClosedJob, Closure, Dispute, FollowUps, JobError, Jobs};
pub use keys::{
    AccountId, ContentHash, JobId, OpeningKey, PublicKey, SealingKey, Signature, SigningKey,
};
pub use ledger::{
    BASE_PRICE, Balance, Credits, FeeSplit, Ledger, LedgerEntry, LedgerError,
    PROBATION_FORFEIT_PERCENT, STARTER_GRANT, fee,
};
pub use relay::{
    CHANNEL_SECS, CREDITS_PER_GIB_RELAYED, Checked, FRAME_BYTES, Frame, FrameError, FrameHeader,
    Incoming, MAX_CHANNEL_BYTES, Opening, REDEEM_GRACE_SECS, RelayEntry, RelayError, RelayOrder,
    Relays, Route, Take, Taken, channel_id, relay_fee,
};
pub use request::{
    AccountStatus, Blob, BlobData, BlobRef, Call, CheckedIn, IncomingChannels, MAX_CLOCK_SKEW_SECS,
    MAX_MESSAGE_BYTES, Placement, PublishedKey, Refusal, Request, ShardNow, Submitted,
};
pub use scores::{Conduct, Penalty, ScoreEvent, Scores, Standing};
pub use seal::{SEAL_OVERHEAD_BYTES, SealingKeys};
pub use splits::{Merge, Splits};
pub use state::{MAX_SHARD_NAME_BYTES, ShardRef, StateEntry, States};
pub use storage::{
    Challenge, Located, Storage, StorageEntry, StorageError, StoreOrder, Storing, storage_fee,
};
