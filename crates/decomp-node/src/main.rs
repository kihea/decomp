//! The DeComp node agent: this device's side of the network. It joins an
//! account, works as a node, submits jobs, stores files, and sends files to
//! other devices.

mod agent;
mod device;

use std::collections::HashSet;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread::sleep;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use decomp_core::{
    AccountId, AccountStatus, AppManifest, ContentHash, Credits, JobStatus, Manifest, Outcome,
    PublicKey, Split, Sponsorship, merkle,
};
use decomp_node::client::{Client, unix_now};
use decomp_node::relay;

use crate::agent::Reachable;
use decomp_sandbox::{Job, Sandbox};

const NO_KEY: &str = "this device has no key yet; run `decomp-node link` first";

/// How many times, and how far apart, `store` offers a file to a node that is
/// not yet ready to take it.
const UPLOAD_ATTEMPTS: u32 = 15;
const UPLOAD_RETRY: Duration = Duration::from_secs(1);

/// How often `submit` asks whether its job has finished.
const JOB_POLL: Duration = Duration::from_secs(1);

#[derive(Parser)]
#[command(name = "decomp-node", about = "The DeComp node agent", version)]
struct Cli {
    /// Directory holding this device's key.
    #[arg(long, env = "DECOMP_NODE_DIR", default_value = "node", global = true)]
    dir: PathBuf,
    /// Address of the coordinator.
    #[arg(
        long,
        env = "DECOMP_COORDINATOR_URL",
        default_value = "http://127.0.0.1:8787",
        global = true
    )]
    coordinator: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Join an account: show this device's code and wait for it to be approved.
    Link,
    /// Show this device's account, its credits, and how far this node is trusted.
    Account,
    /// Put earned credits up as stake, which jobs of tier 3 and above require.
    Stake {
        /// Credits to stake, such as 20 or 12.5.
        amount: Credits,
    },
    /// Take stake back into earned credits.
    Unstake {
        /// Credits to take back, such as 20 or 12.5.
        amount: Credits,
    },
    /// Work as a node: run the jobs the coordinator hands out, until stopped.
    Start {
        /// Also hold and relay data for the network, at this local address, such
        /// as 0.0.0.0:9000. Other devices must be able to connect to it.
        #[arg(long)]
        listen: Option<String>,
        /// The address other devices use to reach `--listen`, such as
        /// http://203.0.113.5:9000. Without it, http:// and the listen address.
        #[arg(long, requires = "listen")]
        public_url: Option<String>,
    },
    /// Pay to have a file held by nodes, and upload it to them.
    Store {
        file: PathBuf,
        /// How many days to pay for.
        #[arg(long)]
        days: u32,
    },
    /// Fetch a stored file from the nodes that hold it.
    Fetch {
        /// The root the file was stored under, as `store` printed it.
        root: ContentHash,
        /// Where to save the file.
        #[arg(long)]
        output: PathBuf,
    },
    /// Publish this account's app, or replace the one it has published. The
    /// app's ID is the account's.
    Publish {
        /// The app's name.
        #[arg(long)]
        name: String,
        /// A job module of the app, a WebAssembly file. Repeat for each module.
        #[arg(long = "module")]
        modules: Vec<PathBuf>,
        /// Pay for users' jobs that run the app's modules, up to this many
        /// credits a day for each account. Without it the app pays for none.
        #[arg(long)]
        sponsor_daily: Option<Credits>,
    },
    /// Send a file to another device through a relay node.
    Send {
        file: PathBuf,
        /// The receiving device's code, as `link` or `account` shows it.
        #[arg(long)]
        to: PublicKey,
    },
    /// Wait for a file sent to this device, and save it.
    Receive {
        /// Where to save the file.
        #[arg(long)]
        output: PathBuf,
    },
    /// Submit a job to the network and wait for its result.
    Submit {
        #[command(flatten)]
        job: JobArgs,
        /// Seconds from now by which the job must have a result.
        #[arg(long, default_value_t = 300)]
        deadline_secs: u64,
        /// Nodes, each from a different account, that must run the job. Without
        /// it the coordinator decides: one proven node, or two if one is new.
        #[arg(long)]
        redundancy: Option<u32>,
        /// Cut the input into pieces of this many bytes and run the module on
        /// each as a job of its own. The output is the outputs of the pieces, joined in order.
        #[arg(long)]
        split_bytes: Option<u64>,
        /// The ID of an app to pay for the job, if the app agrees to. Without
        /// it this account pays.
        #[arg(long)]
        sponsor: Option<AccountId>,
    },
    /// Run a job module on this device only, without the network.
    Run {
        #[command(flatten)]
        job: JobArgs,
    },
}

