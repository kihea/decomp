//! Everything the local network's control plane knows, and where each part
//! is kept. On the Internet Computer each part lives in the stable memory of
//! the canister that owns it (spec, "Planes and components"); here each is a
//! log file in the data directory.
//!
//! | Part | Canister | Kept in | Survives a restart |
//! | --- | --- | --- | --- |
//! | Account records | Accounts | `ops.jsonl` | Yes |
//! | Balances | Accounts | `ops.jsonl` (starter grants) and `ledger.jsonl` | Yes |
//! | When each account appeared, and what it earned and spent | Accounts | `activity.jsonl` | Yes |
//! | Names | Registry | `registry.jsonl` | Yes |
//! | Records: the current version of each key | Records | `records.jsonl` | Yes |
//! | Node scores | Job board | `scores.jsonl` | Yes |
//! | Work preferences and standing bids | Job board | `market.jsonl` | Yes |
//! | Receipts | Receipts | `receipts.jsonl` | Yes |
//! | The settlement log: batches, signed by the control plane | Receipts | `settlement.jsonl` | Yes |
//! | The draws of closed jobs, signed by the control plane | Job board | `draws.jsonl` | Yes |
//! | Which nodes hold which blobs | Storage placement | `storage.jsonl` | Yes |
//! | Relay channels | Relay placement | `relays.jsonl` | Yes |
//! | The sealing key each device published | Accounts | `sealing.jsonl` | Yes |
//! | What each shard of app state holds now | Records | `state.jsonl` | Yes |
//! | Blobs | Storage nodes; this process holds a copy of job inputs and outputs | `blobs/` | Yes |
//! | Open jobs, leases, commitments, nodes asking, unanswered challenges | Job board | Memory | No, by design (spec, "Data management") |

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io;
use std::path::Path;

use decomp_core::Records as AccountRecords;
use decomp_core::market::{BALANCED_LOOKBACK_SECS, fallback_rate};
use decomp_core::{
    Account, AccountId, AccountStanding, AccountStatus, Accounts, Activity, Beacon, Bid, BidOrder,
    CashOut, Challenge, Closure, Conduct, ContentHash, Credits, Directory, Envelope, EnvelopeError,
    FallBackJob, FollowUps, Hosts, Inclusion, IncomingChannels, JobError, JobId, JobResult,
    JobStatus, Jobs, LeasesGranted, Ledger, LedgerAddress, LedgerEntry, LedgerError, Located,
    LogPage, MAX_APART_FROM, MAX_KEPT_FROM, MAX_LOG_PAGE, MAX_PIECES, MAX_READS,
    MAX_SHARD_NAME_BYTES, Manifest, Merge, NAME_DEPOSIT, NameError, NameRecord, OpError, Opening,
    Outcome, PROBATION_FORFEIT_PERCENT, Payout, Penalty, Placement, Posted, PrivacyError,
    PublicKey, Quote, Receipt, Record, RecordError, Registry, RegistryEntry, RelayEntry,
    RelayError, Relays, Resolved, Route, ScoreEvent, Scores, SealingKeys, SettlementLog, ShardRef,
    SigningKey, Splits, SponsorError, Sponsored, Standing, Standings, StateEntry, States, Storage,
    StorageEntry, StorageError, Storing, Through, WorkPreferences, channel_id, compute_fee, merkle,
    privacy, settlement,
};
use decomp_sandbox::Sandbox;
use decomp_schema::records::{self, Records};
use decomp_schema::{Mismatch, Signature};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::blobs::Blobs;
use crate::key;
use crate::log::Log;
use crate::referee::{Case, Verdict};

/// Why a blob was not accepted for storage.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error("the receiving key is not a device in any account")]
    UnknownDevice,
}

/// Why a channel was not opened.
#[derive(Debug, thiserror::Error)]
pub enum OpenRelayError {
    #[error(transparent)]
    Relay(#[from] RelayError),
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error("the receiving key is not a device in any account")]
    UnknownDevice,
}

/// Why no proof was given that a job's receipt is in a batch.
#[derive(Debug, thiserror::Error)]
pub enum InclusionError {
    #[error("there is no receipt for this job")]
    NoReceipt,
    #[error("the job's receipt is not in a batch yet")]
    NotBatched,
    #[error(transparent)]
    Storage(#[from] io::Error),
}

/// Why a name was not registered or changed.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error(transparent)]
    Name(#[from] NameError),
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error("only an account that has published an app can hold a name")]
    NoApp,
    #[error("only a reviewer may do this")]
    NotReviewer,
}

/// Why a job was not accepted.
#[derive(Debug, thiserror::Error)]
pub enum SubmitError {
    #[error(transparent)]
    Job(#[from] JobError),
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error(transparent)]
    Storage(#[from] io::Error),
    #[error("no blob with hash {0} has been stored")]
    UnknownBlob(ContentHash),
    #[error("max_fee must cover the highest possible fee, {0} credits")]
    FeeCapTooLow(Credits),
    #[error("the split rule would cut the job into more than {MAX_PIECES} pieces")]
    TooManyPieces,
    #[error("a job may be given at most {MAX_READS} blobs to read")]
    TooManyReads,
    #[error(
        "a job may be kept from the accounts of at most {MAX_KEPT_FROM} keys and from the nodes of at most {MAX_APART_FROM} earlier jobs"
    )]
    KeptFromTooMany,
    #[error("{0}")]
    TypeMismatch(Mismatch),
    #[error("a job with typed input cannot be split by byte count")]
    TypedJobSplitByBytes,
    #[error(transparent)]
    Sponsor(#[from] SponsorError),
    #[error("a job may be handed an app's state only if it runs one of that app's modules")]
    StateNotAllowed,
    #[error("a shard's name is between 1 and {MAX_SHARD_NAME_BYTES} bytes")]
    BadShardName,
    #[error("a job that is handed state cannot be split")]
    SplitWithState,
    #[error(transparent)]
    Privacy(#[from] PrivacyError),
}

/// The settings a local network is started with.
pub struct Settings {
    /// How long a stored blob goes between challenges, in seconds.
    pub challenge_interval: u64,
    /// How long a recovery waits before it takes effect, in seconds.
    pub recovery_wait: u64,
    /// The accounts allowed to review apps and assign held names.
    pub reviewers: Vec<AccountId>,
    /// Brand names held until a reviewer assigns them.
    pub reserved_names: Vec<String>,
}

/// What `market.jsonl` records: changes to work preferences and bids.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum MarketEntry {
    Preferences {
        device: PublicKey,
        preferences: WorkPreferences,
    },
    Bid {
        account: AccountId,
        id: ContentHash,
        order: BidOrder,
        at: u64,
    },
    Cancel {
        account: AccountId,
        id: ContentHash,
    },
}

