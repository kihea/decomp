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
use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant};

use clap::{Args, Parser, Subcommand};
use decomp_core::{APPROVED_MODELS, ICRC_TRANSFER_FEE, IcrcAccount, Subaccount, Tokens};
use decomp_core::{
    AccountId, AccountStatus, AppManifest, Approval, BidOrder, Call, ContentHash, Controller,
    Credits, Encrypted, Fallback, Inclusion, JobClass, JobId, JobStatus, MAX_LOG_PAGE,
    MAX_STORAGE_DAYS, Manifest, OpeningKey, Outcome, Pickup, Price, Privacy, PrivateRoute,
    PublicKey, Receipt, Request, SHARES, ShardRef, Share, ShareSet, SigningKey, Split, Sponsorship,
    Verification, Verify, WorkPreferences, controller, cut, draw_hash, encrypted, merkle, privacy,
    shares,
};

use decomp_node::beacon::Rounds;
use decomp_node::client::{Client, ClientError, unix_now};
use decomp_node::deliver::{self, Delivered};
use decomp_node::models::{CallContext, Generator};
use decomp_node::relay;
use decomp_node::store;

use crate::agent::Reachable;
use decomp_sandbox::{Blobs, Job, Sandbox};
use decomp_schema::{Signature, records};

const NO_KEY: &str = "this device has no key yet; run `decomp-node link` first";

/// The file, in this device's directory, listing the stored files it has
/// already collected, one root to a line.
const COLLECTED_FILE: &str = "collected";

/// The file, in this device's directory, that says which account this device
/// is recovering and when the recovery takes effect, in seconds since 1970.
const RECOVERY_FILE: &str = "recovery";

/// How often `submit` asks whether its job has finished.
const JOB_POLL: Duration = Duration::from_secs(1);

/// The size of job any device can run, the spec's tier 0 job of one unit. A
/// private job this small fits this device whatever its measured pace.
const ANY_DEVICE_UNITS: u64 = 1;

#[derive(Parser)]
#[command(name = "decomp-node", about = "The DeComp node agent", version)]
struct Cli {
    /// Directory holding this device's key.
    #[arg(long, env = "DECOMP_NODE_DIR", default_value = "node", global = true)]
    dir: PathBuf,
    /// Address of the control plane: a local devnet, or a gateway to the
    /// network's canisters.
    #[arg(
        long,
        env = "DECOMP_CONTROL_URL",
        default_value = "http://127.0.0.1:8787",
        global = true
    )]
    control: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create an account for this device: make an account key, save it in a
    /// file to be kept offline, and make this device the account's first. If
    /// the file already holds a key from `new-account-key`, the account is
    /// made for that key.
    CreateAccount {
        /// Where to save the new account key, or the key made earlier.
        #[arg(long)]
        account_key: PathBuf,
    },
    /// Make an account key without making the account, and show the ID the
    /// account will have. For an account that has to be named before it
    /// exists, such as a reviewer a control plane is installed with.
    NewAccountKey {
        /// Where to save the new account key.
        #[arg(long)]
        account_key: PathBuf,
    },
    /// Join an account: show this device's code and wait for it to be approved.
    Link,
    /// Approve a new device into this device's account, by the code its
    /// `link` shows.
    ApproveDevice { device: PublicKey },
    /// Make keys for a new device, such as a rented machine, and approve it
    /// into this device's account. Shows the device's code and a secret to
    /// give the machine in DECOMP_DEVICE_SECRET, which stands in for its key
    /// files. Anyone who has the secret can work as the device: revoke it
    /// when the machine is given back.
    AddDevice,
    /// Revoke a device of this device's account for good, by its code.
    RevokeDevice { device: PublicKey },
    /// Show the proof that this device belongs to its account, which apps'
    /// jobs check, as JSON, after checking it against the control plane's key.
    DeviceProof,
    /// Ask for earned credits to be cashed out. Signed by the account key.
    CashOut {
        /// Credits to cash out, such as 20 or 12.5.
        amount: Credits,
        /// The account key file, as `create-account` made it.
        #[arg(long)]
        account_key: PathBuf,
    },
    /// Send tokens to another account. Signed by the account key.
    SendTokens {
        /// The ID of the account to send them to.
        to: AccountId,
        /// Tokens to send, such as 20 or 12.5.
        amount: Tokens,
        /// The account key file, as `create-account` made it.
        #[arg(long)]
        account_key: PathBuf,
    },
    /// Burn tokens into credits, which can be spent but not cashed out.
    BurnTokens {
        /// Tokens to burn, such as 20 or 12.5.
        amount: Tokens,
    },
    /// Withdraw tokens as copies on the ICRC ledger, which exchanges and
    /// wallets trade. Signed by the account key.
    WithdrawTokens {
        /// Tokens to withdraw, such as 20 or 12.5.
        amount: Tokens,
        /// The principal of the ICRC account to mint the copies to.
        #[arg(long)]
        to: String,
        /// The ICRC account's subaccount, as 64 hex digits.
        #[arg(long)]
        subaccount: Option<Subaccount>,
        /// The account key file, as `create-account` made it.
        #[arg(long)]
        account_key: PathBuf,
    },
    /// Show where to send copies of the token on the ICRC ledger to deposit
    /// them into this account.
    DepositAddress,
    /// Burn the copies sent to this account's deposit address and take them
    /// back as tokens.
    DepositTokens,
    /// As a treasurer: give out tokens from the treasury's supply.
    GrantTokens {
        /// The ID of the account to give them to.
        to: AccountId,
        /// Tokens to give, such as 20 or 12.5.
        amount: Tokens,
    },
    /// Start a name's term again.
    RenewName { name: String },
    /// Return a name to the pool; its deposit comes back.
    ReleaseName { name: String },
    /// Show this device's account, its credits, and how far this node is trusted.
    Account,
    /// Make a recovery key, save it in a file to be kept offline, and show its
    /// public key, which the account adds as a controller. If the file is
    /// there already, show the public key of the key in it.
    RecoveryKey { file: PathBuf },
    /// Recover an account onto this device. Name the account and give what
    /// its controllers signed: the recovery starts and then waits. Run it
    /// again with nothing once the wait is over, and this device joins the
    /// account. With an account and no approvals, show what the controllers
    /// must sign.
    Recover {
        /// The ID of the account to recover.
        account: Option<AccountId>,
        /// A recovery key file, as `recovery-key` made it, to approve with.
        /// Repeat for each one.
        #[arg(long = "key")]
        keys: Vec<PathBuf>,
        /// A file holding an approval made elsewhere: by a guardian with
        /// `approve-recovery`, by a passkey, or by a wallet. Repeat for each.
        #[arg(long = "approval")]
        approvals: Vec<PathBuf>,
    },
    /// As a guardian of an account: approve a device for it, and show the
    /// approval for the device to hand in with `recover`.
    ApproveRecovery {
        /// The ID of the account being recovered.
        account: AccountId,
        /// The code of the device to approve, as its `recover` shows it.
        device: PublicKey,
    },
    /// Cancel a recovery that is waiting on this device's account.
    CancelRecovery {
        /// The code of the device whose recovery to cancel.
        device: PublicKey,
    },
    /// As a treasurer: record that an account bought credits.
    /// Nothing here takes or checks a payment.
    Bought {
        /// The ID of the account that bought them.
        account: AccountId,
        /// Credits bought, such as 20 or 12.5.
        amount: Credits,
        /// A name for the payment. A payment is credited once.
        #[arg(long)]
        payment: String,
    },
    /// As a treasurer: show the cash-outs that wait.
    CashOuts,
    /// As a treasurer: record that a cash-out was paid, which
    /// removes its credits for good. Nothing here pays anyone.
    PayCashOut {
        /// The cash-out's name, as `cash-outs` shows it.
        id: ContentHash,
        /// A name for the payment the account was paid by.
        #[arg(long)]
        payout: String,
    },
    /// As a treasurer: refuse a cash-out, which returns its
    /// credits to the account.
    RefuseCashOut {
        /// The cash-out's name, as `cash-outs` shows it.
        id: ContentHash,
    },
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
    /// Work as a node: ask for work and run the jobs leased to this device,
    /// within the owner's work preferences, until stopped.
    Start {
        /// Also hold and relay data for the network, at this local address, such
        /// as 0.0.0.0:9000. Other devices must be able to connect to it.
        #[arg(long)]
        listen: Option<String>,
        /// The address other devices use to reach `--listen`, such as
        /// http://203.0.113.5:9000. Without it, http:// and the listen address.
        #[arg(long, requires = "listen")]
        public_url: Option<String>,
        /// Hold part of a stored model, fetched from storage nodes: NAME for
        /// all of it, NAME:ends to run jobs with it, or NAME:stage-N to run
        /// that stage for other nodes (which needs --listen). Repeat for each.
        #[arg(long = "hold")]
        hold: Vec<String>,
    },
    /// Pay to have a file held by nodes, and upload it to them.
    Store {
        file: PathBuf,
        /// How many days to pay for.
        #[arg(long)]
        days: u32,
    },
    /// Delete a file this account stored: the nodes drop it, and what is left
    /// of its fee comes back.
    Delete {
        /// The root the file was stored under, as `store` printed it.
        root: ContentHash,
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
        /// The app's client files, bundled into one file, for a gateway to
        /// serve at the app's name.
        #[arg(long)]
        client: Option<PathBuf>,
        /// A file of metadata the application layer reads, such as how the app
        /// presents itself in a launcher.
        #[arg(long)]
        metadata: Option<PathBuf>,
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
        /// How the result is confirmed: opportunistic (accepted after one
        /// result, confirmed if a second node picks it up in time), required
        /// (waits for a second node), or none.
        #[arg(long, value_parser = parse_verify, default_value = "opportunistic")]
        verify: Verify,
        /// If no node finishes the job in time, run it in the control plane at
        /// the fallback rate instead of letting it expire.
        #[arg(long)]
        fallback: bool,
        /// The job's class: menial, data, or inference.
        #[arg(long, value_parser = parse_class, default_value = "menial")]
        class: JobClass,
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
        /// A key, an account's or a device's, whose account's nodes must not be
        /// handed the job. Repeat for each one.
        #[arg(long = "keep-from")]
        keep_from: Vec<PublicKey>,
        /// An earlier job whose nodes must not be handed this one: no node of
        /// an account that was handed it is. Repeat for each one.
        #[arg(long = "apart-from")]
        apart_from: Vec<JobId>,
    },
    /// Show the records in a shard of an app's state.
    State {
        /// The app's ID, a slash, and the shard's name.
        #[arg(value_parser = parse_shard)]
        shard: ShardRef,
    },
    /// Check the settlement log from its first entry, and show the batches in it.
    Log,
    /// Check that the receipts of the jobs this node has run are in the
    /// batches the receipts canister posted.
    Receipts,
    /// Check the draws the job board has published: that each lease went to
    /// the node the rule draws from public randomness.
    Draws,
    /// Show a device's work preferences, or change them. Without a device, this
    /// device's. Options left out keep their value.
    Preferences {
        /// The device, by its code. Any device of this account.
        #[arg(long)]
        device: Option<PublicKey>,
        #[command(flatten)]
        set: PreferenceArgs,
    },
    /// Show the posted rate of every job class and tier.
    Rates,
    /// Place a standing bid in the open market (tiers 3 and 4).
    Bid {
        /// The job class: menial, data, or inference.
        #[arg(long, value_parser = parse_class)]
        class: JobClass,
        /// The lowest tier the bid takes.
        #[arg(long, default_value_t = 3)]
        min_tier: u32,
        /// The highest tier the bid takes.
        #[arg(long, default_value_t = 4)]
        max_tier: u32,
        /// How many jobs the bid asks for.
        #[arg(long)]
        quantity: u32,
        /// The price per compute unit, in microcredits.
        #[arg(long, conflicts_with = "per_job", required_unless_present = "per_job")]
        per_unit: Option<u64>,
        /// The price per job, in microcredits.
        #[arg(long)]
        per_job: Option<u64>,
        /// How long the bid stands, in hours.
        #[arg(long, default_value_t = 24)]
        hours: u64,
    },
    /// Show this account's standing bids.
    Bids,
    /// Withdraw a standing bid.
    CancelBid { id: ContentHash },
    /// Register a name for this account's app, setting the deposit aside.
    RegisterName { name: String },
    /// Show what a name points to.
    Resolve { name: String },
    /// List registered names, in order.
    Directory {
        /// Only apps that have been reviewed.
        #[arg(long)]
        verified: bool,
    },
    /// As a reviewer: mark the app a name points to as reviewed, or not.
    Review {
        name: String,
        /// Mark it as not reviewed.
        #[arg(long)]
        revoke: bool,
    },
    /// Run a job module on this device only, without the network. For a private
    /// job this is the local route.
    Run {
        #[command(flatten)]
        job: JobArgs,
    },
}