#[derive(Args)]
struct JobArgs {
    /// The job module, a WebAssembly file.
    module: PathBuf,
    /// File holding the job's input. Without it, the input is empty.
    #[arg(long)]
    input: Option<PathBuf>,
    /// File to write the job's output to. Without it, only its hash is shown.
    #[arg(long)]
    output: Option<PathBuf>,
    /// Compute units the job may use before it is stopped.
    #[arg(long)]
    compute_cap: u64,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let connect = || -> Result<Client, Box<dyn Error>> {
        let key = device::load(&cli.dir)?.ok_or(NO_KEY)?;
        Ok(Client::new(&cli.coordinator, key))
    };
    let outcome = match cli.command {
        Command::Link => agent::link(&cli.dir, &cli.coordinator).map(|()| ExitCode::SUCCESS),
        Command::Start { listen, public_url } => {
            let public_url =
                public_url.or_else(|| listen.as_ref().map(|at| format!("http://{at}")));
            let reachable = listen.as_deref().zip(public_url.as_deref());
            let reachable = reachable.map(|(listen, public_url)| Reachable { listen, public_url });
            agent::start(&cli.dir, &cli.coordinator, reachable).map(|()| ExitCode::SUCCESS)
        }
        Command::Store { file, days } => connect().and_then(|client| store(&client, &file, days)),
        Command::Fetch { root, output } => {
            connect().and_then(|client| fetch(&client, root, &output))
        }
        Command::Publish {
            name,
            modules,
            sponsor_daily,
        } => connect().and_then(|client| publish(&client, name, &modules, sponsor_daily)),
        Command::Send { file, to } => connect().and_then(|client| send(&client, &file, to)),
        Command::Receive { output } => connect().and_then(|client| receive(&client, &output)),
        Command::Account => connect().and_then(|client| show_account(&client)),
        Command::Stake { amount } => connect().and_then(|client| {
            show_credits(&client.stake(amount)?);
            Ok(ExitCode::SUCCESS)
        }),
        Command::Unstake { amount } => connect().and_then(|client| {
            show_credits(&client.unstake(amount)?);
            Ok(ExitCode::SUCCESS)
        }),
        Command::Submit {
            job,
            deadline_secs,
            redundancy,
            split_bytes,
            sponsor,
        } => connect().and_then(|client| {
            let limits = Limits {
                deadline_secs,
                redundancy,
                split_bytes,
                sponsor,
            };
            submit(&client, job, limits)
        }),
        Command::Run { job } => run_locally(job),
    };
    outcome.unwrap_or_else(|error| {
        eprintln!("error: {error}");
        ExitCode::FAILURE
    })
}

fn show_account(client: &Client) -> Result<ExitCode, Box<dyn Error>> {
    let status = client.account()?;
    println!("account  {}", status.account.id);
    for device in status
        .account
        .devices
        .iter()
        .filter(|device| !device.revoked)
    {
        println!("device   {}", device.key);
    }
    show_credits(&status);
    if let Some(app) = &status.account.app {
        show_app(app);
    }

    let standing = client.standing()?;
    let stage = if standing.proven {
        "proven"
    } else {
        "on probation"
    };
    println!(
        "node     {stage}, may take jobs up to tier {}",
        standing.max_tier
    );
    println!(
        "         reliability {:.3}, availability {:.3}",
        standing.reliability, standing.availability
    );
    Ok(ExitCode::SUCCESS)
}

fn show_credits(status: &AccountStatus) {
    let balance = &status.balance;
    println!(
        "credits  {} to spend, {} staked",
        balance.spendable(),
        balance.staked
    );
    println!(
        "         {} are starter credits, which cannot be staked or cashed out",
        balance.granted
    );
}

