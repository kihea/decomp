//! The DeComp node agent: this device's side of the network. It joins an
//! account, works as a node, submits jobs, stores files, and sends files to
//! other devices.

mod agent;
mod device;

use std::collections::HashSet;
use std::error::Error;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread::sleep;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use decomp_core::{
    AccountId, AccountStatus, AppManifest, ContentHash, Credits, JobId, JobStatus, Manifest,
    OpeningKey, Outcome, Privacy, PrivateRoute, PublicKey, Receipt, ShardRef, Split, Sponsorship,
    merkle, privacy,
};
use decomp_node::client::{Client, ClientError, unix_now};
use decomp_node::deliver::{self, Delivered};
use decomp_node::relay;

use crate::agent::Reachable;
use decomp_sandbox::{Blobs, Job, Sandbox};
use decomp_schema::records;

const NO_KEY: &str = "this device has no key yet; run `decomp-node link` first";

/// The file, in this device's directory, listing the stored files it has
/// already collected, one root to a line.
const COLLECTED_FILE: &str = "collected";

/// How often `submit` asks whether its job has finished.
const JOB_POLL: Duration = Duration::from_secs(1);

/// The largest private job, in compute units, that `submit` takes to fit this
/// device and so runs here (spec section 7: the local route is taken when "the
/// job fits the user's device"). What a device can do is its hardware-fit score,
/// which nothing measures yet. Until it is, a device is taken to fit what the
/// spec says any device can run: tier 0, a job of one unit.
const LOCAL_ROUTE_MAX_UNITS: u64 = 1;

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
        /// If the device is not waiting to receive, the file is left in storage
        /// for it. This is how many days to pay for.
        #[arg(long, default_value_t = 7)]
        keep_days: u32,
    },
    /// Wait for a file sent to this device, and save it.
    Receive {
        /// Where to save the file.
        #[arg(long)]
        output: PathBuf,
    },
    /// Submit a job to the network and wait for its result. A private job is
    /// run by the first private route that fits it, which may be on this device.
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
        /// A shard of an app's state to hand the job, as the app's ID, a slash,
        /// and the shard's name. The module must be one of that app's.
        #[arg(long, value_parser = parse_shard)]
        state: Option<ShardRef>,
        /// Credits to set aside, on top of the job's own highest fee, for the
        /// follow-up jobs it may ask for. What is not spent comes back.
        #[arg(long, default_value = "0")]
        follow_up_credits: Credits,
    },
    /// Show the records in a shard of an app's state.
    State {
        /// The app's ID, a slash, and the shard's name.
        #[arg(value_parser = parse_shard)]
        shard: ShardRef,
    },
    /// Run a job module on this device only, without the network. For a private
    /// job this is the local route.
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
    /// A file the job may read as a blob, by the hash of its contents. Repeat
    /// for each one.
    #[arg(long = "read")]
    reads: Vec<PathBuf>,
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
        Command::Send {
            file,
            to,
            keep_days,
        } => connect().and_then(|client| send(&client, &file, to, keep_days)),
        Command::Receive { output } => connect().and_then(|client| {
            let opening_key = device::opening_key(&cli.dir)?;
            receive(&client, &cli.dir, &opening_key, &output)
        }),
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
            state,
            follow_up_credits,
        } => connect().and_then(|client| {
            let limits = Limits {
                deadline_secs,
                redundancy,
                split_bytes,
                sponsor,
                state,
                follow_up_credits,
            };
            submit(&client, job, limits)
        }),
        Command::State { shard } => connect().and_then(|client| show_state(&client, shard)),
        Command::Run { job } => {
            read_job(&job).and_then(|files| run_here(&Sandbox::new(), &job, &files))
        }
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
    state: Option<ShardRef>,
    follow_up_credits: Credits,
}

/// Waits until a job closes. Gives its receipt, or says why it has none.
fn wait_for(
    client: &Client,
    job_id: JobId,
) -> Result<Result<Box<Receipt>, &'static str>, ClientError> {
    loop {
        match client.job(job_id)? {
            JobStatus::Waiting | JobStatus::Running => sleep(JOB_POLL),
            JobStatus::Verified { receipt } => return Ok(Ok(receipt)),
            JobStatus::Disputed => return Ok(Err("the nodes disagreed and none was right")),
            JobStatus::Expired => {
                return Ok(Err("the deadline passed before the job had a result"));
            }
        }
    }
}

