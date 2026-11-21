//! The DeComp control plane: the accounts, registry, job board, and receipts
//! services, with the rules from `decomp-core`, and the interface clients
//! call. It does no I/O of its own: a host supplies its storage (see
//! `store`), its clock, the beacon's rounds, and the transport, so the same
//! services run on the devnet and in a canister.

pub mod fallback;
pub mod http;
pub mod state;
pub mod store;

pub use http::{Response, StatusCode, ValueKeyRequest, handle, value_key_request};
pub use state::{ControlPlane, Settings};
pub use store::{BlobBackend, Blobs, Log, LogBackend, MemorySnapshot, MemoryStorage, StateStore};