fn show_app(app: &AppManifest) {
    let modules = match app.modules.len() {
        1 => "1 module".to_string(),
        count => format!("{count} modules"),
    };
    println!("app      {}, {modules}", app.name);
    match app.sponsorship {
        Some(rule) => println!(
            "         pays for up to {} credits of each account's jobs a day",
            rule.per_account_daily
        ),
        None => println!("         does not pay for jobs"),
    }
}

/// Stores the app's modules and publishes its manifest.
fn publish(
    client: &Client,
    name: String,
    modules: &[PathBuf],
    sponsor_daily: Option<Credits>,
) -> Result<ExitCode, Box<dyn Error>> {
    let mut hashes = Vec::new();
    for path in modules {
        let bytes = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let hash = client.put_blob(bytes)?;
        println!("module   {hash}  {}", path.display());
        hashes.push(hash);
    }
    let status = client.publish_app(AppManifest {
        name,
        modules: hashes,
        sponsorship: sponsor_daily.map(|per_account_daily| Sponsorship { per_account_daily }),
    })?;
    println!("app ID   {}", status.account.id);
    if let Some(app) = &status.account.app {
        show_app(app);
    }
    Ok(ExitCode::SUCCESS)
}

/// The limits `submit` puts on a job, and who pays for it.
struct Limits {
    deadline_secs: u64,
    redundancy: Option<u32>,
    split_bytes: Option<u64>,
    sponsor: Option<AccountId>,
}

fn submit(client: &Client, job: JobArgs, limits: Limits) -> Result<ExitCode, Box<dyn Error>> {
    let Limits {
        deadline_secs,
        redundancy,
        split_bytes,
        sponsor,
    } = limits;
    let (module, input) = read_job(&job)?;
    let split = split_bytes.map(|chunk_bytes| Split { chunk_bytes });
    let pieces = split.map_or(1, |split| split.pieces(input.len() as u64));
    let mut manifest = Manifest {
        module: client.put_blob(module)?,
        input: client.put_blob(input)?,
        compute_cap: job.compute_cap,
        deadline: unix_now() + deadline_secs,
        redundancy,
        split,
        max_fee: Credits::ZERO,
        sponsor,
    };
    // Set aside the most the job can cost; what is not spent comes back.
    for _ in 0..pieces {
        manifest.max_fee += manifest.highest_fee();
    }
    println!("module  {}", manifest.module);
    let job_id = client.submit_job(manifest)?;
    println!("job     {job_id}");

    let receipt = loop {
        match client.job(job_id)? {
            JobStatus::Waiting | JobStatus::Running => sleep(JOB_POLL),
            JobStatus::Verified { receipt } => break receipt,
            JobStatus::Disputed => return fail("the nodes disagreed and none was right"),
            JobStatus::Expired => return fail("the deadline passed before the job had a result"),
        }
    };
    if receipt.pieces.is_empty() {
        println!(
            "units   {} on each of {} nodes",
            receipt.units,
            receipt.results.len()
        );
    } else {
        println!(
            "units   {} in all, over {} pieces",
            receipt.units,
            receipt.pieces.len()
        );
    }
    println!("fee     {} credits", receipt.fee.total);
    match receipt.outcome {
        Outcome::Completed { output } => {
            let bytes = client.get_blob(output)?;
            report_output(&bytes, job.output.as_deref())?;
            Ok(ExitCode::SUCCESS)
        }
        Outcome::Failed { failure } => fail(&failure.to_string()),
    }
}