/// What `activity.jsonl` records: when each account appeared, and credits it
/// earned and spent, with the time.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ActivityEntry {
    Created {
        account: AccountId,
        at: u64,
    },
    Earned {
        account: AccountId,
        amount: Credits,
        at: u64,
    },
    Spent {
        account: AccountId,
        amount: Credits,
        at: u64,
    },
}

pub struct ControlPlane {
    pub accounts: Accounts,
    ops: Log,
    ledger: Ledger,
    ledger_log: Log,
    scores: Scores,
    scores_log: Log,
    pub blobs: Blobs,
    pub jobs: Jobs,
    /// What each app has set aside today for each account's jobs.
    sponsored: Sponsored,
    splits: Splits,
    /// What each module says its job takes and returns, read once per module.
    /// `None` for a module that does not describe itself.
    signatures: HashMap<ContentHash, Option<Signature>>,
    receipts: Log,
    /// The hash of each receipt's text, in the order the receipts were made,
    /// and each job's place in that order. Batches are made from these.
    receipt_hashes: Vec<ContentHash>,
    receipt_of: HashMap<JobId, u64>,
    /// The key the settlement log and draws are signed with, which is also the
    /// treasury's.
    control: SigningKey,
    settlement: SettlementLog,
    settlement_log: Log,
    /// The draws of jobs that have closed, each signed by the control plane.
    draws_log: Log,
    registry: Registry,
    registry_log: Log,
    records: AccountRecords,
    records_log: Log,
    reviewers: Vec<AccountId>,
    market_log: Log,
    activity_log: Log,
    /// When each account appeared.
    created: HashMap<AccountId, u64>,
    /// Who set aside each open job's fee.
    escrow_payer: HashMap<JobId, AccountId>,
    /// Jobs to run as an always-on fallback.
    fall_backs: Vec<(JobId, FallBackJob)>,
    /// Credits each account earned and spent, with the time, oldest first.
    activity: Vec<ActivityEntry>,
    /// The nodes other devices can connect to, which hold and relay data.
    hosts: Hosts,
    storage: Storage,
    storage_log: Log,
    relays: Relays,
    relays_log: Log,
    states: States,
    states_log: Log,
    sealing_keys: SealingKeys,
    sealing_log: Log,
    /// How long a stored blob goes between challenges, in seconds.
    challenge_interval: u64,
}

impl ControlPlane {
    pub fn open(data_dir: &Path, settings: Settings) -> io::Result<Self> {
        let Settings {
            challenge_interval,
            recovery_wait,
            reviewers,
            reserved_names,
        } = settings;
        fs::create_dir_all(data_dir)?;

        let mut accounts = Accounts::waiting(recovery_wait);
        let mut ledger = Ledger::default();
        let ops = replay(&data_dir.join("ops.jsonl"), |envelope: Envelope| {
            let account = accounts.apply(&envelope).map_err(|e| e.to_string())?;
            ledger.open(account.id);
            Ok(())
        })?;
        let mut ledger_log = replay(&data_dir.join("ledger.jsonl"), |entry: LedgerEntry| {
            ledger.apply(&entry).map_err(|e| e.to_string())
        })?;
        let mut scores = Scores::default();
        let scores_log = replay(&data_dir.join("scores.jsonl"), |event: ScoreEvent| {
            scores.apply(&event);
            Ok(())
        })?;

        let mut storage = Storage::default();
        let storage_log = replay(&data_dir.join("storage.jsonl"), |entry: StorageEntry| {
            storage.apply(&entry);
            Ok(())
        })?;

        let mut relays = Relays::default();
        let relays_log = replay(&data_dir.join("relays.jsonl"), |entry: RelayEntry| {
            relays.apply(&entry);
            Ok(())
        })?;

        let mut states = States::default();
        let states_log = replay(&data_dir.join("state.jsonl"), |entry: StateEntry| {
            states.apply(&entry);
            Ok(())
        })?;

        let mut sealing_keys = SealingKeys::default();
        let sealing_log = replay(&data_dir.join("sealing.jsonl"), |proof: Envelope| {
            sealing_keys.apply(&proof).map_err(|e| e.to_string())?;
            Ok(())
        })?;

        let mut registry = Registry::default();
        for name in reserved_names {
            registry.apply(&RegistryEntry::Reserve { name });
        }
        let registry_log = replay(&data_dir.join("registry.jsonl"), |entry: RegistryEntry| {
            registry.apply(&entry);
            Ok(())
        })?;

        let mut records = AccountRecords::default();
        let records_log = replay(&data_dir.join("records.jsonl"), |record: Record| {
            records.apply(record);
            Ok(())
        })?;

        let mut jobs = Jobs::default();
        let market_log = replay(&data_dir.join("market.jsonl"), |entry: MarketEntry| {
            match entry {
                MarketEntry::Preferences {
                    device,
                    preferences,
                } => jobs
                    .set_preferences(device, preferences)
                    .map_err(|e| e.to_string()),
                MarketEntry::Bid {
                    account,
                    id,
                    order,
                    at,
                } => {
                    // A bid that has lapsed since is refused here, and that is fine.
                    let _ = jobs.place_bid(account, order, id, at);
                    Ok(())
                }
                MarketEntry::Cancel { account, id } => {
                    let _ = jobs.cancel_bid(&account, &id);
                    Ok(())
                }
            }
        })?;

        let mut created = HashMap::new();
        let mut activity = Vec::new();
        let activity_log = replay(&data_dir.join("activity.jsonl"), |entry: ActivityEntry| {
            if let ActivityEntry::Created { account, at } = entry {
                created.insert(account, at);
            } else {
                activity.push(entry);
            }
            Ok(())
        })?;

        let receipts = Log::open(&data_dir.join("receipts.jsonl"))?;
        let mut receipt_hashes = Vec::new();
        let mut receipt_of = HashMap::new();
        for line in receipts.lines()? {
            let line = line?;
            let Named { job_id } = serde_json::from_str(&line)?;
            receipt_of.insert(job_id, receipt_hashes.len() as u64);
            receipt_hashes.push(merkle::item_hash(line.as_bytes()));
        }

        let mut settlement = SettlementLog::default();
        let settlement_path = data_dir.join("settlement.jsonl");
        let settlement_log = replay(&settlement_path, |entry: Envelope| {
            settlement.apply(&entry).map_err(|e| e.to_string())
        })?;
        let Through {
            ops: posted_ops,
            entries,
            receipts: posted_receipts,
        } = settlement.through();
        if posted_ops > ops.len() || entries > ledger_log.len() || posted_receipts > receipts.len()
        {
            let message = format!(
                "{} reaches further than the records it was made from",
                settlement_path.display()
            );
            return Err(io::Error::new(io::ErrorKind::InvalidData, message));
        }

        // Open jobs do not survive a restart, so hand back what was set aside for
        // any job that was still open when the network last stopped.
        for job_id in ledger.escrowed_jobs() {
            let refund = LedgerEntry::Refund { job_id };
            ledger
                .apply(&refund)
                .expect("the job has credits set aside");
            ledger_log.append(&refund);
        }

        Ok(Self {
            accounts,
            ops,
            ledger,
            ledger_log,
            scores,
            scores_log,
            blobs: Blobs::open(data_dir.join("blobs"))?,
            jobs,
            sponsored: Sponsored::default(),
            splits: Splits::default(),
            signatures: HashMap::new(),
            receipts,
            receipt_hashes,
            receipt_of,
            control: key::control_key(data_dir)?,
            settlement,
            settlement_log,
            draws_log: Log::open(&data_dir.join("draws.jsonl"))?,
            registry,
            registry_log,
            records,
            records_log,
            reviewers,
            market_log,
            activity_log,
            created,
            escrow_payer: HashMap::new(),
            fall_backs: Vec::new(),
            activity,
            hosts: Hosts::default(),
            storage,
            storage_log,
            relays,
            relays_log,
            states,
            states_log,
            sealing_keys,
            sealing_log,
            challenge_interval,
        })
    }

