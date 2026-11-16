//! DeComp protocol rules: keys, signed envelopes, account state, and job state.
//!
//! This crate does no I/O. Services (the coordinator and the node agent) own
//! networking and storage and call into it, so every party applies the same rules.

mod account;
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
mod splits;
mod storage;

pub use account::{Account, AccountOp, Accounts, Device, OpError};
pub use envelope::{Envelope, EnvelopeError};
pub use hosts::{Host, Hosts};
pub use job::{
    Assignment, Failure, JobResult, JobStatus, MAX_PIECES, MAX_TIER, Manifest, Outcome, Receipt,
    Split, UNPROVEN_REDUNDANCY,
};
pub use jobs::{ClosedJob, Closure, Dispute, JobError, Jobs};
pub use keys::{AccountId, ContentHash, JobId, PublicKey, Signature, SigningKey};
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
    MAX_MESSAGE_BYTES, Placement, Refusal, Request, Submitted,
};
pub use scores::{Conduct, Penalty, ScoreEvent, Scores, Standing};
pub use splits::{Merge, Splits};
pub use storage::{
    Challenge, Located, Storage, StorageEntry, StorageError, StoreOrder, Storing, storage_fee,
};
