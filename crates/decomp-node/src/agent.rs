//! The command's two long-running modes: joining an account, and working as a node.

use std::error::Error;
use std::path::Path;
use std::thread::sleep;
use std::time::Duration;

use decomp_core::{Blob, Challenge, CheckedIn, merkle};
use decomp_node::client::{Client, ClientError, unix_now};
use decomp_node::store::{self, Store};
use decomp_node::{IDLE_POLL, RETRY_DELAY, work};
use decomp_sandbox::Sandbox;

use crate::device;

/// How often a device waiting to be approved asks whether it has been.
const LINK_POLL: Duration = Duration::from_secs(3);

/// Makes this device's key if it has none, shows its code, and waits until a
/// device in an account approves it.
pub fn link(dir: &Path, coordinator_url: &str) -> Result<(), Box<dyn Error>> {
    let key = match device::load(dir)? {
        Some(key) => key,
        None => device::create(dir)?,
    };
    let client = Client::new(coordinator_url, key);
    println!("device code: {}", client.device_key());

    let mut waiting = false;
    loop {
        match client.account() {
            Ok(status) => {
                println!("linked to account {}", status.account.id);
                return Ok(());
            }
            Err(error) if error.is("not_linked") => {
                if !waiting {
                    println!("Approve this code from a device already in the account. Waiting...");
                    waiting = true;
                }
                sleep(LINK_POLL);
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/// Where other devices can connect to this node, for a node that holds data.
pub struct Reachable<'a> {
    /// The local address to listen on, such as `0.0.0.0:9000`.
    pub listen: &'a str,
    /// The address other devices use, such as `http://203.0.113.5:9000`.
    pub public_url: &'a str,
}

/// Works as a node until stopped: checks in, runs the job it is given, takes in
/// and vouches for the blobs it is asked to hold, and repeats. A node holds data
/// only if it is given an address other devices can reach it at.
pub fn start(
    dir: &Path,
    coordinator_url: &str,
    reachable: Option<Reachable<'_>>,
) -> Result<(), Box<dyn Error>> {
    let key = device::load(dir)?.ok_or(crate::NO_KEY)?;
    let client = Client::new(coordinator_url, key);
    let account = client.account()?.account;
    println!(
        "node {} of account {} is online",
        client.device_key(),
        account.id
    );

    let store = Store::open(dir.join("stored"))?;
    let address = match &reachable {
        Some(reachable) => {
            store::serve(store.clone(), reachable.listen)?;
            println!("holding data for the network at {}", reachable.public_url);
            Some(reachable.public_url)
        }
        None => None,
    };

    let sandbox = Sandbox::new();
    loop {
        match turn(&client, &sandbox, &store, address) {
            Ok(true) => {}
            Ok(false) => sleep(IDLE_POLL),
            // Removed from its account: there is no more work for this key.
            Err(error) if error.is("device_revoked") || error.is("not_linked") => {
                return Err(error.into());
            }
            Err(error) => {
                eprintln!("{error}; retrying");
                sleep(RETRY_DELAY);
            }
        }
    }
}

/// One check-in and whatever it asks for. Returns whether a job was run, in
/// which case the node checks in again at once.
fn turn(
    client: &Client,
    sandbox: &Sandbox,
    store: &Store,
    address: Option<&str>,
) -> Result<bool, ClientError> {
    let CheckedIn {
        assignment,
        store: orders,
        challenge,
    } = client.check_in(address)?;

    store.delete_expired(unix_now());
    store.expect(&orders);
    for order in &orders {
        // The blob arrives at this node's server; say so once it is here.
        if store.holds(&order.root) {
            client.confirm_stored(order.root)?;
            println!("holding blob {}", order.root);
        }
    }
    if let Some(challenge) = challenge {
        answer_challenge(client, store, challenge)?;
    }
    let Some(assignment) = assignment else {
        return Ok(false);
    };
    let result = work(client, sandbox, &assignment)?;
    println!(
        "job {} reported, units used: {}",
        result.job_id, result.units
    );
    Ok(true)
}

/// Shows the coordinator the chunk it asked for. A node that no longer has the
/// blob stays silent, which the coordinator counts as a failed challenge.
fn answer_challenge(
    client: &Client,
    store: &Store,
    challenge: Challenge,
) -> Result<(), ClientError> {
    let Ok(blob) = store.read(&challenge.root) else {
        eprintln!(
            "challenged for blob {}, which is no longer here",
            challenge.root
        );
        return Ok(());
    };
    let Some((chunk, proof)) = merkle::prove(&blob, challenge.chunk) else {
        return Ok(());
    };
    client.answer_challenge(challenge, Blob(chunk.to_vec()), proof)
}