    /// The key that signs the settlement log and draws, which is also the
    /// treasury's.
    pub fn control_key(&self) -> PublicKey {
        self.control.public_key()
    }

    /// Posts a batch to the settlement log (spec section 6: "Every 10 minutes
    /// (starting value) the receipts canister combines the new receipts into
    /// one fingerprint and posts it with the net balance change per account").
    /// Nothing is posted when there is no new receipt and no balance changed.
    pub fn post_batch(&mut self, now: u64) {
        let through = Through {
            ops: self.ops.len(),
            entries: self.ledger_log.len(),
            receipts: self.receipts.len(),
        };
        let new = &self.receipt_hashes[self.settlement.through().receipts as usize..];
        let holdings = self.holdings();
        let Some(batch) = self.settlement.next_batch(new, &holdings, through) else {
            return;
        };
        self.post(now, Posted::Batch(batch));
    }

    /// Signs an entry and adds it to the settlement log.
    fn post(&mut self, now: u64, posted: Posted) {
        let entry = self.settlement.entry(now, posted);
        let entry = Envelope::seal(&self.control, &entry);
        self.settlement
            .apply(&entry)
            .expect("the control plane's own entry follows the log's rules");
        self.settlement_log.append(&entry);
    }

    /// Takes in the newest round of public randomness. Nodes are drawn for
    /// jobs from it, so until there is one, jobs wait.
    pub fn set_beacon(&mut self, beacon: Beacon) {
        self.jobs.set_beacon(beacon);
    }

    /// Up to `MAX_LOG_PAGE` of the draws published so far, from number `from` on.
    pub fn draws_page(&self, from: u64) -> io::Result<LogPage> {
        let entries = read(self.draws_log.range(from, MAX_LOG_PAGE)?)?;
        Ok(LogPage { entries })
    }

    /// What sits at every ledger address now.
    fn holdings(&self) -> BTreeMap<LedgerAddress, Credits> {
        settlement::addressed(&self.ledger, &self.accounts)
    }

    /// Up to `MAX_LOG_PAGE` entries of the settlement log, from entry `from` on.
    pub fn log_page(&self, from: u64) -> io::Result<LogPage> {
        let entries = read(self.settlement_log.range(from, MAX_LOG_PAGE)?)?;
        Ok(LogPage { entries })
    }

    /// Proof that a job's receipt is in a batch (spec section 6: "Any node can
    /// check that its own receipts are included").
    pub fn inclusion(&self, job_id: &JobId) -> Result<Inclusion, InclusionError> {
        let place = *self
            .receipt_of
            .get(job_id)
            .ok_or(InclusionError::NoReceipt)?;
        let batch = self
            .settlement
            .batch_holding(place)
            .ok_or(InclusionError::NotBatched)?;
        let (start, end) = self
            .settlement
            .receipts_of(batch.number)
            .expect("the batch is in the log");
        let hashes = &self.receipt_hashes[start as usize..end as usize];
        let proof = merkle::list_proof(hashes, place - start);
        let mut line = self.receipts.range(place, 1)?;
        Ok(Inclusion {
            batch: batch.number,
            index: place - start,
            receipt: line
                .pop()
                .expect("a receipt that was counted is in the file"),
            proof: proof.expect("the receipt is one of the batch's"),
        })
    }

    /// Keeps a receipt, as the exact text that batches and proofs are made from.
    fn keep_receipt(&mut self, receipt: &Receipt) {
        let text = serde_json::to_string(receipt).expect("receipts serialize");
        self.receipt_of
            .insert(receipt.job_id, self.receipt_hashes.len() as u64);
        self.receipt_hashes.push(merkle::item_hash(text.as_bytes()));
        self.receipts.append_line(text);
    }

    /// Applies a change to an account that has just arrived, at time `now`,
    /// and records it. A new account gets its starter grant.
    pub fn apply_op(&mut self, envelope: &Envelope, now: u64) -> Result<AccountStatus, OpError> {
        let account = self.accounts.apply_at(envelope, now)?.clone();
        self.ledger.open(account.id);
        self.ops.append(envelope);
        if let std::collections::hash_map::Entry::Vacant(entry) = self.created.entry(account.id) {
            entry.insert(now);
            self.activity_log.append(&ActivityEntry::Created {
                account: account.id,
                at: now,
            });
        }
        Ok(self.status(account))
    }

    /// An account record with its balance, reputation, and when it appeared:
    /// the six fields of spec section 1, "Accounts and identity".
    pub fn status(&self, account: Account) -> AccountStatus {
        let balance = self.ledger.balance(&account.id);
        let reputation = self.reputation(&account);
        let created = self.created.get(&account.id).copied().unwrap_or(0);
        AccountStatus {
            account,
            balance,
            reputation,
            created,
        }
    }

    /// The combined reliability of an account's devices: their average.
    fn reputation(&self, account: &Account) -> f64 {
        let staked = self.ledger.balance(&account.id).staked;
        let scores: Vec<f64> = account
            .devices
            .iter()
            .filter(|device| !device.revoked)
            .map(|device| self.scores.standing(&device.key, staked).reliability)
            .collect();
        if scores.is_empty() {
            return 0.0;
        }
        scores.iter().sum::<f64>() / scores.len() as f64
    }

    /// What the share caps and lease order need to know about an account.
    fn account_standing(&self, account: &AccountId, now: u64) -> AccountStanding {
        let record = self.accounts.get(account);
        let spent_lately = self.activity.iter().rev().any(|entry| {
            matches!(entry, ActivityEntry::Spent { account: spender, at, .. }
                if spender == account && now.saturating_sub(*at) <= BALANCED_LOOKBACK_SECS)
        });
        AccountStanding {
            reliability: record.map_or(0.0, |record| self.reputation(record)),
            created: self.created.get(account).copied().unwrap_or(now),
            balanced: spent_lately,
        }
    }

