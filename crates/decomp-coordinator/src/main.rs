//! The DeComp coordinator: keeps account records and balances, stores blobs, hands
//! jobs to nodes, checks their results, referees disputes, keeps node scores, and
//! posts batches of receipts to a settlement log its operator signs.
//!
//! Configuration, by environment variable:
//! - `DECOMP_ADDR`: address to listen on (default `127.0.0.1:8787`)
//! - `DECOMP_DATA_DIR`: directory for logs and blobs (default `data`)
//! - `DECOMP_STORAGE_CHALLENGE_SECS`: how long a stored blob goes between
//!   challenges (default one hour)
//! - `DECOMP_BATCH_SECS`: how often a batch is posted to the settlement log
//!   (default ten minutes)

mod api;
mod blobs;
mod log;
mod operator;
mod referee;
mod state;

use std::env;
use std::error::Error;
use std::path::Path;
use std::time::Duration;

use decomp_core::BATCH_PERIOD_SECS;
use tokio::net::TcpListener;

use crate::state::Coordinator;

/// How long a stored blob goes between challenges unless set otherwise. Shorter
/// catches a node that dropped a blob sooner; longer costs nodes less traffic.
const DEFAULT_CHALLENGE_INTERVAL_SECS: u64 = 3600;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let addr = env::var("DECOMP_ADDR").unwrap_or_else(|_| "127.0.0.1:8787".into());
    let data_dir = env::var("DECOMP_DATA_DIR").unwrap_or_else(|_| "data".into());

    let challenge_interval = match env::var("DECOMP_STORAGE_CHALLENGE_SECS") {
        Ok(seconds) => seconds.parse()?,
        Err(_) => DEFAULT_CHALLENGE_INTERVAL_SECS,
    };

    let batch_period = match env::var("DECOMP_BATCH_SECS") {
        Ok(seconds) => seconds.parse()?,
        Err(_) => BATCH_PERIOD_SECS,
    };

    let coordinator = Coordinator::open(Path::new(&data_dir), challenge_interval)?;
    let listener = TcpListener::bind(&addr).await?;
    println!("coordinator listening on http://{addr}, data in {data_dir}/");
    println!("operator key: {}", coordinator.operator());
    let batch_period = Duration::from_secs(batch_period);
    axum::serve(listener, api::router(coordinator, batch_period)).await?;
    Ok(())
}
