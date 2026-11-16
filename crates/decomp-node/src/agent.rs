//! The command's two long-running modes: joining an account, and working as a node.

use std::error::Error;
use std::path::Path;
use std::thread::sleep;
use std::time::Duration;

use decomp_node::client::Client;
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

/// Works as a node until stopped: checks in, runs the job it is given, reports
/// the result, and repeats.
pub fn start(dir: &Path, coordinator_url: &str) -> Result<(), Box<dyn Error>> {
    let key =
        device::load(dir)?.ok_or("this device has no key yet; run `decomp-node link` first")?;
    let client = Client::new(coordinator_url, key);
    let account = client.account()?.account;
    println!(
        "node {} of account {} is online",
        client.device_key(),
        account.id
    );

    let sandbox = Sandbox::new();
    loop {
        let worked = client.check_in().and_then(|assignment| match assignment {
            Some(assignment) => work(&client, &sandbox, &assignment).map(Some),
            None => Ok(None),
        });
        match worked {
            Ok(Some(result)) => {
                println!(
                    "job {} reported, units used: {}",
                    result.job_id, result.units
                )
            }
            Ok(None) => sleep(IDLE_POLL),
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