    /// What an account earned and spent since `since`.
    pub fn activity(&self, account: &AccountId, since: u64) -> Activity {
        let mut activity = Activity::default();
        for entry in &self.activity {
            match entry {
                ActivityEntry::Earned {
                    account: who,
                    amount,
                    at,
                } if who == account && *at >= since => {
                    activity.earned += *amount;
                }
                ActivityEntry::Spent {
                    account: who,
                    amount,
                    at,
                } if who == account && *at >= since => {
                    activity.spent += *amount;
                }
                _ => {}
            }
        }
        activity
    }

    fn note_activity(&mut self, entry: ActivityEntry) {
        self.activity_log.append(&entry);
        self.activity.push(entry);
    }

    /// How far a node is trusted, given its scores and its account's stake.
    pub fn standing(&self, node: &PublicKey, account: &AccountId) -> Standing {
        self.scores
            .standing(node, self.ledger.balance(account).staked)
    }

    /// Records that a node is asking for work and returns what it should be
    /// doing: the leases it holds, results to reveal, blobs to take in, a
    /// storage challenge, and channels to carry.
    pub fn request_lease(
        &mut self,
        node: PublicKey,
        account: AccountId,
        address: Option<String>,
        now: u64,
    ) -> LeasesGranted {
        self.hosts.seen(node, account, address, now);
        let interval = self.challenge_interval;
        let mut jobs = std::mem::take(&mut self.jobs);
        let granted = {
            let node_standing = |node: &PublicKey, account: &AccountId| {
                self.scores
                    .standing(node, self.ledger.balance(account).staked)
            };
            let account_standing = |account: &AccountId| self.account_standing(account, now);
            let standings = Standings {
                node: &node_standing,
                account: &account_standing,
            };
            // `settle` runs after every request, so the contents read here
            // already include what every settled job left behind.
            let states = &self.states;
            let contents = |shard: &ShardRef| states.contents(shard);
            jobs.request_lease(node, account, now, &mut rand::rng(), &standings, &contents)
        };
        self.jobs = jobs;
        LeasesGranted {
            leases: granted.leases,
            reveal: granted.reveal,
            store: self.storage.orders_for(&node),
            drop: self.storage.take_drops(&node),
            challenge: self
                .storage
                .challenge_for(node, now, interval, &mut rand::rng()),
            relay: self.relays.orders_for(&node, now),
        }
    }

    /// What a job would cost if it were submitted now.
    pub fn quote(&self, manifest: &Manifest) -> Result<Quote, SubmitError> {
        let pieces = match manifest.split {
            Some(split) => {
                let input = self.blobs.get(&manifest.input)?.unwrap_or_default();
                split.pieces(input.len() as u64)
            }
            None => 1,
        };
        let per_piece = self.jobs.highest_fee(manifest);
        Ok(Quote {
            rate: self.jobs.posted(manifest),
            fallback_micros_per_unit: fallback_rate(manifest.class),
            highest_fee: Credits::from_micros(per_piece.micros().saturating_mul(pieces)),
        })
    }

    /// A device's work preferences, for any device of the same account.
    pub fn preferences(
        &self,
        asker: &AccountId,
        device: &PublicKey,
    ) -> Result<WorkPreferences, JobError> {
        self.same_account(asker, device)?;
        Ok(self.jobs.preferences(device))
    }

    /// Sets a device's work preferences, for any device of the same account.
    pub fn set_preferences(
        &mut self,
        asker: &AccountId,
        device: PublicKey,
        preferences: WorkPreferences,
    ) -> Result<WorkPreferences, JobError> {
        self.same_account(asker, &device)?;
        self.jobs.set_preferences(device, preferences.clone())?;
        self.market_log.append(&MarketEntry::Preferences {
            device,
            preferences: preferences.clone(),
        });
        Ok(preferences)
    }

    fn same_account(&self, asker: &AccountId, device: &PublicKey) -> Result<(), JobError> {
        match self.accounts.account_of(device) {
            Some(account) if account.id == *asker => Ok(()),
            _ => Err(JobError::NotLeased),
        }
    }

    /// Places a bid for `account`. The signed request names the bid.
    pub fn place_bid(
        &mut self,
        account: AccountId,
        order: BidOrder,
        request: &Envelope,
        now: u64,
    ) -> Result<Bid, JobError> {
        let id = ContentHash::of(request.signature.as_bytes());
        let bid = self.jobs.place_bid(account, order.clone(), id, now)?;
        self.market_log.append(&MarketEntry::Bid {
            account,
            id,
            order,
            at: now,
        });
        Ok(bid)
    }

    pub fn cancel_bid(&mut self, account: AccountId, id: ContentHash) -> Result<(), JobError> {
        self.jobs.cancel_bid(&account, &id)?;
        self.market_log.append(&MarketEntry::Cancel { account, id });
        Ok(())
    }

    /// Registers a name for the app `account` published, setting the deposit
    /// aside.
    pub fn register_name(
        &mut self,
        account: &Account,
        name: String,
        now: u64,
    ) -> Result<NameRecord, RegistryError> {
        if account.app.is_none() {
            return Err(RegistryError::NoApp);
        }
        self.registry.check(&name, &account.id)?;
        self.record(LedgerEntry::DataEscrow {
            id: name_deposit_id(&name),
            account: account.id,
            amount: NAME_DEPOSIT,
        })?;
        let entry = RegistryEntry::Register {
            name: name.clone(),
            app: account.id,
            at: now,
        };
        self.note_registry(entry);
        Ok(self
            .registry
            .resolve(&name)
            .cloned()
            .expect("just registered"))
    }

    pub fn renew_name(
        &mut self,
        account: &AccountId,
        name: String,
        now: u64,
    ) -> Result<NameRecord, RegistryError> {
        self.registry.owned(&name, account)?;
        self.note_registry(RegistryEntry::Renew {
            name: name.clone(),
            at: now,
        });
        Ok(self.registry.resolve(&name).cloned().expect("still held"))
    }

    pub fn release_name(&mut self, account: &AccountId, name: String) -> Result<(), RegistryError> {
        self.registry.owned(&name, account)?;
        self.release(name);
        Ok(())
    }

    fn release(&mut self, name: String) {
        // A name assigned by a reviewer was never paid a deposit for.
        let _ = self.record(LedgerEntry::DataRefund {
            id: name_deposit_id(&name),
        });
        self.note_registry(RegistryEntry::Release { name });
    }

    /// What a name points to.
    pub fn resolve_name(&self, name: &str) -> Result<Resolved, RegistryError> {
        let record = self.registry.resolve(name).ok_or(NameError::UnknownName)?;
        Ok(self.resolved(record))
    }