/// Waits for one follow-up job and says how it went, on one line.
fn show_follow_up(client: &Client, job_id: JobId) -> Result<(), Box<dyn Error>> {
    let went = match wait_for(client, job_id) {
        // The job that asked for it had not left enough of its fee cap.
        Err(error) if error.is("unknown_job") => "not started".to_string(),
        Err(error) => return Err(error.into()),
        Ok(Err(reason)) => reason.to_string(),
        Ok(Ok(receipt)) => match receipt.outcome {
            Outcome::Completed {
                output, spawned, ..
            } if spawned.is_empty() => {
                format!("output {output}, fee {} credits", receipt.fee.total)
            }
            Outcome::Completed {
                output, spawned, ..
            } => format!(
                "output {output}, fee {} credits, asked for {} more",
                receipt.fee.total,
                spawned.len()
            ),
            Outcome::Failed { failure } => format!("failed: {failure}"),
        },
    };
    println!("follow-up {job_id}: {went}");
    Ok(())
}

/// Reads a shard as it is written on the command line: the app's ID, a slash,
/// and the shard's name.
fn parse_shard(text: &str) -> Result<ShardRef, String> {
    let (app, shard) = text
        .split_once('/')
        .ok_or("write a shard as the app's ID, a slash, and the shard's name")?;
    Ok(ShardRef {
        app: app
            .parse()
            .map_err(|_| "the part before the slash is not an app ID")?,
        shard: shard.to_string(),
    })
}

/// Prints a shard's records, one to a line.
fn show_state(client: &Client, shard: ShardRef) -> Result<ExitCode, Box<dyn Error>> {
    let Some(contents) = client.state(shard.app, shard.shard)? else {
        println!("the shard is empty");
        return Ok(ExitCode::SUCCESS);
    };
    let records = records::decode(&client.get_blob(contents)?)
        .ok_or("the shard's contents are not a set of records")?;
    println!("contents {contents}");
    for (key, value) in &records {
        let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
        println!("{} = {}", text(key), text(value));
    }
    Ok(ExitCode::SUCCESS)
}

