//! `decomp-load`: measures how much work the control canister takes on a
//! local replica (spec section 12, "Control-plane throughput": "measure the
//! ceiling in testing"). It makes a number of accounts, has them submit jobs
//! at once, waits for every job to settle, and reports how many jobs a second
//! the control plane took and settled, how long jobs took, and how many cycles
//! the canister spent on each.
//!
//! ```text
//! decomp-load module.wasm --jobs 200 --accounts 10
//! ```
//!
//! It needs a running local network with nodes contributing (see
//! `scripts/local-network.sh` in Third Spaces, or `decomp-replica` and
//! `decomp-node start`). Configuration, by environment variable:
//! - `DECOMP_CONTROL_URL`: the canister's address, in the gateway's form
//!   `http://localhost:4943?canisterId=<id>`
//! - `POCKET_IC_URL`: the PocketIC server, to read the canister's cycles
//!   (default `http://127.0.0.1:47200`)

use std::env;
use std::error::Error;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use candid::Principal;
use decomp_core::{
    Credits, JobClass, JobId, JobStatus, Manifest, Privacy, SigningKey, Verify, merkle,
};
use decomp_node::client::{Client, unix_now};
use decomp_node::store;
use pocket_ic::PocketIc;

/// How often a job's status is asked for while waiting for it to settle.
const POLL: Duration = Duration::from_millis(500);

/// How long each job is given before its deadline.
const DEADLINE_SECS: u64 = 600;

struct Options {
    module: String,
    jobs: usize,
    accounts: usize,
    compute_cap: u64,
}

fn options() -> Result<Options, Box<dyn Error>> {
    let mut args = env::args().skip(1);
    let mut options = Options {
        module: String::new(),
        jobs: 100,
        accounts: 10,
        compute_cap: 1,
    };
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--jobs" => options.jobs = value()?.parse()?,
            "--accounts" => options.accounts = value()?.parse()?,
            "--compute-cap" => options.compute_cap = value()?.parse()?,
            _ if options.module.is_empty() => options.module = arg,
            _ => return Err(format!("unexpected argument {arg}").into()),
        }
    }
    if options.module.is_empty() || options.accounts == 0 {
        return Err(
            "usage: decomp-load module.wasm [--jobs N] [--accounts N] [--compute-cap N]".into(),
        );
    }
    Ok(options)
}

/// The canister's cycle balance, read from the PocketIC instance it runs on.
fn cycles(server: &str, canister: Principal) -> Option<u128> {
    let instances: Vec<String> = ureq::get(format!("{server}/instances"))
        .call()
        .ok()?
        .body_mut()
        .read_json()
        .ok()?;
    let instance = instances.iter().rposition(|state| state == "Available")?;
    let replica = PocketIc::new_from_existing_instance(server.parse().ok()?, instance, None);
    Some(replica.cycle_balance(canister))
}