#[derive(Args)]
struct PreferenceArgs {
    /// Whether the device contributes at all.
    #[arg(long)]
    contributing: Option<bool>,
    /// Job classes accepted, comma-separated: menial, data, inference.
    #[arg(long, value_delimiter = ',', value_parser = parse_class)]
    classes: Option<Vec<JobClass>>,
    #[arg(long)]
    min_tier: Option<u32>,
    #[arg(long)]
    max_tier: Option<u32>,
    /// The lowest posted rate worked for, in microcredits per unit. 0 takes any.
    #[arg(long)]
    floor: Option<u64>,
    #[arg(long)]
    slots: Option<u32>,
    /// Working hours, local time, as START-END, such as 22-6; or "any".
    #[arg(long, value_parser = parse_hours)]
    hours: Option<Option<(u8, u8)>>,
    #[arg(long)]
    on_battery: Option<bool>,
    #[arg(long)]
    on_metered: Option<bool>,
    /// Take work in tiers 3 and 4 only through this account's bids.
    #[arg(long)]
    market_only: Option<bool>,
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
    /// An approved model the job may call, by name (such as tiny-1) or ID.
    /// Repeat for each one. A job that calls models is of the inference
    /// class.
    #[arg(long = "model", value_parser = parse_model)]
    models: Vec<ContentHash>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let connect = || -> Result<Client, Box<dyn Error>> {
        let key = device::load(&cli.dir)?.ok_or(NO_KEY)?;
        Ok(Client::new(&cli.control, key))
    };
    let outcome = match cli.command {
        Command::Link => agent::link(&cli.dir, &cli.control).map(|()| ExitCode::SUCCESS),
        Command::ApproveDevice { device } => connect().and_then(|client| {
            client.approve_device(device)?;
            println!("approved device {device}");
            Ok(ExitCode::SUCCESS)
        }),
        Command::AddDevice => connect().and_then(|client| {
            let key = SigningKey::generate();
            let opening = OpeningKey::generate();
            client.approve_device(key.public_key())?;
            let device = Client::new(&cli.control, SigningKey::from_seed(key.seed()));
            // So that other devices can send to the new one from the start.
            device.publish_sealing_key(opening.sealing_key())?;
            println!("approved device {}", key.public_key());
            println!("{}={}", device::SECRET_VAR, device::secret(&key, &opening));
            Ok(ExitCode::SUCCESS)
        }),
        Command::DeviceProof => connect().and_then(|client| {
            let proof = client.device_proof()?;
            let control = client.control_key().ok_or("the settlement log has no entry yet")?;
            let account = proof
                .account_of(&client.device_key(), &control, unix_now())
                .ok_or("the proof does not hold")?;
            eprintln!("this device belongs to account {account}");
            println!("{}", serde_json::to_string(&proof)?);
            Ok(ExitCode::SUCCESS)
        }),
        Command::RevokeDevice { device } => connect().and_then(|client| {
            client.revoke_device(device)?;
            println!("revoked device {device}");
            Ok(ExitCode::SUCCESS)
        }),
        Command::CashOut {
            amount,
            account_key,
        } => as_account(&cli.control, &account_key).and_then(|client| {
            let id = client.cash_out(amount)?;
            println!(
                "cash-out {id} asked for; {amount} credits are set aside until it is paid or refused"
            );
            Ok(ExitCode::SUCCESS)
        }),
        Command::SendTokens {
            to,
            amount,
            account_key,
        } => as_account(&cli.control, &account_key).and_then(|client| {
            client.send_tokens(to, amount)?;
            println!("sent {amount} tokens to account {to}");
            Ok(ExitCode::SUCCESS)
        }),
        Command::BurnTokens { amount } => connect().and_then(|client| {
            client.burn_tokens(amount)?;
            println!(
                "burned {amount} tokens into {} credits, which can be spent but not cashed out",
                amount.credits()
            );
            Ok(ExitCode::SUCCESS)
        }),
        Command::WithdrawTokens {
            amount,
            to,
            subaccount,
            account_key,
        } => as_account(&cli.control, &account_key).and_then(|client| {
            let to = IcrcAccount {
                owner: to,
                subaccount,
            };
            let id = client.withdraw_tokens(amount, to)?;
            println!("withdrawal {id} asked for; {amount} tokens are set aside until they are minted on the ICRC ledger");
            Ok(ExitCode::SUCCESS)
        }),
        Command::DepositAddress => connect().and_then(|client| {
            let decomp_core::DepositAddress { ledger, account } = client.deposit_address()?;
            println!("ledger      {ledger}");
            println!("owner       {}", account.owner);
            if let Some(subaccount) = account.subaccount {
                println!("subaccount  {subaccount}");
            }
            println!(
                "Send copies of the token there, then run deposit-tokens. At least {ICRC_TRANSFER_FEE} tokens."
            );
            Ok(ExitCode::SUCCESS)
        }),
        Command::DepositTokens => connect().and_then(|client| {
            client.deposit_tokens()?;
            println!("asked; the copies at the deposit address are burned and arrive as tokens in a few seconds");
            Ok(ExitCode::SUCCESS)
        }),
        Command::GrantTokens { to, amount } => connect().and_then(|client| {
            client.grant_tokens(to, amount)?;
            println!("granted {amount} tokens from the treasury to account {to}");
            Ok(ExitCode::SUCCESS)
        }),
        Command::RenewName { name } => connect().and_then(|client| {
            let record = client.renew_name(name)?;
            println!("{} held until {}", record.name, record.expires_at);
            Ok(ExitCode::SUCCESS)
        }),
        Command::ReleaseName { name } => connect().and_then(|client| {
            client.release_name(name.clone())?;
            println!("{name} released; its deposit is back");
            Ok(ExitCode::SUCCESS)
        }),
        Command::CreateAccount { account_key } => {
            create_account(&cli.dir, &cli.control, &account_key)
        }
        Command::NewAccountKey { account_key } => new_account_key(&account_key),
        Command::Start {
            listen,
            public_url,
            hold,
        } => {
            let public_url =
                public_url.or_else(|| listen.as_ref().map(|at| format!("http://{at}")));
            let reachable = listen.as_deref().zip(public_url.as_deref());
            let reachable = reachable.map(|(listen, public_url)| Reachable { listen, public_url });
            agent::start(&cli.dir, &cli.control, reachable, &hold).map(|()| ExitCode::SUCCESS)
        }
        Command::Store { file, days } => connect().and_then(|client| store(&client, &file, days)),
        Command::Delete { root } => connect().and_then(|client| {
            client.delete_blob(root)?;
            println!("deleted {root}; its nodes drop it when they next ask for work");
            Ok(ExitCode::SUCCESS)
        }),
        Command::Fetch { root, output } => {
            connect().and_then(|client| fetch(&client, root, &output))
        }
        Command::Publish {
            name,
            modules,
            sponsor_daily,
            client: client_files,
            metadata,
        } => connect().and_then(|client| {
            let files = AppFiles {
                client: client_files,
                metadata,
            };
            publish(&client, name, &modules, sponsor_daily, files)
        }),
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
        Command::RecoveryKey { file } => recovery_key(&file),
        Command::Recover {
            account,
            keys,
            approvals,
        } => recover(&cli.dir, &cli.control, account, &keys, &approvals),
        Command::ApproveRecovery { account, device } => connect().and_then(|client| {
            let statement = controller::statement(&account, &device);
            let approval = Approval::Key {
                signer: client.device_key(),
                signature: client.key().sign(statement.as_bytes()),
            };
            println!("{}", serde_json::to_string(&approval)?);
            Ok(ExitCode::SUCCESS)
        }),
        Command::CancelRecovery { device } => connect().and_then(|client| {
            client.cancel_recovery(device)?;
            println!("cancelled; device {device} can never join the account");
            Ok(ExitCode::SUCCESS)
        }),
        Command::Bought {
            account,
            amount,
            payment,
        } => connect().and_then(|client| {
            client.credit_purchase(account, amount, payment)?;
            println!("credited {amount} bought credits to account {account}");
            Ok(ExitCode::SUCCESS)
        }),
        Command::CashOuts => connect().and_then(|client| {
            let waiting = client.cash_outs()?;
            for cash_out in &waiting {
                let (id, account) = (cash_out.id, cash_out.account);
                println!("{id}  {} credits  account {account}", cash_out.amount);
            }
            if waiting.is_empty() {
                println!("no cash-out is waiting");
            }
            Ok(ExitCode::SUCCESS)
        }),
        Command::PayCashOut { id, payout } => connect().and_then(|client| {
            client.pay_cash_out(id, payout)?;
            println!("recorded as paid; the credits of cash-out {id} are gone");
            Ok(ExitCode::SUCCESS)
        }),
        Command::RefuseCashOut { id } => connect().and_then(|client| {
            client.refuse_cash_out(id)?;
            println!("refused; the credits of cash-out {id} are back with the account");
            Ok(ExitCode::SUCCESS)
        }),
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
            verify,
            fallback,
            class,
            split_bytes,
            sponsor,
            state,
            follow_up_credits,
            keep_from,
            apart_from,
        } => connect().and_then(|client| {
            let limits = Limits {
                deadline_secs,
                verify,
                fallback: fallback.then_some(Fallback::AlwaysOn),
                class,
                split_bytes,
                sponsor,
                state,
                follow_up_credits,
                keep_from,
                apart_from,
            };
            submit(&client, &cli.dir, job, limits)
        }),
        Command::State { shard } => connect().and_then(|client| show_state(&client, shard)),
        Command::Log => connect().and_then(|client| show_log(&client)),
        Command::Receipts => connect().and_then(|client| check_receipts(&client, &cli.dir)),
        Command::Draws => connect().and_then(|client| check_draws(&client)),
        Command::Preferences { device, set } => {
            connect().and_then(|client| preferences(&client, device, set))
        }
        Command::Rates => connect().and_then(|client| show_rates(&client)),
        Command::Bid {
            class,
            min_tier,
            max_tier,
            quantity,
            per_unit,
            per_job,
            hours,
        } => connect().and_then(|client| {
            let price = match (per_unit, per_job) {
                (Some(price), _) => Price::PerUnit(price),
                (None, Some(price)) => Price::PerJob(price),
                (None, None) => return Err("give --per-unit or --per-job".into()),
            };
            let bid = client.place_bid(BidOrder {
                class,
                min_tier,
                max_tier,
                quantity,
                price,
                expires_at: unix_now() + hours * 3600,
            })?;
            println!("bid {} placed", bid.id);
            Ok(ExitCode::SUCCESS)
        }),
        Command::Bids => connect().and_then(|client| {
            let bids = client.bids()?;
            for bid in &bids {
                let price = match bid.order.price {
                    Price::PerUnit(price) => format!("{price} µc/unit"),
                    Price::PerJob(price) => format!("{price} µc/job"),
                };
                println!(
                    "{}  {:?} tiers {}-{}  {price}  {}/{} filled  lapses in {} s",
                    bid.id,
                    bid.order.class,
                    bid.order.min_tier,
                    bid.order.max_tier,
                    bid.filled,
                    bid.order.quantity,
                    bid.order.expires_at.saturating_sub(unix_now())
                );
            }
            if bids.is_empty() {
                println!("no standing bids");
            }
            Ok(ExitCode::SUCCESS)
        }),
        Command::CancelBid { id } => connect().and_then(|client| {
            client.cancel_bid(id)?;
            println!("bid {id} withdrawn");
            Ok(ExitCode::SUCCESS)
        }),
        Command::RegisterName { name } => connect().and_then(|client| {
            let record = client.register_name(name)?;
            println!(
                "{} registered to app {}, held until {}",
                record.name, record.app, record.expires_at
            );
            Ok(ExitCode::SUCCESS)
        }),
        Command::Resolve { name } => connect().and_then(|client| {
            let resolved = client.resolve_name(name)?;
            println!("{} -> app {}", resolved.record.name, resolved.record.app);
            println!(
                "reviewed {}",
                if resolved.record.verified {
                    "yes"
                } else {
                    "no"
                }
            );
            show_app(&resolved.manifest);
            Ok(ExitCode::SUCCESS)
        }),
        Command::Directory { verified } => connect().and_then(|client| {
            let mut from = None;
            loop {
                let page = client.directory(verified, from.take())?;
                for entry in &page.entries {
                    let mark = if entry.record.verified {
                        "reviewed"
                    } else {
                        ""
                    };
                    println!("{:<24} {}  {mark}", entry.record.name, entry.record.app);
                }
                match page.next {
                    Some(next) => from = Some(next),
                    None => break,
                }
            }
            Ok(ExitCode::SUCCESS)
        }),
        Command::Review { name, revoke } => connect().and_then(|client| {
            let record = client.set_verified(name, !revoke)?;
            println!("{} reviewed: {}", record.name, record.verified);
            Ok(ExitCode::SUCCESS)
        }),
        Command::Run { job } => {
            // Device proofs a job checks rest on the control plane's key,
            // known here only if the control plane can be reached.
            let control_key = connect().ok().and_then(|client| client.control_key());
            read_job(&job).and_then(|files| {
                run_here(&Sandbox::new(), &cli.dir, &job, &files, control_key)
            })
        }
    };
    outcome.unwrap_or_else(|error| {
        eprintln!("error: {error}");
        ExitCode::FAILURE
    })
}