/// Pays for a file to be held by nodes and sends it to each of them.
fn store(client: &Client, file: &Path, days: u32) -> Result<ExitCode, Box<dyn Error>> {
    let bytes = fs::read(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let root = merkle::root(&bytes);
    let placement = client.store_blob(root, bytes.len() as u64, days)?;
    println!("root    {root}");

    let mut holding = 0;
    for address in &placement.addresses {
        // A node refuses the blob until it has heard from the coordinator that it
        // should hold it, which it does at its next check-in. So try for a while.
        let sent = (0..UPLOAD_ATTEMPTS).find_map(|attempt| {
            if attempt > 0 {
                sleep(UPLOAD_RETRY);
            }
            decomp_node::store::upload(address, &root, &bytes).ok()
        });
        match sent {
            Some(()) => holding += 1,
            None => eprintln!("could not send the file to the node at {address}"),
        }
    }
    println!(
        "nodes   {holding} of {} hold the file",
        placement.addresses.len()
    );
    Ok(if holding > 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Fetches a stored file from the first node that gives a true copy of it.
fn fetch(client: &Client, root: ContentHash, output: &Path) -> Result<ExitCode, Box<dyn Error>> {
    let located = client.locate(root)?;
    for address in &located.addresses {
        match decomp_node::store::download(address, &root, located.size) {
            Ok(bytes) => {
                fs::write(output, &bytes).map_err(|e| format!("{}: {e}", output.display()))?;
                println!("fetched {} bytes from {address}", bytes.len());
                return Ok(ExitCode::SUCCESS);
            }
            Err(reason) => eprintln!("the node at {address} did not give the file: {reason}"),
        }
    }
    fail("no node that holds the file gave it")
}

/// Sends a file to another device through a relay node.
fn send(client: &Client, file: &Path, to: PublicKey) -> Result<ExitCode, Box<dyn Error>> {
    let bytes = fs::read(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let route = relay::send(client, to, &bytes)?;
    println!("channel {}", route.channel);
    println!(
        "sent    {} bytes to the relay at {}",
        bytes.len(),
        route.address
    );
    Ok(ExitCode::SUCCESS)
}

/// Waits for a channel addressed to this device that has something on it, and
/// saves what it carries.
fn receive(client: &Client, output: &Path) -> Result<ExitCode, Box<dyn Error>> {
    println!("device  {}", client.device_key());
    println!("waiting for a file sent to this device...");
    // Channels that went wrong, which are not tried again.
    let mut broken = HashSet::new();
    loop {
        for incoming in client.incoming()? {
            if broken.contains(&incoming.channel) {
                continue;
            }
            let bytes = match relay::receive(client, &incoming) {
                Ok(Some(bytes)) => bytes,
                Ok(None) => continue,
                Err(error) => {
                    eprintln!("channel {}: {error}", incoming.channel);
                    broken.insert(incoming.channel);
                    continue;
                }
            };
            fs::write(output, &bytes).map_err(|e| format!("{}: {e}", output.display()))?;
            println!("from    {}", incoming.from);
            println!(
                "saved   {} bytes, relayed by {}",
                bytes.len(),
                incoming.address
            );
            return Ok(ExitCode::SUCCESS);
        }
        sleep(relay::TAKE_POLL);
    }
}

fn run_locally(job: JobArgs) -> Result<ExitCode, Box<dyn Error>> {
    let (module, input) = read_job(&job)?;
    // Off the network a job has no ID to draw its randomness from, so the seed is
    // fixed: every local run of a job draws the same bytes.
    let sandbox = Sandbox::new();
    let run = sandbox.run(&Job {
        module: &module,
        input: &input,
        compute_cap: job.compute_cap,
        seed: [0; 32],
    })?;

    println!("module  {}", ContentHash::of(&module));
    if let Some(signature) = sandbox.signature(&module)? {
        println!("types   {}", String::from_utf8_lossy(&signature));
    }
    println!("units   {} of {}", run.units, job.compute_cap);
    let checkpoints = ContentHash::of(&run.checkpoints.concat());
    println!(
        "checks  {} checkpoints, list {checkpoints}",
        run.checkpoints.len()
    );
    match run.result {
        Ok(bytes) => {
            report_output(&bytes, job.output.as_deref())?;
            Ok(ExitCode::SUCCESS)
        }
        Err(failure) => fail(&failure.to_string()),
    }
}

/// The module and input a job's arguments name.
fn read_job(job: &JobArgs) -> Result<(Vec<u8>, Vec<u8>), Box<dyn Error>> {
    let read = |path: &Path| fs::read(path).map_err(|e| format!("{}: {e}", path.display()));
    let input = match &job.input {
        Some(path) => read(path)?,
        None => Vec::new(),
    };
    Ok((read(&job.module)?, input))
}

fn report_output(bytes: &[u8], save_to: Option<&Path>) -> Result<(), Box<dyn Error>> {
    println!("output  {} ({} bytes)", ContentHash::of(bytes), bytes.len());
    if let Some(path) = save_to {
        fs::write(path, bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(())
}

/// Reports a job that ran but did not produce a result.
fn fail(reason: &str) -> Result<ExitCode, Box<dyn Error>> {
    println!("failed  {reason}");
    Ok(ExitCode::FAILURE)
}
