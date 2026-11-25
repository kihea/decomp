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
//! It installs the token's ICRC ledger beside it (`canisters/token`), with
//! the control canister as its only minter.
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
//! - `DECOMP_REVIEWERS`, `DECOMP_TREASURERS`, `DECOMP_RESERVED_NAMES`: as for
//!   the devnet

use std::env;
use std::error::Error;
use std::thread;
use std::time::Duration;

use candid::{CandidType, Nat, Principal, encode_args, encode_one};
use decomp_core::{ICRC_TRANSFER_FEE, TOKEN_DECIMALS, TOKEN_NAME, TOKEN_SYMBOL};
use icrc_ledger_types::icrc::generic_metadata_value::MetadataValue;
use icrc_ledger_types::icrc1::account::Account;
use pocket_ic::PocketIcBuilder;
use serde::Deserialize;

/// The control canister's install argument (`canisters/control/control.did`).
#[derive(CandidType)]
struct Settings {
    reviewers: Vec<String>,
    treasurers: Vec<String>,
    reserved_names: Vec<String>,
    recovery_wait_secs: Option<u64>,
    token_ledger: Option<Principal>,
}

/// The token's ICRC ledger, as DFINITY publishes it (`canisters/token`).
const TOKEN_LEDGER_WASM: &[u8] = include_bytes!("../../../canisters/token/ic-icrc1-ledger.wasm.gz");

/// The ICRC ledger's install argument (`canisters/token/ledger.did`).
#[derive(CandidType)]
enum LedgerArg {
    Init(LedgerInit),
}

#[derive(CandidType)]
struct LedgerInit {
    minting_account: Account,
    fee_collector_account: Option<Account>,
    transfer_fee: Nat,
    decimals: Option<u8>,
    max_memo_length: Option<u16>,
    token_symbol: String,
    token_name: String,
    metadata: Vec<(String, MetadataValue)>,
    initial_balances: Vec<(Account, Nat)>,
    feature_flags: Option<FeatureFlags>,
    archive_options: ArchiveOptions,
    index_principal: Option<Principal>,
}

#[derive(CandidType)]
struct FeatureFlags {
    icrc2: bool,
}

#[derive(CandidType)]
struct ArchiveOptions {
    num_blocks_to_archive: u64,
    max_transactions_per_response: Option<u64>,
    trigger_threshold: u64,
    max_message_size_bytes: Option<u64>,
    cycles_for_archive_creation: Option<u64>,
    node_max_memory_size_bytes: Option<u64>,
    controller_id: Principal,
    more_controller_ids: Option<Vec<Principal>>,
}

/// How many blocks the ICRC ledger holds before it moves the oldest to an
/// archive canister, and how many it moves at a time: the values DFINITY's
/// own ledgers start with.
const ARCHIVE_TRIGGER_BLOCKS: u64 = 2_000;
const ARCHIVE_BATCH_BLOCKS: u64 = 1_000;

/// The longest memo the ICRC ledger takes: a withdrawal's name.
const MAX_MEMO_BYTES: u16 = 32;

/// Installs the token's ICRC ledger with `minter` as its only minting
/// account and no balances.
fn token_ledger_init(minter: Principal) -> LedgerArg {
    LedgerArg::Init(LedgerInit {
        minting_account: Account {
            owner: minter,
            subaccount: None,
        },
        fee_collector_account: None,
        transfer_fee: Nat::from(ICRC_TRANSFER_FEE.e8s()),
        decimals: Some(TOKEN_DECIMALS),
        max_memo_length: Some(MAX_MEMO_BYTES),
        token_symbol: TOKEN_SYMBOL.into(),
        token_name: TOKEN_NAME.into(),
        metadata: Vec::new(),
        initial_balances: Vec::new(),
        feature_flags: Some(FeatureFlags { icrc2: true }),
        archive_options: ArchiveOptions {
            num_blocks_to_archive: ARCHIVE_BATCH_BLOCKS,
            max_transactions_per_response: None,
            trigger_threshold: ARCHIVE_TRIGGER_BLOCKS,
            max_message_size_bytes: None,
            cycles_for_archive_creation: None,
            node_max_memory_size_bytes: None,
            // The local replica's own caller, which installed the ledger.
            controller_id: Principal::anonymous(),
            more_controller_ids: None,
        },
        index_principal: None,
    })
}

/// How often the canister is asked whether it has started.
const READY_POLL: Duration = Duration::from_millis(500);

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

/// A gateway the PocketIC server is running, as its `/http_gateway` lists it.
#[derive(Deserialize)]
struct Gateway {
    instance_id: u64,
    port: u16,
    forward_to: Forward,
}

#[derive(Deserialize)]
enum Forward {
    PocketIcInstance(u64),
    #[serde(other)]
    Elsewhere,
}

/// Stops a gateway left on `port` by an earlier run, and deletes the
/// instance behind it. The server keeps both after the process that made them
/// is gone, and the gateway would hold the port.
fn clear_port(server: &str, port: u16) -> Result<(), Box<dyn Error>> {
    let gateways: Vec<Gateway> = ureq::get(format!("{server}/http_gateway"))
        .call()?
        .body_mut()
        .read_json()?;
    for gateway in gateways.iter().filter(|gateway| gateway.port == port) {
        ureq::post(format!(
            "{server}/http_gateway/{}/stop",
            gateway.instance_id
        ))
        .send_empty()?;
        if let Forward::PocketIcInstance(instance) = gateway.forward_to {
            ureq::delete(format!("{server}/instances/{instance}")).call()?;
        }
        println!("stopped the gateway an earlier run left on port {port}");
    }
    Ok(())
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

    clear_port(server.trim_end_matches('/'), port)?;
    let mut replica = PocketIcBuilder::new()
        .with_server_url(server.parse()?)
        .with_application_subnet()
        // The subnet that holds the threshold keys (vetKD, Schnorr), as on
        // the network.
        .with_ii_subnet()
        .build();
    let canister = replica.create_canister();
    replica.add_cycles(canister, CYCLES);
    let token_ledger = replica.create_canister();
    replica.add_cycles(token_ledger, CYCLES);
    replica.install_canister(
        token_ledger,
        TOKEN_LEDGER_WASM.to_vec(),
        encode_one(token_ledger_init(canister))?,
        None,
    );
    let settings = Settings {
        reviewers: list("DECOMP_REVIEWERS"),
        treasurers: list("DECOMP_TREASURERS"),
        reserved_names: list("DECOMP_RESERVED_NAMES"),
        recovery_wait_secs: None,
        token_ledger: Some(token_ledger),
    };
    replica.install_canister(canister, wasm, encode_one(Some(settings))?, None);
    let gateway = replica.make_live(Some(port));
    let port = gateway.port().unwrap_or(port);

    // The canister answers once it has fetched its signing key, seconds after
    // it is installed; until then it refuses every call as `starting`.
    let probe = format!("http://localhost:{port}/v1/request?canisterId={canister}");
    let starting = || {
        ureq::post(&probe)
            .config()
            .http_status_as_error(false)
            .build()
            .send("{}")
            .ok()
            .and_then(|mut answer| answer.body_mut().read_to_string().ok())
            .is_none_or(|body| body.contains("\"starting\""))
    };
    while starting() {
        thread::sleep(READY_POLL);
    }

    // The gateway finds the canister by the query as well as by the host
    // name; a `*.localhost` name does not resolve everywhere, the query does.
    println!("control canister {canister}");
    println!("token ledger     {token_ledger} (ICRC-1, ICRC-2, ICRC-3)");
    println!("control plane at http://localhost:{port}?canisterId={canister}");
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