fn main() -> Result<(), Box<dyn Error>> {
    let options = options()?;
    let control = env::var("DECOMP_CONTROL_URL")
        .map_err(|_| "set DECOMP_CONTROL_URL to the canister's address")?;
    let server = env::var("POCKET_IC_URL").unwrap_or_else(|_| "http://127.0.0.1:47200".into());
    let canister = control
        .split_once("canisterId=")
        .and_then(|(_, id)| Principal::from_text(id).ok())
        .ok_or("DECOMP_CONTROL_URL has no canisterId")?;

    // Accounts of their own, each with one device that submits.
    let mut devices = Vec::new();
    for _ in 0..options.accounts {
        let device = SigningKey::generate();
        Client::new(&control, SigningKey::generate()).create_account(device.public_key())?;
        devices.push(Arc::new(Client::new(&control, device)));
    }
    let module = std::fs::read(&options.module)?;
    let input = b"the quick brown fox jumps over the lazy dog".to_vec();
    let module_root = store::put(&devices[0], &module, store::JOB_DATA_DAYS)?;
    let input_root = store::put(&devices[0], &input, store::JOB_DATA_DAYS)?;
    debug_assert_eq!(module_root, merkle::root(&module));
    println!(
        "{} accounts ready; submitting {} jobs",
        options.accounts, options.jobs
    );

    let cycles_before = cycles(&server, canister);
    let started = Instant::now();
    let per_account = options.jobs.div_ceil(options.accounts);
    let workers: Vec<_> = devices
        .iter()
        .enumerate()
        .map(|(index, client)| {
            let client = Arc::clone(client);
            let count = per_account.min(options.jobs.saturating_sub(index * per_account));
            let compute_cap = options.compute_cap;
            thread::spawn(move || {
                let mut submitted: Vec<(JobId, Instant)> = Vec::new();
                let mut refused = Vec::new();
                // Each job's deadline differs, so that no two jobs sign to the
                // same request and so the same job ID.
                for number in 0..count as u64 {
                    let mut manifest = Manifest {
                        module: module_root,
                        input: input_root,
                        compute_cap,
                        deadline: unix_now() + DEADLINE_SECS + number,
                        class: JobClass::Menial,
                        slots: 1,
                        verify: Verify::Opportunistic,
                        fallback: None,
                        split: None,
                        max_fee: Credits::ZERO,
                        sponsor: None,
                        reads: Vec::new(),
                        state: None,
                        privacy: Privacy::Open,
                        share: None,
                        keep_from: Vec::new(),
                        apart_from: Vec::new(),
                        models: Vec::new(),
                    };
                    let submit = client.quote(manifest.clone()).and_then(|quote| {
                        manifest.max_fee = quote.highest_fee;
                        client.submit_job(manifest)
                    });
                    match submit {
                        Ok(job_id) => submitted.push((job_id, Instant::now())),
                        Err(error) => refused.push(error.to_string()),
                    }
                }
                let submitting = Instant::now();
                let mut settled = Vec::new();
                let mut failed = 0;
                for (job_id, at) in submitted {
                    loop {
                        match client.job(job_id) {
                            Ok(JobStatus::Settled { .. }) => {
                                settled.push(at.elapsed());
                                break;
                            }
                            Ok(JobStatus::Rejected | JobStatus::Expired) | Err(_) => {
                                failed += 1;
                                break;
                            }
                            Ok(_) => thread::sleep(POLL),
                        }
                    }
                }
                (submitting, settled, refused, failed)
            })
        })
        .collect();

    let mut last_submit = started;
    let mut times = Vec::new();
    let (mut refused, mut failed) = (Vec::new(), 0);
    for worker in workers {
        let (submitting, settled, r, f) = worker.join().expect("a worker does not panic");
        last_submit = last_submit.max(submitting);
        times.extend(settled);
        refused.extend(r);
        failed += f;
    }
    let total = started.elapsed();
    let cycles_after = cycles(&server, canister);

    times.sort();
    let at = |share: f64| times.get(((times.len() as f64 - 1.0) * share) as usize);
    let submit_secs = last_submit.duration_since(started).as_secs_f64();
    let accepted = options.jobs - refused.len();
    println!(
        "submitted {accepted} in {submit_secs:.1} s: {:.2} jobs a second taken",
        accepted as f64 / submit_secs.max(0.001)
    );
    println!(
        "settled   {} in {:.1} s: {:.2} jobs a second settled; {} refused, {failed} not settled",
        times.len(),
        total.as_secs_f64(),
        times.len() as f64 / total.as_secs_f64(),
        refused.len()
    );
    refused.sort();
    refused.dedup();
    for reason in &refused {
        println!("refused   {reason}");
    }
    if let (Some(median), Some(p90)) = (at(0.5), at(0.9)) {
        println!(
            "time      median {:.1} s, 90th percentile {:.1} s from submission to settlement",
            median.as_secs_f64(),
            p90.as_secs_f64()
        );
    }
    match (cycles_before, cycles_after) {
        (Some(before), Some(after)) if accepted > 0 => {
            let spent = before.saturating_sub(after);
            println!(
                "cycles    {spent} spent by the canister, {} a job (timers and node requests included)",
                spent / accepted as u128
            );
        }
        _ => println!("cycles    not read: no PocketIC server at {server}"),
    }
    Ok(())
}