    fn resolved(&self, record: &NameRecord) -> Resolved {
        let manifest = self
            .accounts
            .get(&record.app)
            .and_then(|account| account.app.clone())
            .expect("a named app has a manifest");
        Resolved {
            record: record.clone(),
            manifest,
        }
    }

    pub fn directory(
        &self,
        verified_only: bool,
        query: Option<&str>,
        from: Option<&str>,
        limit: usize,
    ) -> Directory {
        let (records, next) = self.registry.page(verified_only, query, from, limit);
        Directory {
            entries: records.iter().map(|record| self.resolved(record)).collect(),
            next,
        }
    }

    pub fn set_verified(
        &mut self,
        reviewer: &AccountId,
        name: String,
        verified: bool,
    ) -> Result<NameRecord, RegistryError> {
        self.reviewer_only(reviewer)?;
        self.registry.resolve(&name).ok_or(NameError::UnknownName)?;
        self.note_registry(RegistryEntry::SetVerified {
            name: name.clone(),
            verified,
        });
        Ok(self.registry.resolve(&name).cloned().expect("still held"))
    }

    pub fn assign_name(
        &mut self,
        reviewer: &AccountId,
        name: String,
        app: AccountId,
        now: u64,
    ) -> Result<NameRecord, RegistryError> {
        self.reviewer_only(reviewer)?;
        decomp_core::registry::check_syntax(&name)?;
        if self.registry.resolve(&name).is_some() {
            return Err(NameError::Taken.into());
        }
        if self
            .accounts
            .get(&app)
            .is_none_or(|account| account.app.is_none())
        {
            return Err(RegistryError::NoApp);
        }
        self.note_registry(RegistryEntry::Register {
            name: name.clone(),
            app,
            at: now,
        });
        Ok(self
            .registry
            .resolve(&name)
            .cloned()
            .expect("just assigned"))
    }

    fn reviewer_only(&self, account: &AccountId) -> Result<(), RegistryError> {
        if self.reviewers.contains(account) {
            Ok(())
        } else {
            Err(RegistryError::NotReviewer)
        }
    }

    fn note_registry(&mut self, entry: RegistryEntry) {
        self.registry.apply(&entry);
        self.registry_log.append(&entry);
    }

    /// Writes a record of `owner`'s, if `version` is still current.
    pub fn write_record(
        &mut self,
        owner: AccountId,
        key: String,
        version: u64,
        value: ContentHash,
        writer: PublicKey,
        now: u64,
    ) -> Result<Record, RecordError> {
        let record = self
            .records
            .write(owner, key, version, value, writer, now)?;
        self.records_log.append(&record);
        self.records.apply(record.clone());
        Ok(record)
    }

    pub fn get_record(&self, owner: &AccountId, key: &str) -> Result<Record, RecordError> {
        self.records
            .get(owner, key)
            .cloned()
            .ok_or(RecordError::UnknownRecord)
    }

    pub fn list_records(&self, owner: &AccountId, prefix: &str, from: Option<&str>) -> Vec<Record> {
        self.records.list(owner, prefix, from)
    }

    /// Commits a node's result: checks the blobs it names exist, then records
    /// the commitment.
    pub fn commit_result(
        &mut self,
        node: PublicKey,
        job_id: JobId,
        commitment: ContentHash,
        now: u64,
    ) -> Result<(), JobError> {
        self.jobs
            .commit_result(node, job_id, commitment, now, &mut rand::rng())
    }

    /// Takes a node's revealed result.
    pub fn reveal_result(
        &mut self,
        node: PublicKey,
        result: JobResult,
        salt: ContentHash,
        envelope: Envelope,
        now: u64,
    ) -> Result<(), JobError> {
        self.jobs.reveal_result(node, result, salt, envelope, now)
    }

    /// Accepts a job: checks it, sets its highest possible fee aside, and queues
    /// it. The fee comes from the submitter's account, or from the app the
    /// manifest names as sponsor if that app agrees to pay. A job with a split rule is cut into pieces here, each queued as a job
    /// of its own on its own part of the input.
    ///
    /// `request` is the signed request that submits the job. The job's ID is
    /// made from it, signature and all, which is what makes it a one-time ID
    /// (see `JobId`).
    pub fn submit_job(
        &mut self,
        request: &Envelope,
        account: AccountId,
        manifest: Manifest,
        now: u64,
        sandbox: &Sandbox,
    ) -> Result<JobId, SubmitError> {
        if manifest.reads.len() > MAX_READS {
            return Err(SubmitError::TooManyReads);
        }
        if manifest.keep_from.len() > MAX_KEPT_FROM || manifest.apart_from.len() > MAX_APART_FROM {
            return Err(SubmitError::KeptFromTooMany);
        }
        for hash in [manifest.module, manifest.input]
            .iter()
            .chain(&manifest.reads)
        {
            if !self.blobs.contains(hash) {
                return Err(SubmitError::UnknownBlob(*hash));
            }
        }
        if let Some(shard) = &manifest.state {
            let app = self
                .accounts
                .get(&shard.app)
                .and_then(|owner| owner.app.as_ref());
            if !app.is_some_and(|app| app.modules.contains(&manifest.module)) {
                return Err(SubmitError::StateNotAllowed);
            }
            if !shard.is_well_named() {
                return Err(SubmitError::BadShardName);
            }
            if manifest.split.is_some() {
                return Err(SubmitError::SplitWithState);
            }
        }
        self.check_against_module(&manifest, sandbox)?;
        let job_id = JobId::of(&request.signer, &request.payload, &request.signature);

        // The jobs that will actually run: this one, or its pieces.
        let mut runs = vec![(job_id, manifest.clone())];
        if let Some(split) = manifest.split {
            let input = self.blobs.get(&manifest.input)?.unwrap_or_default();
            if split.pieces(input.len() as u64) > MAX_PIECES {
                return Err(SubmitError::TooManyPieces);
            }
            let mut chunks: Vec<&[u8]> = input.chunks(split.chunk_bytes.max(1) as usize).collect();
            if chunks.is_empty() {
                chunks.push(&[]);
            }
            runs.clear();
            for (index, chunk) in chunks.into_iter().enumerate() {
                let piece = Manifest {
                    input: self.blobs.put(chunk)?,
                    split: None,
                    max_fee: self.jobs.highest_fee(&manifest),
                    ..manifest.clone()
                };
                runs.push((JobId::piece(&job_id, index as u64), piece));
            }
        }

        let total = |amount: &dyn Fn(&Manifest) -> Credits| {
            runs.iter()
                .fold(Credits::ZERO, |sum, (_, run)| sum + amount(run))
        };
        let highest = total(&|run| self.jobs.highest_fee(run));
        if manifest.max_fee < highest {
            return Err(SubmitError::FeeCapTooLow(highest));
        }
        for (id, run) in &runs {
            self.jobs.check(id, run, now)?;
        }
        let to_set_aside = total(&|run| run.max_fee);
        let payer = manifest.sponsor.unwrap_or(account);
        if let Some(app) = manifest.sponsor {
            let published = self.accounts.get(&app).and_then(|owner| owner.app.as_ref());
            let module = &manifest.module;
            self.sponsored
                .check((app, published), account, module, to_set_aside, now)?;
        }
        if self.ledger.balance(&payer).spendable() < to_set_aside {
            return Err(LedgerError::InsufficientCredits.into());
        }

        for (id, run) in &runs {
            self.record(LedgerEntry::Escrow {
                job_id: *id,
                account: payer,
                amount: run.max_fee,
            })?;
            self.escrow_payer.insert(*id, payer);
            let kept_from = self.kept_from(run);
            self.jobs
                .submit(*id, account, run.clone(), kept_from, now)?;
        }
        if let Some(app) = manifest.sponsor {
            self.sponsored.count(app, account, to_set_aside, now);
        }
        if manifest.split.is_some() {
            let pieces = runs.iter().map(|(id, _)| *id).collect();
            self.splits.add(job_id, account, manifest, pieces);
        }
        Ok(job_id)
    }

