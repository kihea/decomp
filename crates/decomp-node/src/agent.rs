//! The command's two long-running modes: joining an account, and working as a node.

use std::error::Error;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant};

use decomp_core::{Blob, Challenge, JobId, LeasesGranted, WorkPreferences, merkle};
use decomp_node::client::{Client, ClientError, unix_now};
use decomp_node::relay::Relay;
use decomp_node::schedule::{Conditions, Event, Worker, may_work};
use decomp_node::store::Store;
use decomp_node::{IDLE_POLL, RETRY_DELAY, server};
use decomp_sandbox::Sandbox;

use crate::device;

/// The file, in this device's directory, listing the jobs this node has
/// revealed a result for, one ID to a line. `decomp-node receipts` reads it to
/// check that their receipts are in the settlement log's batches.
pub const REPORTED_FILE: &str = "reported";

/// How often a device waiting to be approved asks whether it has been.
const LINK_POLL: Duration = Duration::from_secs(3);

/// How often the node reads its owner's work preferences again, so that a
/// change made from another device takes effect.
const PREFERENCES_REFRESH: Duration = Duration::from_secs(60);

/// Makes this device's key if it has none, shows its code, and waits until a
/// device in an account approves it.
pub fn link(dir: &Path, control_url: &str) -> Result<(), Box<dyn Error>> {
    let key = match device::load(dir)? {
        Some(key) => key,
        None => device::create(dir)?,
    };
    let client = Client::new(control_url, key);
    println!("device code: {}", client.device_key());

    let mut waiting = false;
    loop {
        match client.account() {
            Ok(status) => {
                println!("linked to account {}", status.account.id);
                // So that other devices can send to this one from now on.
                client.publish_sealing_key(device::opening_key(dir)?.sealing_key())?;
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

/// Works as a node until stopped. It asks for work every couple of seconds,
/// whether or not jobs are running, because blobs to take in, storage
/// challenges, channels to carry, and results to reveal do not wait for a job
/// to finish; jobs run alongside, up to the slots the owner offers. A node
/// holds and relays data only if it is given an address other devices can
/// reach it at.
///
/// The command-line node runs on machines its owner points it at, so it takes
/// the device to be on mains power and an unmetered network; the desktop app
/// reads both from the system.
pub fn start(
    dir: &Path,
    control_url: &str,
    reachable: Option<Reachable<'_>>,
) -> Result<(), Box<dyn Error>> {
    let key = device::load(dir)?.ok_or(crate::NO_KEY)?;
    let client = Arc::new(Client::new(control_url, key));
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

    let worker = Worker::new(Arc::clone(&client), Arc::new(Sandbox::new()));
    let mut preferences: Option<(Instant, WorkPreferences)> = None;
    loop {
        let stale = preferences
            .as_ref()
            .is_none_or(|(read, _)| read.elapsed() >= PREFERENCES_REFRESH);
        if stale {
            match client.work_preferences(client.device_key()) {
                Ok(read) => preferences = Some((Instant::now(), read)),
                Err(error) => eprintln!("could not read work preferences: {error}"),
            }
        }
        let allowed = preferences
            .as_ref()
            .is_none_or(|(_, preferences)| may_work(preferences, Conditions::default()));
        // Outside the owner's rules the node stops asking, once every result
        // it committed to has been revealed.
        let pause = if !allowed && !worker.has_unrevealed() && worker.running() == 0 {
            IDLE_POLL
        } else {
            match turn(&client, &worker, &store, &relay, address) {
                Ok(()) => IDLE_POLL,
                // Removed from its account: there is no more work for this key.
                Err(error) if error.is("device_revoked") || error.is("not_linked") => {
                    return Err(error.into());
                }
                Err(error) => {
                    eprintln!("{error}; retrying");
                    RETRY_DELAY
                }
            }
        };
        for event in worker.take_events() {
            report(dir, &event);
        }
        sleep(pause);
    }
}

/// One request for work, and the data work it asks for.
fn turn(
    client: &Client,
    worker: &Worker,
    store: &Store,
    relay: &Relay,
    address: Option<&str>,
) -> Result<(), ClientError> {
    let granted: LeasesGranted = client.request_lease(address)?;
    let now = unix_now();
    worker.handle(&granted);

    store.delete_expired(now);
    store.expect(&granted.store);
    for order in &granted.store {
        // The blob arrives at this node's server; say so once it is here.
        if store.holds(&order.root) {
            client.confirm_stored(order.root)?;
            println!("holding blob {}", order.root);
        }
    }
    if let Some(challenge) = granted.challenge {
        answer_challenge(client, store, challenge)?;
    }

    relay.expect(&granted.relay, now);
    for claim in relay.claims() {
        match client.redeem_relay(claim.header.clone(), claim.receipt.clone()) {
            Ok(()) => println!("relayed {} bytes on channel {}", claim.bytes, claim.channel),
            // The control plane will not pay this claim, and asking again
            // would not change its answer.
            Err(error @ ClientError::Refused { .. }) => {
                eprintln!("not paid for channel {}: {error}", claim.channel);
            }
            Err(error) => return Err(error),
        }
        relay.claimed(&claim);
    }
    Ok(())
}

/// Logs what the worker did, and notes each revealed job so that its receipt
/// can be looked for in a batch later.
fn report(dir: &Path, event: &Event) {
    match event {
        Event::Committed { job_id, units } => {
            println!("job {job_id} committed, units used: {units}");
        }
        Event::Revealed { job_id } => {
            println!("job {job_id} revealed");
            if let Err(error) = remember(dir, job_id) {
                eprintln!("could not note job {job_id}: {error}");
            }
        }
        Event::Failed { job_id, error } => eprintln!("job {job_id}: {error}"),
    }
}

/// Notes a job this node has revealed a result for.
fn remember(dir: &Path, job_id: &JobId) -> std::io::Result<()> {
    let mut reported = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(REPORTED_FILE))?;
    writeln!(reported, "{job_id}")
}

/// Shows the control plane the chunk it asked for. A node that no longer has
/// the blob stays silent, which counts as a failed challenge.
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
