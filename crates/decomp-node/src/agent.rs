//! The command's two long-running modes: joining an account, and working as a node.

use std::error::Error;
use std::path::Path;
use std::sync::mpsc::{self, Sender};
use std::thread::{self, sleep};
use std::time::Duration;

use decomp_core::{Assignment, Blob, Challenge, CheckedIn, merkle};
use decomp_node::client::{Client, ClientError, unix_now};
use decomp_node::relay::Relay;
use decomp_node::store::Store;
use decomp_node::{IDLE_POLL, RETRY_DELAY, server, work};
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

/// Where other devices can connect to this node, for a node that holds and
/// relays data.
pub struct Reachable<'a> {
    /// The local address to listen on, such as `0.0.0.0:9000`.
    pub listen: &'a str,
    /// The address other devices use, such as `http://203.0.113.5:9000`.
    pub public_url: &'a str,
}

/// Works as a node until stopped. It checks in every couple of seconds, whether
/// or not it is on a job, because blobs to take in, storage challenges, and
/// channels to carry do not wait for a job to finish; the job runs alongside.
/// A node holds and relays data only if it is given an address other devices can
/// reach it at.
pub fn start(
    dir: &Path,
    coordinator_url: &str,
    reachable: Option<Reachable<'_>>,
) -> Result<(), Box<dyn Error>> {
    let key = device::load(dir)?.ok_or(crate::NO_KEY)?;
    let client = &Client::new(coordinator_url, key);
    let account = client.account()?.account;
    println!(
        "node {} of account {} is online",
        client.device_key(),
        account.id
    );

    let store = Store::open(dir.join("stored"))?;
    let relay = Relay::new();
    let address = match &reachable {
        Some(reachable) => {
            server::serve(reachable.listen, store.clone(), relay.clone())?;
            println!(
                "holding and relaying data for the network at {}",
                reachable.public_url
            );
            Some(reachable.public_url)
        }
        None => None,
    };

    let sandbox = &Sandbox::new();
    // A job tells this loop when it is over, so the node asks for its next at once.
    let (job_over, job_is_over) = mpsc::channel();
    let mut on_a_job = false;
    thread::scope(|jobs| {
        loop {
            let pause = match turn(client, &store, &relay, address) {
                Ok(Some(assignment)) if !on_a_job => {
                    let job_over = job_over.clone();
                    jobs.spawn(move || run(client, sandbox, &assignment, &job_over));
                    on_a_job = true;
                    IDLE_POLL
                }
                Ok(_) => IDLE_POLL,
                // Removed from its account: there is no more work for this key.
                Err(error) if error.is("device_revoked") || error.is("not_linked") => {
                    return Err(error.into());
                }
                Err(error) => {
                    eprintln!("{error}; retrying");
                    RETRY_DELAY
                }
            };
            if job_is_over.recv_timeout(pause).is_ok() {
                on_a_job = false;
            }
        }
    })
}

/// One check-in and the data work it asks for. Returns the job the node should
/// be running, if any.
fn turn(
    client: &Client,
    store: &Store,
    relay: &Relay,
    address: Option<&str>,
) -> Result<Option<Assignment>, ClientError> {
    let CheckedIn {
        assignment,
        store: orders,
        challenge,
        relay: channels,
    } = client.check_in(address)?;
    let now = unix_now();

    store.delete_expired(now);
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

    relay.expect(&channels, now);
    for claim in relay.claims() {
        match client.redeem_relay(claim.header.clone(), claim.receipt.clone()) {
            Ok(()) => println!("relayed {} bytes on channel {}", claim.bytes, claim.channel),
            // The coordinator will not pay this claim, and asking again would not
            // change its answer.
            Err(error @ ClientError::Refused { .. }) => {
                eprintln!("not paid for channel {}: {error}", claim.channel);
            }
            Err(error) => return Err(error),
        }
        relay.claimed(&claim);
    }
    Ok(assignment)
}

/// Runs one job and says when it is over. A job that could not be run or
/// reported is handed back at a later check-in, so wait before saying so.
fn run(client: &Client, sandbox: &Sandbox, assignment: &Assignment, over: &Sender<()>) {
    match work(client, sandbox, assignment) {
        Ok(result) => println!(
            "job {} reported, units used: {}",
            result.job_id, result.units
        ),
        Err(error) => {
            eprintln!("job {}: {error}; retrying", assignment.job_id);
            sleep(RETRY_DELAY);
        }
    }
    // The loop this reports to runs for as long as the program does.
    let _ = over.send(());
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