/// A client that signs with the account key in `file`, for what moves value
/// out of an account.
fn as_account(control: &str, file: &Path) -> Result<Client, Box<dyn Error>> {
    let missing = || format!("{} is not there", file.display());
    let key = device::key_file(file)?.ok_or_else(missing)?;
    Ok(Client::new(control, key))
}

/// Makes a recovery key in `file`, or reads the one there, and shows its
/// public key (spec section 1: "Recovery key or phrase: A public key ... With
/// the user, offline").
fn recovery_key(file: &Path) -> Result<ExitCode, Box<dyn Error>> {
    let key = match device::key_file(file)? {
        Some(key) => key,
        None => device::create_key_file(file)?,
    };
    println!("recovery key: {}", key.public_key());
    println!(
        "Add it to the account as a controller, and keep {} offline.",
        file.display()
    );
    Ok(ExitCode::SUCCESS)
}

/// Starts, or finishes, the recovery of an account onto this device (spec
/// section 1: "a passkey or any other controller can approve a device the same
/// way", and "A recovery takes effect after 48 hours (starting value), and
/// any approved device can cancel it in that time").
fn recover(
    dir: &Path,
    coordinator: &str,
    account: Option<AccountId>,
    keys: &[PathBuf],
    approval_files: &[PathBuf],
) -> Result<ExitCode, Box<dyn Error>> {
    let key = match device::load(dir)? {
        Some(key) => key,
        None => device::create(dir)?,
    };
    let client = Client::new(coordinator, key);
    let device = client.device_key();
    let note = dir.join(RECOVERY_FILE);

    let Some(account) = account else {
        let Ok(started) = fs::read_to_string(&note) else {
            return Err("name the account to recover".into());
        };
        let (account, effective_at) = started.trim().split_once(' ').ok_or("bad recovery file")?;
        let (account, effective_at): (AccountId, u64) = (account.parse()?, effective_at.parse()?);
        let now = unix_now();
        if now < effective_at {
            println!(
                "The recovery of account {account} takes effect in {} seconds. Until then a device of the account can cancel it.",
                effective_at - now
            );
            return Ok(ExitCode::SUCCESS);
        }
        client.complete_recovery(account)?;
        fs::remove_file(&note)?;
        // So that other devices can send to this one from now on.
        client.publish_sealing_key(device::opening_key(dir)?.sealing_key())?;
        println!("recovered: this device is now in account {account}");
        return Ok(ExitCode::SUCCESS);
    };

    let statement = controller::statement(&account, &device);
    let mut approvals = Vec::new();
    for file in keys {
        let missing = || format!("{} is not there", file.display());
        let key = device::key_file(file)?.ok_or_else(missing)?;
        approvals.push(Approval::Key {
            signer: key.public_key(),
            signature: key.sign(statement.as_bytes()),
        });
    }
    for file in approval_files {
        approvals.push(serde_json::from_str(fs::read_to_string(file)?.trim())?);
    }
    if approvals.is_empty() {
        println!("device code: {device}");
        println!("The account's controllers approve this device by signing these two lines:");
        println!("{statement}");
        println!(
            "A guardian runs `decomp-node approve-recovery {account} {device}`. Hand what they give to `recover` with --approval."
        );
        return Ok(ExitCode::SUCCESS);
    }

    let status = client.recover(account, approvals)?;
    let mut waiting = status.account.recoveries.iter();
    let recovery = waiting
        .find(|recovery| recovery.device_key == device)
        .ok_or("the coordinator did not start the recovery")?;
    fs::write(&note, format!("{account} {}\n", recovery.effective_at))?;
    println!(
        "The recovery has started and takes effect in {} seconds. Until then a device of the account can cancel it. Run `decomp-node recover` again after that.",
        recovery.effective_at.saturating_sub(unix_now())
    );
    Ok(ExitCode::SUCCESS)
}

