//! `decomp-replica`: runs the control-plane canister on a local Internet
//! Computer replica and serves it through an HTTP gateway, for development
//! and testing (spec section 6: "During development and testing, a local
//! replica of the canisters stands in for the network").
//!
//! It drives a PocketIC server, the replica the Internet Computer's tools
//! use, which runs on Linux and macOS (on Windows, inside WSL):
//!
//! ```text
//! pocket-ic --port 47200 --ttl 86400
//! ```
//!
//! then, from anywhere that can reach that port:
//!
//! ```text
//! cargo build --release --target wasm32-unknown-unknown -p decomp-control-canister
//! cargo run -p decomp-replica
//! ```
//!
//! It prints the address the canister answers at, such as
//! `http://localhost:4943?canisterId=lxzze-o7777-77777-aaaaa-cai`, which is
//! what nodes, apps, and the launcher take as the control plane's address. When the canister's
//! module is rebuilt, it upgrades the canister in place, keeping its state.
//!
//! Configuration, by environment variable:
//! - `POCKET_IC_URL`: the PocketIC server (default `http://127.0.0.1:47200`)
//! - `DECOMP_CANISTER_WASM`: the control canister's module (default the
//!   release build in `target/`)
//! - `DECOMP_GATEWAY_PORT`: the HTTP gateway's port (default 4943)
//! - `DECOMP_REVIEWERS`, `DECOMP_RESERVED_NAMES`: as for the devnet

use std::env;
use std::error::Error;
use std::thread;
use std::time::Duration;

use candid::{CandidType, encode_args, encode_one};
use pocket_ic::PocketIcBuilder;

/// The control canister's install argument (`canisters/control/control.did`).
#[derive(CandidType)]
struct Settings {
    reviewers: Vec<String>,
    reserved_names: Vec<String>,
    recovery_wait_secs: Option<u64>,
}

/// How often the canister's module file is checked for a new build.
const WATCH_INTERVAL: Duration = Duration::from_secs(2);

/// Cycles the canister is given. A local replica charges cycles as the
/// network does, and this is far more than development needs.
const CYCLES: u128 = 100_000_000_000_000;

fn list(name: &str) -> Vec<String> {
    env::var(name)
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(String::from)
        .collect()
}

fn main() -> Result<(), Box<dyn Error>> {
    let server = env::var("POCKET_IC_URL").unwrap_or_else(|_| "http://127.0.0.1:47200".into());
    let wasm_path = env::var("DECOMP_CANISTER_WASM").unwrap_or_else(|_| {
        "target/wasm32-unknown-unknown/release/decomp_control_canister.wasm".into()
    });
    let port: u16 = match env::var("DECOMP_GATEWAY_PORT") {
        Ok(port) => port.parse()?,
        Err(_) => 4943,
    };
    let read = |path: &str| std::fs::read(path).map_err(|e| format!("{path}: {e}"));
    let wasm = read(&wasm_path)?;
    let mut installed = std::fs::metadata(&wasm_path)?.modified()?;

    let mut replica = PocketIcBuilder::new()
        .with_server_url(server.parse()?)
        .with_application_subnet()
        .build();
    let canister = replica.create_canister();
    replica.add_cycles(canister, CYCLES);
    let settings = Settings {
        reviewers: list("DECOMP_REVIEWERS"),
        reserved_names: list("DECOMP_RESERVED_NAMES"),
        recovery_wait_secs: None,
    };
    replica.install_canister(canister, wasm, encode_one(Some(settings))?, None);
    let gateway = replica.make_live(Some(port));

    // The gateway finds the canister by the query as well as by the host
    // name; a `*.localhost` name does not resolve everywhere, the query does.
    println!("control canister {canister}");
    println!(
        "control plane at http://localhost:{}?canisterId={canister}",
        gateway.port().unwrap_or(port)
    );
    // The replica runs for as long as this process does. A rebuilt module is
    // installed as an upgrade, so the canister keeps its state.
    loop {
        thread::sleep(WATCH_INTERVAL);
        let Ok(modified) = std::fs::metadata(&wasm_path).and_then(|meta| meta.modified()) else {
            continue;
        };
        if modified == installed {
            continue;
        }
        installed = modified;
        // A build may still be writing the file; give it a moment.
        thread::sleep(WATCH_INTERVAL);
        match read(&wasm_path) {
            Ok(wasm) => match replica.upgrade_canister(canister, wasm, encode_args(())?, None) {
                Ok(()) => println!("upgraded the control canister"),
                Err(error) => eprintln!("upgrade refused: {error:?}"),
            },
            Err(error) => eprintln!("{error}"),
        }
    }
}