    /// The accounts a job is kept from: those that the keys its manifest names
    /// belong to. A key that belongs to no account bars nobody.
    fn kept_from(&self, manifest: &Manifest) -> HashSet<AccountId> {
        let keys = manifest.keep_from.iter();
        keys.filter_map(|key| self.accounts.account_of(key))
            .map(|account| account.id)
            .collect()
    }

    /// Refuses a job that is not what its module says of itself: one in another
    /// privacy mode than the module declares, or a private one that no route
    /// lets nodes run (see `privacy`), or one whose input does not fit the types
    /// the module says its job takes (spec section 7, "Typing"), in the form
    /// its route hands them to nodes. A module that does not describe itself is
    /// open and takes any input.
    fn check_against_module(
        &mut self,
        manifest: &Manifest,
        sandbox: &Sandbox,
    ) -> Result<(), SubmitError> {
        if !self.signatures.contains_key(&manifest.module) {
            let module = self.blobs.get(&manifest.module)?.unwrap_or_default();
            self.signatures
                .insert(manifest.module, sandbox.declared(&module));
        }
        let signature = self.signatures[&manifest.module].as_ref();
        privacy::check(manifest, signature)?;
        let Some(signature) = signature else {
            return Ok(());
        };
        // Cutting typed input at byte boundaries would hand each piece broken data.
        if manifest.split.is_some() {
            return Err(SubmitError::TypedJobSplitByBytes);
        }
        let input = self.blobs.get(&manifest.input)?.unwrap_or_default();
        privacy::check_input(manifest, signature, &input).map_err(SubmitError::TypeMismatch)
    }

    /// Where a job stands, for the account that submitted it.
    pub fn job_status(
        &mut self,
        job_id: JobId,
        account: AccountId,
        now: u64,
    ) -> Result<JobStatus, JobError> {
        match self.splits.status(&job_id, &account, now) {
            Some(status) => Ok(status),
            None => self.jobs.status(job_id, account, now),
        }
    }

    /// Joins the outputs of a split job whose pieces have all completed.
    fn merge(&mut self, merge: Merge, now: u64) {
        let mut output = Vec::new();
        for hash in &merge.outputs {
            let piece = self.blobs.get(hash).expect("blobs can be read back");
            output.extend(piece.expect("a verified output was stored"));
        }
        let hash = self.blobs.put(&output).expect("blobs can be stored");
        self.splits.merged(merge.job_id, hash, now);
    }

    /// Accepts a blob for storage: chooses the nodes to hold it and sets the
    /// whole fee aside. The caller then uploads the blob to those nodes.
    pub fn store_blob(&mut self, blob: Storing, now: u64) -> Result<Placement, StoreError> {
        if let Some(addressed) = &blob.addressed
            && !self.is_device(&addressed.to)
        {
            return Err(StoreError::UnknownDevice);
        }
        let online = self.hosts.online(now);
        let (placed, escrow) = self.storage.place(blob, online, now)?;
        self.record(escrow)?;
        let StorageEntry::Placed { replicas, .. } = &placed else {
            unreachable!("placing a blob yields a Placed entry");
        };
        let addresses = replicas
            .iter()
            .map(|replica| replica.address.clone())
            .collect();
        self.note_storage(placed, None);
        Ok(Placement { addresses })
    }

    /// A node says it has received a blob it was asked to hold.
    pub fn confirm_stored(
        &mut self,
        root: ContentHash,
        node: PublicKey,
        now: u64,
    ) -> Result<(), StorageError> {
        let confirmed = self.storage.confirm(root, node, now)?;
        self.note_storage(confirmed, None);
        Ok(())
    }

    /// Checks a node's answer to a storage challenge and pays it if it passed.
    pub fn answer_challenge(
        &mut self,
        node: PublicKey,
        challenge: Challenge,
        chunk: &[u8],
        proof: &[ContentHash],
        now: u64,
    ) -> Result<(), StorageError> {
        let (outcome, payment) = self.storage.answer(node, challenge, chunk, proof, now)?;
        self.note_storage(outcome, payment);
        Ok(())
    }

    /// Deletes a blob `owner` stored, returning what is left of its fee.
    pub fn delete_blob(&mut self, root: &ContentHash, owner: &AccountId) -> Result<(), StoreError> {
        let (ended, refund) = self.storage.delete(root, owner)?;
        self.note_storage(ended, Some(refund));
        Ok(())
    }

    /// Where a stored blob can be fetched from.
    pub fn locate(&self, root: &ContentHash) -> Option<Located> {
        self.storage.locate(root)
    }

    /// Keeps a device's signed request to publish its sealing key.
    pub fn publish_sealing_key(&mut self, proof: &Envelope) -> Result<(), EnvelopeError> {
        if self.sealing_keys.apply(proof)? {
            self.sealing_log.append(proof);
        }
        Ok(())
    }

    /// The request in which a device published its sealing key, if it has.
    pub fn sealing_key_proof(&self, device: &PublicKey) -> Option<Envelope> {
        self.sealing_keys.proof(device).cloned()
    }

    /// The hash of the contents a shard of state has now, or `None` if it is empty.
    pub fn shard_now(&self, shard: &ShardRef) -> Option<ContentHash> {
        self.states.contents(shard)
    }

