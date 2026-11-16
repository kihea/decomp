//! The DeComp coordinator: keeps account records and balances, stores blobs, hands
//! jobs to nodes, checks their results, referees disputes, and keeps node scores.
//! Settlement on a public ledger joins it in a later build step.
//!
//! Configuration, by environment variable:
//! - `DECOMP_ADDR`: address to listen on (default `127.0.0.1:8787`)
//! - `DECOMP_DATA_DIR`: directory for logs and blobs (default `data`)

mod api;
mod blobs;
mod log;
mod referee;
mod state;

use std::env;
use std::error::Error;
use std::path::Path;

use tokio::net::TcpListener;

use crate::state::Coordinator;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let addr = env::var("DECOMP_ADDR").unwrap_or_else(|_| "127.0.0.1:8787".into());
    let data_dir = env::var("DECOMP_DATA_DIR").unwrap_or_else(|_| "data".into());

    let coordinator = Coordinator::open(Path::new(&data_dir))?;
    let listener = TcpListener::bind(&addr).await?;
    println!("coordinator listening on http://{addr}, data in {data_dir}/");
    axum::serve(listener, api::router(coordinator)).await?;
    Ok(())
}