fn submit(client: &Client, job: JobArgs, limits: Limits) -> Result<ExitCode, Box<dyn Error>> {
    let Limits {
        deadline_secs,
        redundancy,
        split_bytes,
        sponsor,
        state,
        follow_up_credits,
    } = limits;
    let files = read_job(&job)?;
    let sandbox = Sandbox::new();
    let declared = sandbox.declared(&files.module);
    if declared.is_some_and(|signature| signature.privacy == Privacy::Private) {
        if state.is_some() {
            return Err("a private job cannot be handed state".into());
        }
        // "The SDK takes the first route that fits and reports which one it used."
        let fits_device = job.compute_cap <= LOCAL_ROUTE_MAX_UNITS;
        return match privacy::route(fits_device)? {
            PrivateRoute::Local => run_here(&sandbox, &job, &files),
        };
    }
    let JobFiles {
        module,
        input,
        blobs,
    } = files;
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
        reads: blobs
            .into_values()
            .map(|blob| client.put_blob(blob))
            .collect::<Result<_, _>>()?,
        state,
        privacy: Privacy::Open,
    };
    // Set aside the most the job can cost, and what its follow-ups may; what is
    // not spent comes back.
    for _ in 0..pieces {
        manifest.max_fee += manifest.highest_fee();
    }
    manifest.max_fee += follow_up_credits;
    println!("module  {}", manifest.module);
    let job_id = client.submit_job(manifest)?;
    println!("job     {job_id}");

    let receipt = match wait_for(client, job_id)? {
        Ok(receipt) => receipt,
        Err(reason) => return fail(reason),
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
        Outcome::Completed {
            output,
            blobs,
            state,
            spawned,
        } => {
            for blob in blobs {
                println!("stored  {blob}");
            }
            if let Some(contents) = state {
                println!("state   {contents}");
            }
            for index in 0..spawned.len() {
                show_follow_up(client, JobId::follow_up(&job_id, index as u64))?;
            }
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
    let placement = client.store_blob(root, bytes.len() as u64, days, None)?;
    println!("root    {root}");
    let holding = decomp_node::store::upload_to(&placement, &root, &bytes);
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
    let bytes = decomp_node::store::fetch(client, root)?;
    fs::write(output, &bytes).map_err(|e| format!("{}: {e}", output.display()))?;
    println!("fetched {} bytes", bytes.len());
    Ok(ExitCode::SUCCESS)
}

/// Sends a file to another device: over a channel if the device is waiting, and
/// otherwise into storage to wait for it.
fn send(
    client: &Client,
    file: &Path,
    to: PublicKey,
    keep_days: u32,
) -> Result<ExitCode, Box<dyn Error>> {
    let bytes = fs::read(file).map_err(|e| format!("{}: {e}", file.display()))?;
    match deliver::deliver(client, to, &bytes, keep_days)? {
        Delivered::Sent(route) => {
            println!("channel {}", route.channel);
            println!("sent    {} bytes by way of {}", bytes.len(), route.address);
        }
        Delivered::Stored { root, nodes } => {
            println!("root    {root}");
            println!(
                "stored  {} bytes on {nodes} nodes for {keep_days} days; the device was not waiting",
                bytes.len()
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Waits for something sent to this device, and saves it.
fn receive(
    client: &Client,
    dir: &Path,
    opening_key: &OpeningKey,
    output: &Path,
) -> Result<ExitCode, Box<dyn Error>> {
    // A device that linked before sealing existed has not published a key yet.
    client.publish_sealing_key(opening_key.sealing_key())?;
    println!("device  {}", client.device_key());
    println!("waiting for a file sent to this device...");
    // Stored files stay listed after they are collected, so remember them.
    let collected = dir.join(COLLECTED_FILE);
    let mut skip: HashSet<ContentHash> = fs::read_to_string(&collected)
        .unwrap_or_default()
        .lines()
        .filter_map(|root| root.parse().ok())
        .collect();
    loop {
        match deliver::collect(client, opening_key, &mut skip) {
            Ok(Some(found)) => {
                fs::write(output, &found.data).map_err(|e| format!("{}: {e}", output.display()))?;
                println!("from    {}", found.from);
                match found.stored_as {
                    Some(root) => {
                        let mut list = OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&collected)?;
                        writeln!(list, "{root}")?;
                        println!(
                            "saved   {} bytes, which were waiting in storage",
                            found.data.len()
                        );
                    }
                    None => println!("saved   {} bytes, sent just now", found.data.len()),
                }
                return Ok(ExitCode::SUCCESS);
            }
            Ok(None) => sleep(relay::TAKE_POLL),
            Err(error) => eprintln!("skipping one: {error}"),
        }
    }
}

/// Runs a job on this device and shows what it produced. For a private job this
/// is the local route: "Local runs the module on the user's device with no
/// verification" (spec section 7), and nothing of the job reaches a node.
fn run_here(
    sandbox: &Sandbox,
    job: &JobArgs,
    files: &JobFiles,
) -> Result<ExitCode, Box<dyn Error>> {
    let JobFiles {
        module,
        input,
        blobs,
    } = files;
    let declared = sandbox.declared(module);
    let private = declared
        .as_ref()
        .is_some_and(|signature| signature.privacy == Privacy::Private);
    // The coordinator checks an input's types when a job is submitted to it. A
    // private job run here never reaches it, so the check is made here.
    if let Some(signature) = declared.filter(|_| private) {
        signature
            .check_input(input)
            .map_err(|mismatch| mismatch.to_string())?;
    }
    // Off the network a job has no ID to draw its randomness from, so the seed is
    // fixed: every local run of a job draws the same bytes.
    let run = sandbox.run(&Job {
        module,
        input,
        compute_cap: job.compute_cap,
        seed: [0; 32],
        blobs,
        // Nor is there any state to hand it.
        state: None,
    })?;

    println!("module  {}", ContentHash::of(module));
    if let Some(signature) = sandbox.signature(module)? {
        println!("types   {}", String::from_utf8_lossy(&signature));
    }
    if private {
        println!("route   local: run on this device, and sent to no node");
    }
    println!("units   {} of {}", run.units, job.compute_cap);
    let checkpoints = ContentHash::of(&run.checkpoints.concat());
    println!(
        "checks  {} checkpoints, list {checkpoints}",
        run.checkpoints.len()
    );
    match run.result {
        Ok(produced) => {
            for blob in &produced.blobs {
                println!("stored  {} ({} bytes)", ContentHash::of(blob), blob.len());
            }
            report_output(&produced.output, job.output.as_deref())?;
            Ok(ExitCode::SUCCESS)
        }
        Err(failure) => fail(&failure.to_string()),
    }
}

/// The files a job's arguments name.
struct JobFiles {
    module: Vec<u8>,
    input: Vec<u8>,
    /// The blobs the job may read.
    blobs: Blobs,
}

fn read_job(job: &JobArgs) -> Result<JobFiles, Box<dyn Error>> {
    let read = |path: &Path| fs::read(path).map_err(|e| format!("{}: {e}", path.display()));
    let input = match &job.input {
        Some(path) => read(path)?,
        None => Vec::new(),
    };
    let mut blobs = Blobs::new();
    for path in &job.reads {
        let bytes = read(path)?;
        println!("read    {}  {}", ContentHash::of(&bytes), path.display());
        blobs.insert(ContentHash::of(&bytes), bytes);
    }
    Ok(JobFiles {
        module: read(&job.module)?,
        input,
        blobs,
    })
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
