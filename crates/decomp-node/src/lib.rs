//! What a DeComp node does, shared by its two front ends: the `decomp-node`
//! command and the desktop app. Each front end decides where the device key is
//! kept and when to work; this crate talks to the coordinator and runs the jobs.

pub mod client;
mod peer;
pub mod relay;
pub mod server;
pub mod store;
mod work;

pub use work::{IDLE_POLL, RETRY_DELAY, work};
