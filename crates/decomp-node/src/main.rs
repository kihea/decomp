//! The DeComp node agent: this device's side of the network. It joins an
//! account, works as a node, and submits jobs for the account.

mod agent;
mod device;

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread::sleep;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use decomp_core::{AccountStatus, ContentHash, Credits, JobStatus, Manifest, Outcome, Split};
use decomp_node::client::{Client, unix_now};
use decomp_sandbox::{Job, Sandbox};

const NO_KEY: &str = "this device has no key yet; run `decomp-node link` first";

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
    Start,
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
        Command::Start => agent::start(&cli.dir, &cli.coordinator).map(|()| ExitCode::SUCCESS),
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
        } => connect()
            .and_then(|client| submit(&client, job, deadline_secs, redundancy, split_bytes)),
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

fn submit(
    client: &Client,
    job: JobArgs,
    deadline_secs: u64,
    redundancy: Option<u32>,
    split_bytes: Option<u64>,
) -> Result<ExitCode, Box<dyn Error>> {
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