    /// Opens a channel from the device `from` to the device `to`: chooses the
    /// node that relays it and sets the fee for `bytes` bytes aside. `payload` is
    /// the signed request, which gives the channel its name.
    pub fn open_relay(
        &mut self,
        from: PublicKey,
        payer: AccountId,
        payload: &str,
        to: PublicKey,
        bytes: u64,
        now: u64,
    ) -> Result<Route, OpenRelayError> {
        if !self.is_device(&to) {
            return Err(OpenRelayError::UnknownDevice);
        }
        let channel = channel_id(&from, payload);
        let opening = Opening {
            channel,
            from,
            payer,
            to,
            bytes,
        };
        let online = self.hosts.online(now);
        let (opened, escrow) = self.relays.open(opening, &online, now, &mut rand::rng())?;
        self.record(escrow)?;
        let RelayEntry::Opened { relay, .. } = &opened else {
            unreachable!("opening a channel yields an Opened entry");
        };
        let address = relay.address.clone();
        self.note_relay(opened, None);
        Ok(Route { channel, address })
    }

    /// Whether `key` is a device some account has approved and not revoked.
    fn is_device(&self, key: &PublicKey) -> bool {
        let account = self.accounts.account_of(key);
        account.is_some_and(|account| account.authorizes(key))
    }

    /// What is addressed to a device: open channels and stored blobs left for
    /// it. Asking shows that the device is waiting to receive.
    pub fn incoming(&mut self, to: &PublicKey, now: u64) -> IncomingChannels {
        self.relays.listening(*to, now);
        IncomingChannels {
            channels: self.relays.incoming(to, now),
            stored: self.storage.waiting_for(to),
        }
    }

    /// Pays a relay node for the bytes a channel's receiver has signed for.
    pub fn redeem_relay(
        &mut self,
        node: &PublicKey,
        header: &Envelope,
        receipt: &Envelope,
    ) -> Result<(), RelayError> {
        for (entry, credits) in self.relays.redeem(node, header, receipt)? {
            self.note_relay(entry, Some(credits));
        }
        Ok(())
    }

    /// Applies and records a change to the channels, and moves any credits that
    /// go with it.
    fn note_relay(&mut self, entry: RelayEntry, credits: Option<LedgerEntry>) {
        self.relays.apply(&entry);
        self.relays_log.append(&entry);
        if let Some(credits) = credits {
            // A relay is never paid for more bytes than the channel may carry, and
            // the fee for that many was set aside when it opened.
            self.record(credits)
                .expect("relay payments fit what was set aside");
        }
    }

    /// Applies and records a change to what is stored where, moves any credits
    /// that go with it, and scores the node if it was challenged.
    fn note_storage(&mut self, entry: StorageEntry, payment: Option<LedgerEntry>) {
        self.storage.apply(&entry);
        self.storage_log.append(&entry);
        if let Some(payment) = payment {
            // A payment never exceeds what was set aside: both are the same rate
            // over time that lies inside the paid period.
            self.record(payment)
                .expect("storage payments fit what was set aside");
        }
        let scored = match entry {
            StorageEntry::Passed { node, at, .. } => Some((node, at, Conduct::Finished)),
            StorageEntry::Failed { node, at, .. } => Some((node, at, Conduct::Missed)),
            _ => None,
        };
        if let Some((node, at, what)) = scored {
            let event = ScoreEvent { node, at, what };
            self.scores.apply(&event);
            self.scores_log.append(&event);
        }
    }

    /// The cash-outs that wait to be paid or refused.
    pub fn cash_outs(&self) -> Vec<CashOut> {
        self.ledger.cash_outs()
    }

    /// Moves credits and records the move.
    pub fn record(&mut self, entry: LedgerEntry) -> Result<(), LedgerError> {
        self.ledger.apply(&entry)?;
        self.ledger_log.append(&entry);
        Ok(())
    }

    /// Acts on what the job rules decided: updates node scores, pays for jobs
    /// whose result stood, and returns the credits set aside for jobs that have
    /// none. Call after anything that can change a job.
    pub fn settle(&mut self, now: u64) {
        self.jobs.tick(now);
        for name in self.registry.lapsed(now) {
            self.release(name.name);
        }
        for (entry, refund) in self.storage.sweep(now) {
            self.note_storage(entry, refund);
        }
        for failed in self.storage.overdue(now, self.challenge_interval) {
            self.note_storage(failed, None);
        }
        for repair in self.storage.repairs(&self.hosts.online(now)) {
            self.note_storage(repair, None);
        }
        for (entry, refund) in self.relays.sweep(now) {
            self.note_relay(entry, Some(refund));
        }
        for event in self.jobs.take_events() {
            let penalty = self.scores.apply(&event);
            self.scores_log.append(&event);
            if penalty == Penalty::ReturnedToProbation {
                let lost = |staked: Credits| staked.percent(PROBATION_FORFEIT_PERCENT);
                self.forfeit(&event.node, lost);
            }
        }
        // A job's draws are published once the job has closed (spec section 4:
        // "Each draw is published").
        for draw in self.jobs.take_draws() {
            let signed = Envelope::seal(&self.control, &draw);
            self.draws_log.append(&signed);
        }
        for closed in self.jobs.take_closed() {
            let job_id = closed.job_id;
            let mut asked = None;
            if let Some(merge) = self.splits.piece_closed(&closed, now) {
                self.merge(merge, now);
            }
            // Every accepted job has its highest possible fee set aside, results
            // above the compute cap are refused, and the other entries here cannot
            // fail for an account that exists.
            let entries = match closed.how {
                Closure::Settled {
                    receipt,
                    split,
                    payouts,
                    follow_ups,
                } => {
                    asked = follow_ups;
                    // What the job left its shard with is what the next job on
                    // that shard is handed.
                    if let (
                        Some(shard),
                        Outcome::Completed {
                            state: Some(contents),
                            ..
                        },
                    ) = (&receipt.state, &receipt.outcome)
                    {
                        let entry = StateEntry {
                            shard: shard.clone(),
                            contents: *contents,
                        };
                        self.states.apply(&entry);
                        self.states_log.append(&entry);
                    }
                    self.keep_receipt(&receipt);
                    if let Some(payer) = self.payer_of(&job_id) {
                        self.note_activity(ActivityEntry::Spent {
                            account: payer,
                            amount: split.total,
                            at: now,
                        });
                    }
                    for Payout {
                        account, amount, ..
                    } in &payouts
                    {
                        self.note_activity(ActivityEntry::Earned {
                            account: *account,
                            amount: *amount,
                            at: now,
                        });
                    }
                    vec![Some(LedgerEntry::Settle {
                        job_id,
                        split,
                        payouts,
                    })]
                }
                Closure::Rejected | Closure::Expired => {
                    vec![Some(LedgerEntry::Refund { job_id })]
                }
                Closure::FallBack(job) => {
                    self.fall_backs.push((job_id, *job));
                    vec![]
                }
                Closure::CheckPassed { account, reward } => {
                    self.note_activity(ActivityEntry::Earned {
                        account,
                        amount: reward,
                        at: now,
                    });
                    vec![Some(LedgerEntry::PayChecker {
                        account,
                        amount: reward,
                    })]
                }
                Closure::Release => vec![Some(LedgerEntry::Release { job_id })],
                Closure::AuditFailed { account } => {
                    vec![Some(LedgerEntry::ForfeitHeld { job_id, account })]
                }
            };
            for entry in entries.into_iter().flatten() {
                self.record(entry).expect("a closed job can be settled");
            }
            self.escrow_payer.remove(&job_id);
            // The job has been paid for and the rest of its fee cap returned,
            // which is what its follow-ups are paid from.
            if let Some(follow_ups) = asked {
                self.start_follow_ups(follow_ups, now);
            }
        }
    }

