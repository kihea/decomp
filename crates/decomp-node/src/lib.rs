//! What a DeComp node does, shared by its two front ends: the `decomp-node`
//! command and the desktop app. Each front end decides where the device key is
//! kept and when to work; this crate talks to the control plane and runs the jobs.

pub mod beacon;
pub mod client;
pub mod deliver;
pub mod hardware;
pub mod models;
mod peer;
pub mod relay;
pub mod schedule;
pub mod server;
pub mod store;
mod work;

pub use peer::measure;
pub use work::{Committed, IDLE_POLL, RETRY_DELAY, reveal, work};