/// Checks the settlement log and shows what it holds.
fn show_log(client: &Client) -> Result<ExitCode, Box<dyn Error>> {
    let log = client.settlement_log()?;
    let Some(signer) = log.signer() else {
        println!("the settlement log has no entries yet");
        return Ok(ExitCode::SUCCESS);
    };
    println!("signed by {signer}");
    for batch in log.batches() {
        let (start, end) = log.receipts_of(batch.number).unwrap_or_default();
        println!(
            "batch {}  {} receipts, {} balances and {} token balances changed, fingerprint {}",
            batch.number,
            end - start,
            batch.changes.len(),
            batch.token_changes.len(),
            batch.fingerprint
        );
    }
    println!("every entry is signed by the control plane and follows the one before it");
    Ok(ExitCode::SUCCESS)
}

/// Checks, for every job this node has reported, that its receipt is in a
/// batch of the settlement log (spec section 6: "Any node can check that its
/// own receipts are included"). The log is checked from its first entry, each
/// receipt is linked to its batch's fingerprint by the hashes the control
/// plane gives, and each must hold this node's own signed reveal of the job.
fn check_receipts(client: &Client, dir: &Path) -> Result<ExitCode, Box<dyn Error>> {
    let log = client.settlement_log()?;
    let reported = fs::read_to_string(dir.join(agent::REPORTED_FILE)).unwrap_or_default();
    let jobs: Vec<JobId> = reported.lines().filter_map(|id| id.parse().ok()).collect();
    let (mut included, mut missing) = (0, 0);
    for job_id in &jobs {
        match client.inclusion(*job_id) {
            Ok(inclusion) => {
                if log.includes(&inclusion) && is_report_of(&inclusion, job_id, client) {
                    println!("job {job_id}  in batch {}", inclusion.batch);
                    included += 1;
                } else {
                    println!(
                        "job {job_id}  NOT PROVEN to be in batch {}",
                        inclusion.batch
                    );
                    missing += 1;
                }
            }
            Err(error) if error.is("not_batched") => {
                println!("job {job_id}  its receipt is not in a batch yet");
            }
            // A job that expired or was rejected has no receipt, and neither
            // has a check that was handed out to this node.
            Err(error) if error.is("unknown_receipt") => println!("job {job_id}  no receipt"),
            Err(error) => return Err(error.into()),
        }
    }
    println!(
        "{} jobs revealed by this node: {included} proven to be in a batch, {missing} not proven",
        jobs.len()
    );
    Ok(if missing == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Checks every draw the job board has published (spec section 4: "Each draw
/// is published, so anyone can recheck that nodes were not hand-picked"). Each
/// must name the beacon round that was newest when it was made, with the
/// randomness the beacon itself gives for that round, and grant the lease to
/// the node the rule draws. Each draw a batch of the settlement log covers
/// must be among those the batch fingerprints, which shows that the control
/// plane published it; newer draws are not yet covered.
fn check_draws(client: &Client) -> Result<ExitCode, Box<dyn Error>> {
    let log = client.settlement_log()?;
    let mut rounds = Rounds::default();
    let (mut from, mut confirming, mut audits, mut filled) = (0, 0, 0, 0);
    let mut hashes = Vec::new();
    loop {
        let page = client.draws(from)?;
        for text in &page.draws {
            let draw = rounds
                .check(text)
                .map_err(|fault| format!("draw {from} does not hold: {fault}"))?;
            hashes.push(draw_hash(text));
            let pickup = match draw.pickup {
                Pickup::First => "first pickup".to_string(),
                Pickup::Second => "second pickup".to_string(),
                Pickup::Audit => "audit".to_string(),
                Pickup::Referee => "referee for a dispute".to_string(),
                Pickup::Stage { lease, stage, .. } => {
                    format!("stage {stage} for lease {lease}")
                }
            };
            let market = if draw.bid.is_some() {
                ", filling a bid"
            } else {
                ""
            };
            println!(
                "job {}  round {}, {pickup} drawn from {} nodes{market}",
                draw.job_id,
                draw.beacon.round,
                draw.eligible.len(),
            );
            confirming += u64::from(draw.pickup == Pickup::Second);
            audits += u64::from(draw.pickup == Pickup::Audit);
            filled += u64::from(draw.bid.is_some());
            from += 1;
        }
        if page.draws.len() < MAX_LOG_PAGE {
            break;
        }
    }
    let mut covered = 0;
    for (number, batch) in log.batches().iter().enumerate() {
        let (start, end) = log.draws_of(number as u64).expect("the batch exists");
        let Some(drawn) = hashes.get(start as usize..end as usize) else {
            return fail(&format!(
                "batch {number} covers draws the control plane did not hand over"
            ));
        };
        if merkle::list_root(drawn) != batch.draws {
            return fail(&format!(
                "batch {number} fingerprints other draws than those published"
            ));
        }
        covered = end;
    }
    println!(
        "{from} draws hold: {confirming} second pickups, {audits} audits, {filled} filled bids"
    );
    println!("{covered} of them are in signed batches of the settlement log");
    Ok(ExitCode::SUCCESS)
}

/// Whether the receipt in `inclusion` is of `job_id` and holds this device's
/// signed report of that job.
fn is_report_of(inclusion: &Inclusion, job_id: &JobId, client: &Client) -> bool {
    let Ok(receipt) = serde_json::from_str::<Receipt>(&inclusion.receipt) else {
        return false;
    };
    let mine = |report: &&decomp_core::Envelope| report.signer == client.device_key();
    let reported = receipt.results.iter().find(mine).and_then(|report| {
        let request: Request = report.open().ok()?;
        match request.call {
            Call::RevealResult { result, .. } => Some(result.job_id),
            _ => None,
        }
    });
    receipt.job_id == *job_id && reported == Some(*job_id)
}

fn show_account(client: &Client) -> Result<ExitCode, Box<dyn Error>> {
    let status = client.account()?;
    println!("account  {}", status.account.id);
    println!("ledger   {}", status.account.ledger_address);
    for device in status
        .account
        .devices
        .iter()
        .filter(|device| !device.revoked)
    {
        println!("device   {}", device.key);
    }
    for controller in &status.account.controllers {
        match controller {
            Controller::Key { key } => println!("controller  key {key}"),
            Controller::Passkey { key } => println!("controller  passkey {key}"),
            Controller::Wallet { key } => println!("controller  wallet {key}"),
            Controller::Guardians { accounts, required } => {
                println!("controller  {required} of these guardian accounts:");
                for guardian in accounts {
                    println!("            {guardian}");
                }
            }
        }
    }
    if status.account.methods_required > 1 {
        let required = status.account.methods_required;
        println!("            {required} controllers must agree to a recovery");
    }
    for recovery in &status.account.recoveries {
        println!(
            "recovery waiting  device {}, takes effect in {} seconds",
            recovery.device_key,
            recovery.effective_at.saturating_sub(unix_now())
        );
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
        "         reliability {:.3}, availability {:.3}, hardware fit {:.3}, bandwidth {:.3}",
        standing.reliability, standing.availability, standing.fit, standing.bandwidth
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
    if balance.cashing_out > Credits::ZERO {
        println!(
            "         {} set aside to be cashed out",
            balance.cashing_out
        );
    }
    println!(
        "         {} are starter credits or burned tokens, which cannot be staked or cashed out",
        balance.granted
    );
    println!("tokens   {}", status.tokens);
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
/// The files an app publishes besides its modules.
struct AppFiles {
    client: Option<PathBuf>,
    metadata: Option<PathBuf>,
}

fn publish(
    client: &Client,
    name: String,
    modules: &[PathBuf],
    sponsor_daily: Option<Credits>,
    files: AppFiles,
) -> Result<ExitCode, Box<dyn Error>> {
    let mut hashes = Vec::new();
    for path in modules {
        let bytes = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let hash = store::put(client, &bytes, MAX_STORAGE_DAYS)?;
        println!("module   {hash}  {}", path.display());
        hashes.push(hash);
    }
    let upload = |path: &Option<PathBuf>| -> Result<Option<ContentHash>, Box<dyn Error>> {
        let Some(path) = path else { return Ok(None) };
        let bytes = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Some(store::put(client, &bytes, MAX_STORAGE_DAYS)?))
    };
    let status = client.publish_app(AppManifest {
        name,
        modules: hashes,
        sponsorship: sponsor_daily.map(|per_account_daily| Sponsorship { per_account_daily }),
        client: upload(&files.client)?,
        metadata: upload(&files.metadata)?,
    })?;
    println!("app ID   {}", status.account.id);
    if let Some(app) = &status.account.app {
        show_app(app);
    }
    Ok(ExitCode::SUCCESS)
}

/// What every result reports (spec section 7): "the privacy route used, how the
/// result was verified, what it cost, how long it took, and whether an enclave
/// was involved".
struct Report {
    route: String,
    checked: &'static str,
    fee: Credits,
    time: Duration,
    enclave: bool,
}

impl Report {
    fn show(&self) {
        println!("route   {}", self.route);
        println!("checked {}", self.checked);
        println!("fee     {} credits", self.fee);
        println!("time    {:.1} s", self.time.as_secs_f64());
        println!("enclave {}", if self.enclave { "yes" } else { "no" });
    }
}

/// How a result that nodes produced was verified, in words. A result nothing
/// was compared with is said to be so (spec section 7: "A result that is
/// unverified or under dispute is flagged as such").
fn how_checked(verified: Verification) -> &'static str {
    match verified {
        Verification::Confirmed => "confirmed: a second node's result matched",
        Verification::Refereed => {
            "refereed: nodes disagreed, and referee nodes agreed with one of them"
        }
        Verification::AlwaysOn => {
            "always-on: no node finished in time, so the control plane ran it"
        }
        Verification::Unconfirmed => {
            "unconfirmed: one node's result, accepted on its reputation and open to audit"
        }
    }
}

/// The limits `submit` puts on a job, who pays for it, and whose nodes it is
/// kept from.
struct Limits {
    deadline_secs: u64,
    verify: Verify,
    fallback: Option<Fallback>,
    class: JobClass,
    split_bytes: Option<u64>,
    sponsor: Option<AccountId>,
    state: Option<ShardRef>,
    follow_up_credits: Credits,
    keep_from: Vec<PublicKey>,
    apart_from: Vec<JobId>,
}

/// Waits until a job closes. Gives its receipt, or says why it has none.
fn wait_for(
    client: &Client,
    job_id: JobId,
) -> Result<Result<Box<Receipt>, &'static str>, ClientError> {
    loop {
        match client.job(job_id)? {
            JobStatus::Submitted
            | JobStatus::Leased
            | JobStatus::Committed
            | JobStatus::Disputed => sleep(JOB_POLL),
            JobStatus::Settled { receipt } => return Ok(Ok(receipt)),
            JobStatus::Rejected => return Ok(Err("the nodes disagreed and none was right")),
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
            Outcome::Pieces { outputs } => format!(
                "{} pieces' outputs, fee {} credits",
                outputs.len(),
                receipt.fee.total
            ),
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
    let records = records::decode(&store::fetch(client, contents)?)
        .ok_or("the shard's contents are not a set of records")?;
    println!("contents {contents}");
    for (key, value) in &records {
        let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
        println!("{} = {}", text(key), text(value));
    }
    Ok(ExitCode::SUCCESS)
}

fn submit(
    client: &Client,
    dir: &Path,
    job: JobArgs,
    limits: Limits,
) -> Result<ExitCode, Box<dyn Error>> {
    let files = read_job(&job)?;
    let sandbox = Sandbox::new();
    let declared = sandbox.declared(&files.module);
    if let Some(signature) = declared.filter(|signature| signature.privacy == Privacy::Private) {
        let follow_ups = limits.follow_up_credits > Credits::ZERO;
        if limits.state.is_some() || limits.split_bytes.is_some() || follow_ups {
            let message = "a private job cannot be handed state, split by byte count, or given credits for follow-up jobs";
            return Err(message.into());
        }
        // A private job takes "the first route that fits" (spec section 7). It
        // fits this device if, at the pace the job board measured for it
        // (its hardware fit), it finishes before its deadline.
        let pace = client.standing().map_or(0.0, |standing| {
            standing.fit * decomp_core::REFERENCE_UNITS_PER_SEC
        });
        let fits_device = job.compute_cap <= ANY_DEVICE_UNITS
            || job.compute_cap as f64 <= pace * limits.deadline_secs as f64;
        return match privacy::route(&signature, job.compute_cap, fits_device)? {
            PrivateRoute::Local => run_here(&sandbox, dir, &job, &files, client.control_key()),
            PrivateRoute::PrivateTotals => submit_shares(client, &job, files, &signature, limits),
            PrivateRoute::EncryptedCompute => {
                submit_encrypted(client, &job, files, &signature, limits)
            }
        };
    }
    let Limits {
        deadline_secs,
        verify,
        fallback,
        class,
        split_bytes,
        sponsor,
        state,
        follow_up_credits,
        keep_from,
        apart_from,
    } = limits;
    let JobFiles {
        module,
        input,
        blobs,
    } = files;
    let always_on = fallback == Some(Fallback::AlwaysOn);
    let split = match split_bytes {
        Some(chunk_bytes) => {
            let pieces = cut(&input, chunk_bytes as usize)
                .into_iter()
                .map(|piece| put_job_blob(client, piece.to_vec(), always_on))
                .collect::<Result<_, _>>()?;
            Some(Split { pieces })
        }
        None => None,
    };
    let mut manifest = Manifest {
        module: put_job_blob(client, module, always_on)?,
        input: put_job_blob(client, input, always_on)?,
        compute_cap: job.compute_cap,
        deadline: unix_now() + deadline_secs,
        class,
        slots: 1,
        verify,
        fallback,
        split,
        max_fee: Credits::ZERO,
        sponsor,
        reads: blobs
            .into_values()
            .map(|blob| put_job_blob(client, blob, always_on))
            .collect::<Result<_, _>>()?,
        state,
        privacy: Privacy::Open,
        share: None,
        keep_from,
        apart_from,
        models: job.models.clone(),
    };
    // Set aside the most the job can cost at the rate posted now, and what its
    // follow-ups may; what is not spent comes back.
    let quote = client.quote(manifest.clone())?;
    println!(
        "rate    {} µc per unit (tier {}, premium {:.2})",
        quote.rate.micros_per_unit,
        quote.rate.tier,
        quote.rate.premium as f64 / 1000.0
    );
    manifest.max_fee = quote.highest_fee + follow_up_credits;
    println!("module  {}", manifest.module);
    let job_id = client.submit_job(manifest)?;
    println!("job     {job_id}");

    let receipt = match wait_for(client, job_id)? {
        Ok(receipt) => receipt,
        Err(reason) => return fail(reason),
    };
    if receipt.pieces.is_empty() {
        println!(
            "units   {} on each of {} nodes, {} paid runs",
            receipt.units,
            receipt.results.len(),
            receipt.runs
        );
    } else {
        println!(
            "units   {} in all, over {} pieces",
            receipt.units,
            receipt.pieces.len()
        );
    }
    Report {
        route: "open".into(),
        checked: how_checked(receipt.verified),
        fee: receipt.fee.total,
        time: Duration::from_secs(receipt.seconds),
        enclave: receipt.enclave,
    }
    .show();
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
            let bytes = store::fetch(client, output)?;
            report_output(&bytes, job.output.as_deref())?;
            Ok(ExitCode::SUCCESS)
        }
        // A split job's output is its pieces' outputs, joined here.
        Outcome::Pieces { outputs } => {
            let mut bytes = Vec::new();
            for output in outputs {
                bytes.extend(store::fetch(client, output)?);
            }
            report_output(&bytes, job.output.as_deref())?;
            Ok(ExitCode::SUCCESS)
        }
        Outcome::Failed { failure } => fail(&failure.to_string()),
    }
}

/// Puts one of a job's blobs where it is run from: on storage nodes, and, for
/// a job that may fall back to always-on, with the control plane as well.
fn put_job_blob(
    client: &Client,
    bytes: Vec<u8>,
    always_on: bool,
) -> Result<ContentHash, Box<dyn Error>> {
    let root = store::put(client, &bytes, store::JOB_DATA_DAYS)?;
    if always_on {
        client.put_blob(bytes)?;
    }
    Ok(root)
}

/// Runs a private job by the private totals route (spec section 7: "Private
/// totals splits each input into shares held by nodes on different accounts").
/// The input is cut into shares here, each share is submitted as a job of its
/// own, and their outputs are added together here. Only this device sees the
/// input or the result.
fn submit_shares(
    client: &Client,
    job: &JobArgs,
    files: JobFiles,
    signature: &Signature,
    limits: Limits,
) -> Result<ExitCode, Box<dyn Error>> {
    let inputs = shares::cut(signature, &files.input)?;
    let always_on = limits.fallback == Some(Fallback::AlwaysOn);
    let module = put_job_blob(client, files.module, always_on)?;
    let reads = (files.blobs.into_values())
        .map(|blob| put_job_blob(client, blob, always_on))
        .collect::<Result<Vec<_>, _>>()?;
    let deadline = unix_now() + limits.deadline_secs;
    let set = ShareSet::generate();
    println!("module  {module}");

    let mut jobs = Vec::new();
    for (index, input) in (0..).zip(inputs) {
        let mut manifest = Manifest {
            module,
            input: put_job_blob(client, input, always_on)?,
            compute_cap: job.compute_cap,
            deadline,
            class: limits.class,
            slots: 1,
            verify: limits.verify,
            fallback: limits.fallback,
            split: None,
            max_fee: Credits::ZERO,
            sponsor: limits.sponsor,
            reads: reads.clone(),
            state: None,
            privacy: Privacy::Private,
            share: Some(Share { set, index }),
            keep_from: limits.keep_from.clone(),
            apart_from: limits.apart_from.clone(),
            models: job.models.clone(),
        };
        manifest.max_fee = client.quote(manifest.clone())?.highest_fee;
        jobs.push(client.submit_job(manifest)?);
    }

    let mut receipts = Vec::new();
    for (index, job_id) in jobs.into_iter().enumerate() {
        let receipt = match wait_for(client, job_id)? {
            Ok(receipt) => receipt,
            Err(reason) => return fail(reason),
        };
        println!(
            "share   {} of {SHARES}: job {job_id}, {} units on each of {} nodes",
            index + 1,
            receipt.units,
            receipt.results.len()
        );
        receipts.push(receipt);
    }
    // What the shares report together: the weakest check any of them had, what
    // they cost in all, and, since they run side by side, the longest any took.
    let checks = receipts.iter().map(|receipt| receipt.verified);
    let times = receipts.iter().map(|receipt| receipt.seconds);
    Report {
        route: format!("private totals: {SHARES} shares, each run by other accounts' nodes"),
        checked: how_checked(checks.min().unwrap_or(Verification::Unconfirmed)),
        fee: (receipts.iter()).fold(Credits::ZERO, |sum, receipt| sum + receipt.fee.total),
        time: Duration::from_secs(times.max().unwrap_or(0)),
        enclave: receipts.iter().any(|receipt| receipt.enclave),
    }
    .show();

    let mut outputs = Vec::new();
    for receipt in receipts {
        match receipt.outcome {
            Outcome::Completed { output, .. } => outputs.push(store::fetch(client, output)?),
            Outcome::Failed { failure } => return fail(&failure.to_string()),
            Outcome::Pieces { .. } => return fail("a share is never split"),
        }
    }
    report_output(&shares::join(signature, &outputs)?, job.output.as_deref())?;
    Ok(ExitCode::SUCCESS)
}

/// Runs a private job by the encrypted compute route (spec section 7:
/// "Encrypted compute uses homomorphic encryption, so a node computes on values
/// it cannot read"). A key is made here for this one job, the private values in
/// the input are encrypted under it, and the job is submitted on that input
/// with the part of the key that nodes compute with. The result is read here
/// with the part of the key that never left.
fn submit_encrypted(
    client: &Client,
    job: &JobArgs,
    files: JobFiles,
    signature: &Signature,
    limits: Limits,
) -> Result<ExitCode, Box<dyn Error>> {
    let Encrypted {
        key,
        computing_key,
        input,
    } = encrypted::encrypt(signature, &files.input)?;
    let always_on = limits.fallback == Some(Fallback::AlwaysOn);
    let mut reads = vec![put_job_blob(client, computing_key, always_on)?];
    for blob in files.blobs.into_values() {
        reads.push(put_job_blob(client, blob, always_on)?);
    }
    let mut manifest = Manifest {
        module: put_job_blob(client, files.module, always_on)?,
        input: put_job_blob(client, input, always_on)?,
        compute_cap: job.compute_cap,
        deadline: unix_now() + limits.deadline_secs,
        class: limits.class,
        slots: 1,
        verify: limits.verify,
        fallback: limits.fallback,
        split: None,
        max_fee: Credits::ZERO,
        sponsor: limits.sponsor,
        reads,
        state: None,
        privacy: Privacy::Private,
        share: None,
        keep_from: limits.keep_from,
        apart_from: limits.apart_from,
        // The encrypted route computes on encrypted values, which no model reads.
        models: Vec::new(),
    };
    manifest.max_fee = client.quote(manifest.clone())?.highest_fee;
    println!("module  {}", manifest.module);
    let job_id = client.submit_job(manifest)?;
    println!("job     {job_id}");

    let receipt = match wait_for(client, job_id)? {
        Ok(receipt) => receipt,
        Err(reason) => return fail(reason),
    };
    println!(
        "units   {} on each of {} nodes",
        receipt.units,
        receipt.results.len()
    );
    Report {
        route: "encrypted compute: nodes ran it on values only this device can read".into(),
        checked: how_checked(receipt.verified),
        fee: receipt.fee.total,
        time: Duration::from_secs(receipt.seconds),
        enclave: receipt.enclave,
    }
    .show();
    match receipt.outcome {
        Outcome::Completed { output, .. } => {
            let returned = store::fetch(client, output)?;
            let output = encrypted::decrypt(signature, &key, &returned)?;
            report_output(&output, job.output.as_deref())?;
            Ok(ExitCode::SUCCESS)
        }
        Outcome::Failed { failure } => fail(&failure.to_string()),
        Outcome::Pieces { .. } => fail("an encrypted job is never split"),
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
    dir: &Path,
    job: &JobArgs,
    files: &JobFiles,
    control_key: Option<PublicKey>,
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
    // A private job run here is this device's own user's, so what was sealed to
    // this device is the job's to open. An open job gets no key here, as it
    // gets none on a node.
    let opening_key = if private {
        device::opening_key_if_made(dir)?
    } else {
        None
    };
    // Off the network a job has no ID to draw its randomness from, so the seed is
    // fixed: every local run of a job draws the same bytes.
    let started = Instant::now();
    let run = sandbox.run(&Job {
        module,
        input,
        compute_cap: job.compute_cap,
        seed: [0; 32],
        blobs,
        // Nor is there any state to hand it.
        state: None,
        opening_key: opening_key.as_ref(),
        control_key,
        models: &job.models,
        // Off the network there is no one to check the models' answers, so
        // they are generated here.
        model_host: (!job.models.is_empty()).then(|| {
            let context = CallContext::local(&decomp_core::SigningKey::generate());
            Arc::new(Generator::new(context)) as Arc<dyn decomp_sandbox::Models>
        }),
    })?;
    let took = started.elapsed();

    println!("module  {}", merkle::root(module));
    if let Some(signature) = sandbox.signature(module)? {
        println!("types   {}", String::from_utf8_lossy(&signature));
    }
    println!("units   {} of {}", run.units, job.compute_cap);
    if private {
        Report {
            route: "local: run on this device, and sent to no node".into(),
            // The spec's table: "Not needed; the device belongs to the user".
            checked: "not needed: this device is its user's own",
            fee: Credits::ZERO,
            time: took,
            enclave: false,
        }
        .show();
    }
    let checkpoints = ContentHash::of(&run.checkpoints.concat());
    println!(
        "checks  {} checkpoints, list {checkpoints}",
        run.checkpoints.len()
    );
    match run.result {
        Ok(produced) => {
            for blob in &produced.blobs {
                println!("stored  {} ({} bytes)", merkle::root(blob), blob.len());
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
        // Named by its root, as it is stored and as a job asks for it.
        let root = merkle::root(&bytes);
        println!("read    {root}  {}", path.display());
        blobs.insert(root, bytes);
    }
    Ok(JobFiles {
        module: read(&job.module)?,
        input,
        blobs,
    })
}

fn report_output(bytes: &[u8], save_to: Option<&Path>) -> Result<(), Box<dyn Error>> {
    println!("output  {} ({} bytes)", merkle::root(bytes), bytes.len());
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

fn parse_verify(text: &str) -> Result<Verify, String> {
    match text {
        "opportunistic" => Ok(Verify::Opportunistic),
        "required" => Ok(Verify::Required),
        "none" => Ok(Verify::None),
        _ => Err("verify is opportunistic, required, or none".into()),
    }
}

fn parse_model(text: &str) -> Result<ContentHash, String> {
    APPROVED_MODELS
        .iter()
        .find(|model| model.name == text || model.id.to_string() == text)
        .map(|model| model.id)
        .ok_or_else(|| {
            let names: Vec<&str> = APPROVED_MODELS.iter().map(|model| model.name).collect();
            format!("the approved models are {}", names.join(", "))
        })
}

fn parse_class(text: &str) -> Result<JobClass, String> {
    match text {
        "menial" => Ok(JobClass::Menial),
        "data" => Ok(JobClass::Data),
        "inference" => Ok(JobClass::Inference),
        _ => Err("a job class is menial, data, or inference".into()),
    }
}

fn parse_hours(text: &str) -> Result<Option<(u8, u8)>, String> {
    if text == "any" {
        return Ok(None);
    }
    let (start, end) = text
        .split_once('-')
        .ok_or("write hours as START-END, such as 22-6, or any")?;
    let hour = |part: &str| part.trim().parse::<u8>().map_err(|_| "an hour is 0 to 24");
    Ok(Some((hour(start)?, hour(end)?)))
}

/// Shows a device's work preferences, changing those given first.
fn preferences(
    client: &Client,
    device: Option<PublicKey>,
    set: PreferenceArgs,
) -> Result<ExitCode, Box<dyn Error>> {
    let device = device.unwrap_or_else(|| client.device_key());
    let mut preferences: WorkPreferences = client.work_preferences(device)?;
    let PreferenceArgs {
        contributing,
        classes,
        min_tier,
        max_tier,
        floor,
        slots,
        hours,
        on_battery,
        on_metered,
        market_only,
    } = set;
    let changing = contributing.is_some()
        || classes.is_some()
        || min_tier.is_some()
        || max_tier.is_some()
        || floor.is_some()
        || slots.is_some()
        || hours.is_some()
        || on_battery.is_some()
        || on_metered.is_some()
        || market_only.is_some();
    if changing {
        let p = &mut preferences;
        p.contributing = contributing.unwrap_or(p.contributing);
        p.classes = classes.unwrap_or_else(|| p.classes.clone());
        p.min_tier = min_tier.unwrap_or(p.min_tier);
        p.max_tier = max_tier.unwrap_or(p.max_tier);
        p.floor_micros_per_unit = floor.unwrap_or(p.floor_micros_per_unit);
        p.slots = slots.unwrap_or(p.slots);
        p.hours = hours.unwrap_or(p.hours);
        p.on_battery = on_battery.unwrap_or(p.on_battery);
        p.on_metered = on_metered.unwrap_or(p.on_metered);
        p.market_only = market_only.unwrap_or(p.market_only);
        preferences = client.set_work_preferences(device, preferences)?;
    }
    let p = &preferences;
    println!("device       {device}");
    println!("contributing {}", p.contributing);
    println!("classes      {:?}", p.classes);
    println!("tiers        {} to {}", p.min_tier, p.max_tier);
    println!("rate floor   {} µc per unit", p.floor_micros_per_unit);
    println!("slots        {}", p.slots);
    match p.hours {
        Some((start, end)) => println!("hours        {start}:00 to {end}:00, local time"),
        None => println!("hours        any"),
    }
    println!("on battery   {}", p.on_battery);
    println!("on metered   {}", p.on_metered);
    println!("market only  {}", p.market_only);
    Ok(ExitCode::SUCCESS)
}

fn show_rates(client: &Client) -> Result<ExitCode, Box<dyn Error>> {
    let rates = client.rates()?;
    println!("class      tier  µc/unit  premium  waiting  asking");
    for rate in rates.rates {
        println!(
            "{:<10} {:>4}  {:>7}  {:>7.2}  {:>7}  {:>6}",
            format!("{:?}", rate.class).to_lowercase(),
            rate.tier,
            rate.micros_per_unit,
            rate.premium as f64 / 1000.0,
            rate.waiting,
            rate.asking
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// Makes an account key and saves it, without making the account.
fn new_account_key(file: &Path) -> Result<ExitCode, Box<dyn Error>> {
    if device::key_file(file)?.is_some() {
        return Err(format!("{} already holds a key", file.display()).into());
    }
    let key = device::create_key_file(file)?;
    println!("account  {}", AccountId::of(&key.public_key()));
    Ok(ExitCode::SUCCESS)
}

/// Makes an account with this device as its first device, for the key in
/// `file`, or for a new key saved there.
fn create_account(dir: &Path, control: &str, file: &Path) -> Result<ExitCode, Box<dyn Error>> {
    let device = match device::load(dir)? {
        Some(key) => key,
        None => device::create(dir)?,
    };
    let (account_key, made_here) = match device::key_file(file)? {
        Some(key) => (key, false),
        None => (device::create_key_file(file)?, true),
    };
    let status = match Client::new(control, account_key).create_account(device.public_key()) {
        Ok(status) => status,
        Err(error) => {
            // A key made here for an account that was never made is of no
            // use, and would block the next attempt.
            if made_here {
                let _ = fs::remove_file(file);
            }
            return Err(error.into());
        }
    };
    // So that other devices can send to this one from now on.
    Client::new(control, device).publish_sealing_key(device::opening_key(dir)?.sealing_key())?;
    println!("account  {}", status.account.id);
    println!("device   {}", status.account.devices[0].key);
    println!(
        "Keep {} offline: it approves new devices and cash-outs.",
        file.display()
    );
    Ok(ExitCode::SUCCESS)
}