    /// Starts the follow-up jobs a verified job asked for, in the order it asked,
    /// for as long as what it left of its fee cap covers the most each could
    /// cost. The rest are not started. A follow-up's input is not checked against
    /// its module's types: a wrong one fails when it runs.
    fn start_follow_ups(&mut self, follow_ups: FollowUps, now: u64) {
        let FollowUps {
            submitter,
            mut spare,
            jobs,
        } = follow_ups;
        for (id, manifest) in jobs {
            let amount = manifest.max_fee;
            let account = manifest.sponsor.unwrap_or(submitter);
            if amount > spare || self.jobs.check(&id, &manifest, now).is_err() {
                break;
            }
            let escrow = LedgerEntry::Escrow {
                job_id: id,
                account,
                amount,
            };
            if self.record(escrow).is_err() {
                break;
            }
            self.escrow_payer.insert(id, account);
            spare = spare - amount;
            let kept_from = self.kept_from(&manifest);
            self.jobs
                .submit(id, submitter, manifest, kept_from, now)
                .expect("the job was checked just above");
        }
    }

    /// The disputes that have arisen, each with the blobs needed to decide it.
    pub fn take_cases(&mut self) -> Vec<Case> {
        let disputes = self.jobs.take_disputes();
        let blob = |hash: &ContentHash| -> Vec<u8> {
            self.blobs.get(hash).ok().flatten().unwrap_or_default()
        };
        disputes
            .into_iter()
            .map(|dispute| Case {
                job_id: dispute.job_id,
                module: blob(&dispute.manifest.module),
                input: blob(&dispute.manifest.input),
                blobs: dispute
                    .manifest
                    .reads
                    .iter()
                    .map(|hash| (*hash, blob(hash)))
                    .collect(),
                shard: dispute
                    .manifest
                    .state
                    .as_ref()
                    .map(|_| match dispute.shard {
                        Some(contents) => blob(&contents),
                        None => records::encode(&Records::new()),
                    }),
                compute_cap: dispute.manifest.compute_cap,
                checkpoint_lists: dispute
                    .reports
                    .iter()
                    .map(|r| blob(&r.1.checkpoints))
                    .collect(),
                reports: dispute.reports,
            })
            .collect()
    }

    /// Closes a dispute. Nodes that were wrong pay what refereeing cost, out of
    /// their accounts' stake.
    pub fn resolve(&mut self, case: &Case, verdict: &Verdict, now: u64) {
        self.jobs.resolve(case.job_id, &verdict.right, now);
        let cost = compute_fee(verdict.units, decomp_core::market::BASE_MICROS_PER_UNIT);
        for node in &verdict.wrong {
            self.forfeit(node, |_| cost);
        }
        self.settle(now);
    }

    /// The jobs to run as an always-on fallback, each with what it needs.
    pub fn take_fall_backs(&mut self) -> Vec<(JobId, FallBackJob, Case)> {
        let fall_backs = std::mem::take(&mut self.fall_backs);
        let blob = |hash: &ContentHash| -> Vec<u8> {
            self.blobs.get(hash).ok().flatten().unwrap_or_default()
        };
        fall_backs
            .into_iter()
            .map(|(job_id, job)| {
                let case = Case {
                    job_id: job.seed,
                    module: blob(&job.manifest.module),
                    input: blob(&job.manifest.input),
                    blobs: job
                        .manifest
                        .reads
                        .iter()
                        .map(|hash| (*hash, blob(hash)))
                        .collect(),
                    shard: job.manifest.state.as_ref().map(|_| match job.shard {
                        Some(contents) => blob(&contents),
                        None => records::encode(&Records::new()),
                    }),
                    compute_cap: job.manifest.compute_cap,
                    checkpoint_lists: Vec::new(),
                    reports: Vec::new(),
                };
                (job_id, job, case)
            })
            .collect()
    }

    /// Settles a job that ran as an always-on fallback. The blobs it produced
    /// are stored first.
    pub fn fell_back(
        &mut self,
        job_id: JobId,
        result: JobResult,
        produced: Vec<Vec<u8>>,
        now: u64,
    ) {
        for blob in produced {
            let _ = self.blobs.put(&blob);
        }
        self.jobs.fall_back(job_id, result, now);
        self.settle(now);
    }

    /// The account that pays for a job: the sponsor or the submitter.
    fn payer_of(&self, job_id: &JobId) -> Option<AccountId> {
        self.escrow_payer.get(job_id).copied()
    }

    /// Takes stake from the account a node belongs to.
    fn forfeit(&mut self, node: &PublicKey, amount: impl Fn(Credits) -> Credits) {
        let Some(account) = self.accounts.account_of(node).map(|account| account.id) else {
            return;
        };
        let amount = amount(self.ledger.balance(&account).staked);
        self.record(LedgerEntry::Forfeit { account, amount })
            .expect("a forfeit takes only what is staked");
    }
}

/// Reads the lines of a log as entries.
fn read<T: DeserializeOwned>(lines: Vec<String>) -> io::Result<Vec<T>> {
    let entries = lines.iter().map(|line| serde_json::from_str(line));
    Ok(entries.collect::<Result<_, _>>()?)
}

/// What a line of `receipts.jsonl` is read for at startup: the job it is the
/// receipt of. The rest of the line is kept as text.
#[derive(Deserialize)]
struct Named {
    job_id: JobId,
}

/// Opens a log and applies every entry already in it.
fn replay<T: DeserializeOwned>(
    path: &Path,
    mut apply: impl FnMut(T) -> Result<(), String>,
) -> io::Result<Log> {
    let log = Log::open(path)?;
    for (index, line) in log.lines()?.enumerate() {
        let applied = serde_json::from_str(&line?)
            .map_err(|e| e.to_string())
            .and_then(&mut apply);
        applied.map_err(|reason| {
            let message = format!("{}, line {}: {reason}", path.display(), index + 1);
            io::Error::new(io::ErrorKind::InvalidData, message)
        })?;
    }
    Ok(log)
}

/// What a name's deposit is set aside under in the ledger.
fn name_deposit_id(name: &str) -> ContentHash {
    ContentHash::of(format!("decomp/name-deposit/v1\n{name}").as_bytes())
}
