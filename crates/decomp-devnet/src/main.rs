//! `decomp-devnet`: a local DeComp network. It runs the control plane's four
//! services (accounts, registry, job board, and receipts) in one process with
//! the same rules the canisters apply, so nodes, clients, and apps can be
//! developed and tested without the Internet Computer (spec section 6: "During
//! development and testing, a local replica of the canisters stands in for the
//! network").
//!
//! Configuration, by environment variable:
//! - `DECOMP_ADDR`: address to listen on (default `127.0.0.1:8787`)
//! - `DECOMP_DATA_DIR`: directory for logs and blobs (default `data`)
//! - `DECOMP_STORAGE_CHALLENGE_SECS`: how long a stored blob goes between
//!   challenges (default one hour)
//! - `DECOMP_BATCH_SECS`: how often a batch is posted to the settlement log
//!   (default ten minutes)
//! - `DECOMP_RECOVERY_WAIT_SECS`: how long a recovery of an account waits
//!   before it takes effect (default 48 hours)
//! - `DECOMP_REVIEWERS`: account IDs, comma-separated, allowed to review apps
//!   and assign held names (default none)
//! - `DECOMP_RESERVED_NAMES`: names, comma-separated, held for their brands'
//!   owners (default none)

mod blobs;
mod key;
mod log;
mod server;

use std::env;
use std::error::Error;
use std::path::Path;
use std::time::Duration;

use decomp_core::{AccountId, BATCH_PERIOD_SECS, RECOVERY_WAIT_SECS};
use tokio::net::TcpListener;

use decomp_control::{ControlPlane, Settings};

use crate::blobs::DataDir;

/// How long a stored blob goes between challenges unless set otherwise. Shorter
/// catches a node that dropped a blob sooner; longer costs nodes less traffic.
const DEFAULT_CHALLENGE_INTERVAL_SECS: u64 = 3600;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let data_dir = env::var("DECOMP_DATA_DIR").unwrap_or_else(|_| "data".into());
    let addr = env::var("DECOMP_ADDR").unwrap_or_else(|_| "127.0.0.1:8787".into());
    let seconds = |name: &str, default: u64| -> Result<u64, Box<dyn Error>> {
        Ok(match env::var(name) {
            Ok(value) => value.parse()?,
            Err(_) => default,
        })
    };
    let list = |name: &str| -> Vec<String> {
        env::var(name)
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(String::from)
            .collect()
    };
    let reviewers = list("DECOMP_REVIEWERS")
        .iter()
        .map(|id| id.parse::<AccountId>())
        .collect::<Result<Vec<_>, _>>()?;

    let settings = Settings {
        challenge_interval: seconds(
            "DECOMP_STORAGE_CHALLENGE_SECS",
            DEFAULT_CHALLENGE_INTERVAL_SECS,
        )?,
        recovery_wait: seconds("DECOMP_RECOVERY_WAIT_SECS", RECOVERY_WAIT_SECS)?,
        reviewers,
        reserved_names: list("DECOMP_RESERVED_NAMES"),
        entropy: rand::random(),
    };
    let batch_period = Duration::from_secs(seconds("DECOMP_BATCH_SECS", BATCH_PERIOD_SECS)?);

    let control = ControlPlane::open(&mut DataDir::open(Path::new(&data_dir))?, settings)?;
    let listener = TcpListener::bind(&addr).await?;
    println!("decomp devnet listening on http://{addr}, data in {data_dir}/");
    println!("control-plane key (treasury): {}", control.control_key());
    axum::serve(listener, server::router(control, batch_period)).await?;
    Ok(())
}
