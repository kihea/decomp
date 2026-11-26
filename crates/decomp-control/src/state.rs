//! Everything the control plane knows, and the log each part is kept in. A
//! host supplies the logs (see `store`): files on the devnet, and heap memory
//! saved to stable memory in a canister (spec, "Planes and components").
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

use std::collections::{HashMap, HashSet};
use std::io;

use decomp_core::LogEntry;
use decomp_core::Records as AccountRecords;
use decomp_core::devices::{DEVICE_ROOT_REFRESH_SECS, DeviceProof, DeviceTree};
use decomp_core::market::{BALANCED_LOOKBACK_SECS, fallback_rate};
use decomp_core::{
    Account, AccountId, AccountStanding, AccountStatus, Accounts, Activity, Beacon, Bid, BidOrder,
    CashOut, Challenge, Closure, Conduct, ContentHash, Credits, Directory, DrawPage, Envelope,
    EnvelopeError, FallBackJob, Fallback, FollowUps, Hosts, Inclusion, IncomingChannels, JobError,
    JobId, JobResult, JobStatus, Jobs, LeasesGranted, Ledger, LedgerEntry, LedgerError, Located,
    LogError, LogPage, MAX_APART_FROM, MAX_KEPT_FROM, MAX_LOG_PAGE, MAX_PIECES, MAX_READS,
    MAX_SHARD_NAME_BYTES, Manifest, NAME_DEPOSIT, NameError, NameRecord, OpError, Opening, Outcome,
    PROBATION_FORFEIT_PERCENT, PRODUCED_RETENTION_DAYS, Payout, Penalty, Placement, Posted,
    PrivacyError, PublicKey, Quote, Receipt, Record, RecordError, Registry, RegistryEntry,
    RelayEntry, RelayError, Relays, Resolved, Route, ScoreEvent, Scores, SealingKeys,
    SettlementLog, ShardRef, Signature, Splits, SponsorError, Sponsored, Standing, Standings,
    StateEntry, States, Storage, StorageEntry, StorageError, Storing, Through, WorkPreferences,
    channel_id, draw_hash, merkle, privacy, settlement,
};
use decomp_core::{
    DepositAddress, Hardware, Holdings, Host, IcrcAccount, PROBE_BYTES, PROBE_INTERVAL_SECS,
    PROBE_TIMEOUT_SECS, Probe, Subaccount, TOKEN_SUPPLY, Tokens, Withdrawal, deposit_subaccount,
    probe,
};
use decomp_schema::records::{self, Records};
use rand::rngs::StdRng;
use rand::{Rng, RngExt, SeedableRng};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::fallback::FallBackRun;
use crate::store::{Blobs, ControlKey, Log, StateStore};

/// Why a blob was not accepted for storage.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error("the receiving key is not a device in any account")]
    UnknownDevice,
    #[error("this node holds no lease on the job")]
    NotLeased,
    #[error("nobody pays for this job's blobs")]
    NoPayer,
}

/// Why a measurement was not taken in.
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    #[error("this node was not asked to make that measurement, or was asked too long ago")]
    NotAsked,
    #[error("the hash is not that of the test data")]
    WrongData,
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
    #[error("the control plane is not taking new jobs now; ask again later")]
    NotTakingJobs,
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

/// The ICRC ledger that holds copies of the token, and what the control
/// plane needs to mint and burn them there.
#[derive(Clone)]
pub struct TokenBridge {
    /// The ICRC ledger's principal, in text form.
    pub ledger: String,
    /// The control plane's own principal, the ICRC ledger's only minter.
    pub minter: String,
    /// Makes each account's deposit address, so that the public ICRC ledger
    /// does not show whose a deposit is (see `deposit_subaccount`).
    pub secret: [u8; 32],
}

#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    #[error("this control plane has no ICRC ledger for the token")]
    NoBridge,
    #[error(transparent)]
    Ledger(#[from] LedgerError),
}

/// The settings a local network is started with.
pub struct Settings {
    /// How long a stored blob goes between challenges, in seconds.
    pub challenge_interval: u64,
    /// How long a recovery waits before it takes effect, in seconds.
    pub recovery_wait: u64,
    /// The accounts allowed to review apps and assign held names.
    pub reviewers: Vec<AccountId>,
    /// The accounts allowed to act for the treasury: record credits bought,
    /// pay or refuse cash-outs, and give out tokens.
    pub treasurers: Vec<AccountId>,
    /// Brand names held until a reviewer assigns them.
    pub reserved_names: Vec<String>,
    /// Randomness to seed the control plane's own draws with: the choice of
    /// known-answer jobs, storage challenges, and relays. The devnet passes
    /// the operating system's; a canister passes the network's, and reseeds
    /// as it goes (`reseed`).
    pub entropy: [u8; 32],
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
    receipts: Log,
    /// The hash of each receipt's text, in the order the receipts were made,
    /// and each job's place in that order. Batches are made from these.
    receipt_hashes: Vec<ContentHash>,
    receipt_of: HashMap<JobId, u64>,
    /// The key the settlement log is signed with.
    control: ControlKey,
    /// An entry of the settlement log waiting for a threshold key's signature.
    unsigned: Option<(String, Vec<u8>)>,
    settlement: SettlementLog,
    settlement_log: Log,
    /// The settlement log's last entry posting a devices root, and the tree
    /// it is the root of, once it has been built again since.
    devices_entry: Option<Envelope>,
    device_tree: Option<DeviceTree>,
    /// The draws of jobs that have closed, each as the text it was published
    /// as, and the hash of each text. Each batch fingerprints the draws since
    /// the batch before it.
    draws_log: Log,
    draw_hashes: Vec<ContentHash>,
    registry: Registry,
    registry_log: Log,
    records: AccountRecords,
    records_log: Log,
    reviewers: Vec<AccountId>,
    treasurers: Vec<AccountId>,
    /// The ICRC ledger that holds copies of the token, once the host has one.
    bridge: Option<TokenBridge>,
    /// Accounts that asked for their deposit address to be checked.
    deposit_checks: Vec<AccountId>,
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
    /// The measurement each node has been asked to make and not reported,
    /// with when it was asked, and when each node was last asked.
    probes: HashMap<PublicKey, (Probe, u64)>,
    probed_at: HashMap<PublicKey, u64>,
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
    /// The control plane's own source of randomness (see `Settings::entropy`).
    rng: StdRng,
    /// Whether new jobs are accepted. A host stops taking them when it is
    /// about to run out of what it runs on (spec, "Failure handling": the
    /// canister "stops taking new jobs before it runs out" of cycles).
    taking_jobs: bool,
}

impl ControlPlane {
    pub fn open(store: &mut dyn StateStore, settings: Settings) -> io::Result<Self> {
        let Settings {
            challenge_interval,
            recovery_wait,
            reviewers,
            treasurers,
            reserved_names,
            entropy,
        } = settings;

        let mut accounts = Accounts::waiting(recovery_wait);
        let mut ledger = Ledger::default();
        let ops = replay(store, "ops", |envelope: Envelope| {
            let account = accounts.apply(&envelope).map_err(|e| e.to_string())?;
            ledger.open(account.id);
            Ok(())
        })?;
        let mut ledger_log = replay(store, "ledger", |entry: LedgerEntry| {
            ledger.apply(&entry).map_err(|e| e.to_string())
        })?;
        let mut scores = Scores::default();
        let scores_log = replay(store, "scores", |event: ScoreEvent| {
            scores.apply(&event);
            Ok(())
        })?;

        let mut storage = Storage::default();
        let storage_log = replay(store, "storage", |entry: StorageEntry| {
            storage.apply(&entry);
            Ok(())
        })?;

        let mut relays = Relays::default();
        let relays_log = replay(store, "relays", |entry: RelayEntry| {
            relays.apply(&entry);
            Ok(())
        })?;

        let mut states = States::default();
        let states_log = replay(store, "state", |entry: StateEntry| {
            states.apply(&entry);
            Ok(())
        })?;

        let mut sealing_keys = SealingKeys::default();
        let sealing_log = replay(store, "sealing", |proof: Envelope| {
            sealing_keys.apply(&proof).map_err(|e| e.to_string())?;
            Ok(())
        })?;

        let mut registry = Registry::default();
        for name in reserved_names {
            registry.apply(&RegistryEntry::Reserve { name });
        }
        let registry_log = replay(store, "registry", |entry: RegistryEntry| {
            registry.apply(&entry);
            Ok(())
        })?;

        let mut records = AccountRecords::default();
        let records_log = replay(store, "records", |record: Record| {
            records.apply(record);
            Ok(())
        })?;

        let mut jobs = Jobs::default();
        let market_log = replay(store, "market", |entry: MarketEntry| {
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
        let activity_log = replay(store, "activity", |entry: ActivityEntry| {
            if let ActivityEntry::Created { account, at } = entry {
                created.insert(account, at);
            } else {
                activity.push(entry);
            }
            Ok(())
        })?;

        let draws_log = store.log("draws")?;
        let draw_hashes = draws_log
            .lines()?
            .map(|line| line.map(|text| draw_hash(&text)))
            .collect::<io::Result<Vec<_>>>()?;
        let receipts = store.log("receipts")?;
        let mut receipt_hashes = Vec::new();
        let mut receipt_of = HashMap::new();
        for line in receipts.lines()? {
            let line = line?;
            let Named { job_id } = serde_json::from_str(&line)?;
            receipt_of.insert(job_id, receipt_hashes.len() as u64);
            receipt_hashes.push(merkle::item_hash(line.as_bytes()));
        }

        let mut settlement = SettlementLog::default();
        let mut devices_entry = None;
        let settlement_log = replay(store, "settlement", |entry: Envelope| {
            settlement.apply(&entry).map_err(|e| e.to_string())?;
            if posts_devices(&entry) {
                devices_entry = Some(entry);
            }
            Ok(())
        })?;
        let Through {
            ops: posted_ops,
            entries,
            receipts: posted_receipts,
            draws: posted_draws,
        } = settlement.through();
        if posted_ops > ops.len()
            || entries > ledger_log.len()
            || posted_receipts > receipts.len()
            || posted_draws > draws_log.len()
        {
            let message = "the settlement log reaches further than the records it was made from";
            return Err(io::Error::new(io::ErrorKind::InvalidData, message));
        }

        // The token's whole supply is created once, in the treasury.
        if !ledger.supply_created() {
            let mint = LedgerEntry::MintTokens {
                amount: TOKEN_SUPPLY,
            };
            ledger.apply(&mint).expect("no supply has been created");
            ledger_log.append(&mint);
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
            blobs: store.blobs()?,
            jobs,
            sponsored: Sponsored::default(),
            splits: Splits::default(),
            receipts,
            receipt_hashes,
            receipt_of,
            control: store.control_key()?,
            unsigned: None,
            taking_jobs: true,
            settlement,
            settlement_log,
            devices_entry,
            device_tree: None,
            draw_hashes,
            draws_log,
            registry,
            registry_log,
            records,
            records_log,
            reviewers,
            treasurers,
            bridge: None,
            deposit_checks: Vec::new(),
            market_log,
            activity_log,
            created,
            escrow_payer: HashMap::new(),
            fall_backs: Vec::new(),
            activity,
            hosts: Hosts::default(),
            probes: HashMap::new(),
            probed_at: HashMap::new(),
            storage,
            storage_log,
            relays,
            relays_log,
            states,
            states_log,
            sealing_keys,
            sealing_log,
            challenge_interval,
            rng: StdRng::from_seed(entropy),
        })
    }

    /// Mixes fresh randomness into the control plane's own draws.
    pub fn reseed(&mut self, entropy: [u8; 32]) {
        let mut seed = [0; 32];
        self.rng.fill_bytes(&mut seed);
        for (byte, fresh) in seed.iter_mut().zip(entropy) {
            *byte ^= fresh;
        }
        self.rng = StdRng::from_seed(seed);
    }

    /// The key that signs the settlement log, whose batches prove the draws.
    pub fn control_key(&self) -> PublicKey {
        self.control.public_key()
    }

    /// Posts a batch to the settlement log (spec section 6: "Every 10 minutes
    /// (starting value) the receipts canister combines the new receipts into
    /// one fingerprint and posts it with the net balance change per account").
    /// Nothing is posted when there is no new receipt and no balance changed.
    pub fn post_batch(&mut self, now: u64) {
        // An entry still waiting for its signature comes first.
        if self.unsigned.is_some() {
            return;
        }
        let through = Through {
            ops: self.ops.len(),
            entries: self.ledger_log.len(),
            receipts: self.receipts.len(),
            draws: self.draw_hashes.len() as u64,
        };
        let last = self.settlement.through();
        let receipts = &self.receipt_hashes[last.receipts as usize..];
        let draws = &self.draw_hashes[last.draws as usize..];
        let holdings = self.holdings();
        if let Some(batch) = self
            .settlement
            .next_batch(receipts, draws, &holdings, through)
        {
            self.post(now, Posted::Batch(batch));
        }
        self.post_devices(now);
    }

    /// Posts the root of the tree of devices and their accounts, if the
    /// devices changed since it was posted last or it is due again (see
    /// `decomp_core::devices`). One entry waits for a threshold key's
    /// signature at a time, so behind a batch it waits for the next period.
    fn post_devices(&mut self, now: u64) {
        if self.unsigned.is_some() {
            return;
        }
        let tree = DeviceTree::new(self.accounts.devices());
        let due = match self.settlement.devices() {
            Some((root, at)) => {
                root != tree.root() || now.saturating_sub(at) >= DEVICE_ROOT_REFRESH_SECS
            }
            None => true,
        };
        if due {
            let root = tree.root();
            self.device_tree = Some(tree);
            self.post(now, Posted::Devices { root });
        }
    }

    /// The proof that `device` belongs to its account, resting on the
    /// devices root posted last. `None` if no root has been posted with the
    /// device in it yet.
    pub fn device_proof(&mut self, device: &PublicKey) -> Option<DeviceProof> {
        let entry = self.devices_entry.clone()?;
        let (root, _) = self.settlement.devices()?;
        if self.device_tree.as_ref().map(DeviceTree::root) != Some(root) {
            // Since a restart, or since devices changed: the tree is built
            // again, and serves proofs only if the devices are as posted.
            let tree = DeviceTree::new(self.accounts.devices());
            if tree.root() != root {
                return None;
            }
            self.device_tree = Some(tree);
        }
        let (account, siblings) = self.device_tree.as_ref()?.prove(device)?;
        Some(DeviceProof {
            account,
            entry,
            siblings,
        })
    }

    /// Signs an entry and adds it to the settlement log, or, for a threshold
    /// key, keeps it until the host has had it signed.
    fn post(&mut self, now: u64, posted: Posted) {
        let entry = self.settlement.entry(now, posted);
        match &self.control {
            ControlKey::Held(key) => {
                let entry = Envelope::seal(key, &entry);
                self.append_entry(&entry)
                    .expect("the control plane's own entry follows the log's rules");
            }
            ControlKey::Threshold(_) => self.unsigned = Some(Envelope::to_sign(&entry)),
        }
    }

    /// Starts or stops taking new jobs. Jobs already taken run to the end.
    pub fn take_jobs(&mut self, taking: bool) {
        self.taking_jobs = taking;
    }

    /// The bytes the threshold key is to sign for the entry waiting to be
    /// posted, if one is.
    pub fn entry_to_sign(&self) -> Option<Vec<u8>> {
        self.unsigned.as_ref().map(|(_, bytes)| bytes.clone())
    }

    /// Posts the waiting entry with the threshold key's signature of it.
    pub fn signed(&mut self, signature: Signature) -> Result<(), LogError> {
        let Some((payload, bytes)) = self.unsigned.take() else {
            return Ok(());
        };
        let entry = Envelope::with_signature(payload.clone(), self.control.public_key(), signature);
        let appended = self.append_entry(&entry);
        if appended.is_err() {
            // A signature that does not hold leaves the entry waiting.
            self.unsigned = Some((payload, bytes));
        }
        appended
    }

    fn append_entry(&mut self, entry: &Envelope) -> Result<(), LogError> {
        self.settlement.apply(entry)?;
        self.settlement_log.append(entry);
        if posts_devices(entry) {
            self.devices_entry = Some(entry.clone());
        }
        Ok(())
    }

    /// Takes in the newest round of public randomness. Nodes are drawn for
    /// jobs from it, so until there is one, jobs wait.
    pub fn set_beacon(&mut self, beacon: Beacon) {
        self.jobs.set_beacon(beacon);
    }

    /// Up to `MAX_LOG_PAGE` of the draws published so far, from number `from` on.
    pub fn draws_page(&self, from: u64) -> io::Result<DrawPage> {
        Ok(DrawPage {
            draws: self.draws_log.range(from, MAX_LOG_PAGE)?,
        })
    }

    /// What sits at every ledger address now.
    fn holdings(&self) -> Holdings {
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
            tokens: self.ledger.tokens(&account.id),
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
        hardware: Option<Hardware>,
        now: u64,
    ) -> LeasesGranted {
        let serves_data = address.is_some();
        self.hosts.seen(node, account, address, now);
        if let Some(mut hardware) = hardware {
            // Only a node other nodes can reach runs stages for them.
            if !serves_data {
                hardware.stages.clear();
            }
            self.jobs.declare_hardware(node, hardware);
        }
        let probe = self.probe_for(node, account, serves_data, now);
        let interval = self.challenge_interval;
        let mut jobs = std::mem::take(&mut self.jobs);
        // The closures below borrow the rest of the control plane, so the
        // jobs and the generator are taken out while they run.
        let mut rng = std::mem::replace(&mut self.rng, StdRng::from_seed([0; 32]));
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
            let storage = &self.storage;
            let stored = |root: &ContentHash| storage.is_stored(root);
            jobs.request_lease(node, account, now, &mut rng, &standings, &contents, &stored)
        };
        self.jobs = jobs;
        self.rng = rng;
        let mut leases = granted.leases;
        for stage in leases
            .iter_mut()
            .flat_map(|lease| lease.pipeline.iter_mut())
        {
            stage.address = self.hosts.address(&stage.node);
        }
        LeasesGranted {
            leases,
            stages: granted.stages,
            reveal: granted.reveal,
            store: self.storage.orders_for(&node),
            drop: self.storage.take_drops(&node),
            challenge: self
                .storage
                .challenge_for(node, now, interval, &mut self.rng),
            relay: self.relays.orders_for(&node, now),
            probe,
        }
    }

    /// A measurement of another node's bandwidth for `node` to make, if it is
    /// due one: once every `PROBE_INTERVAL_SECS`, of a node of another account
    /// that serves data, drawn at random. Only nodes that serve data measure,
    /// since only they are known to be reachable and so to be measuring well.
    fn probe_for(
        &mut self,
        node: PublicKey,
        account: AccountId,
        serves_data: bool,
        now: u64,
    ) -> Option<Probe> {
        self.probes
            .retain(|_, (_, asked)| now.saturating_sub(*asked) <= PROBE_TIMEOUT_SECS);
        if let Some((probe, _)) = self.probes.get(&node) {
            return Some(probe.clone());
        }
        let due = self
            .probed_at
            .get(&node)
            .is_none_or(|at| now.saturating_sub(*at) >= PROBE_INTERVAL_SECS);
        if !serves_data || !due {
            return None;
        }
        let targets: Vec<Host> = self
            .hosts
            .online(now)
            .into_iter()
            .filter(|host| host.account != account)
            .collect();
        if targets.is_empty() {
            return None;
        }
        let target = &targets[self.rng.random_range(0..targets.len())];
        let probe = Probe {
            seed: ContentHash::from_bytes(self.rng.random()),
            target: target.node,
            address: target.address.clone(),
            bytes: PROBE_BYTES,
        };
        self.probed_at.insert(node, now);
        self.probes.insert(node, (probe.clone(), now));
        Some(probe)
    }

    /// Takes in a measurement `node` was asked to make: if the data's hash
    /// shows it fetched the test data, the time counts toward the measured
    /// node's bandwidth.
    pub fn report_probe(
        &mut self,
        node: &PublicKey,
        seed: ContentHash,
        millis: u64,
        digest: ContentHash,
        now: u64,
    ) -> Result<(), ProbeError> {
        let Some((probe, _)) = self
            .probes
            .get(node)
            .filter(|(probe, _)| probe.seed == seed)
        else {
            return Err(ProbeError::NotAsked);
        };
        if ContentHash::of(&probe::data(&probe.seed, probe.bytes)) != digest {
            return Err(ProbeError::WrongData);
        }
        let event = ScoreEvent {
            node: probe.target,
            at: now,
            what: Conduct::Transferred {
                bytes: probe.bytes,
                millis,
            },
        };
        self.probes.remove(node);
        self.scores.apply(&event);
        self.scores_log.append(&event);
        Ok(())
    }

    /// What a job would cost if it were submitted now.
    pub fn quote(&self, manifest: &Manifest) -> Result<Quote, SubmitError> {
        let pieces = manifest
            .split
            .as_ref()
            .map_or(1, |split| split.pieces.len().max(1) as u64);
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
        transcript: Option<ContentHash>,
        now: u64,
    ) -> Result<(), JobError> {
        self.jobs
            .commit_result(node, job_id, commitment, transcript, now, &mut self.rng)
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
    ) -> Result<JobId, SubmitError> {
        if !self.taking_jobs {
            return Err(SubmitError::NotTakingJobs);
        }
        if manifest.reads.len() > MAX_READS {
            return Err(SubmitError::TooManyReads);
        }
        if manifest.keep_from.len() > MAX_KEPT_FROM || manifest.apart_from.len() > MAX_APART_FROM {
            return Err(SubmitError::KeptFromTooMany);
        }
        let pieces = manifest.split.iter().flat_map(|split| &split.pieces);
        let always_on = manifest.fallback == Some(Fallback::AlwaysOn);
        for hash in [manifest.module, manifest.input]
            .iter()
            .chain(&manifest.reads)
            .chain(pieces)
        {
            // Nodes fetch every blob from storage nodes; a job that may fall
            // back to always-on also leaves them with the control plane,
            // which runs it then.
            if !self.storage.is_stored(hash) || (always_on && !self.blobs.contains(hash)) {
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
        privacy::check_manifest(&manifest)?;
        let job_id = JobId::of(&request.signer, &request.payload, &request.signature);

        // The jobs that will actually run: this one, or its pieces.
        let mut runs = vec![(job_id, manifest.clone())];
        if let Some(split) = &manifest.split {
            if split.pieces.is_empty() || split.pieces.len() as u64 > MAX_PIECES {
                return Err(SubmitError::TooManyPieces);
            }
            runs.clear();
            for (index, input) in split.pieces.iter().enumerate() {
                let piece = Manifest {
                    input: *input,
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

    /// Stores a blob a job produced, for a node that holds a lease on the job,
    /// paid by the job's payer for `PRODUCED_RETENTION_DAYS` days. A blob that
    /// is already stored is not stored again, and the answer names no nodes:
    /// a second node that ran the job finds its output there.
    pub fn store_produced(
        &mut self,
        node: &PublicKey,
        job_id: JobId,
        root: ContentHash,
        size: u64,
        now: u64,
    ) -> Result<Placement, StoreError> {
        if !self.jobs.holds_lease(&job_id, node) {
            return Err(StoreError::NotLeased);
        }
        if self.storage.locate(&root).is_some() {
            return Ok(Placement {
                addresses: Vec::new(),
            });
        }
        let owner = self.payer_of(&job_id).ok_or(StoreError::NoPayer)?;
        let blob = Storing {
            root,
            size,
            owner,
            days: PRODUCED_RETENTION_DAYS,
            addressed: None,
        };
        self.store_blob(blob, now)
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
        let (opened, escrow) = self.relays.open(opening, &online, now, &mut self.rng)?;
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

    /// Whether `account` may act for the treasury.
    pub fn is_treasurer(&self, account: &AccountId) -> bool {
        self.treasurers.contains(account)
    }

    /// Connects the ICRC ledger that holds copies of the token, which opens
    /// withdrawals and deposits.
    pub fn connect_bridge(&mut self, bridge: TokenBridge) {
        self.bridge = Some(bridge);
    }

    fn bridge(&self) -> Result<&TokenBridge, TokenError> {
        self.bridge.as_ref().ok_or(TokenError::NoBridge)
    }

    /// Asks for an account's tokens to be withdrawn as copies on the ICRC
    /// ledger. The host mints them (`withdrawals`) and records how that went.
    pub fn withdraw_tokens(
        &mut self,
        id: ContentHash,
        account: AccountId,
        amount: Tokens,
        to: IcrcAccount,
        now: u64,
    ) -> Result<(), TokenError> {
        self.bridge()?;
        let entry = LedgerEntry::WithdrawTokens {
            id,
            account,
            amount,
            to,
            at: now,
        };
        Ok(self.record(entry)?)
    }

    /// The withdrawals that wait for their copies to be minted.
    pub fn withdrawals(&self) -> Vec<Withdrawal> {
        self.ledger.withdrawals()
    }

    /// Where `account` deposits copies of the token.
    pub fn deposit_address(&self, account: &AccountId) -> Result<DepositAddress, TokenError> {
        let bridge = self.bridge()?;
        Ok(DepositAddress {
            ledger: bridge.ledger.clone(),
            account: IcrcAccount {
                owner: bridge.minter.clone(),
                subaccount: Some(deposit_subaccount(&bridge.secret, account)),
            },
        })
    }

    /// Asks the host to check an account's deposit address (`deposit_checks`).
    pub fn ask_deposit(&mut self, account: AccountId) -> Result<(), TokenError> {
        self.bridge()?;
        if !self.deposit_checks.contains(&account) {
            self.deposit_checks.push(account);
        }
        Ok(())
    }

    /// The deposit addresses to check, each with its account: the host burns
    /// what it finds there and records `TokensDeposited`.
    pub fn take_deposit_checks(&mut self) -> Vec<(AccountId, Subaccount)> {
        let Some(bridge) = &self.bridge else {
            return Vec::new();
        };
        let secret = bridge.secret;
        self.deposit_checks
            .drain(..)
            .map(|account| (account, deposit_subaccount(&secret, &account)))
            .collect()
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
            let text = serde_json::to_string(&draw).expect("draws serialize");
            self.draw_hashes.push(draw_hash(&text));
            self.draws_log.append_line(text);
        }
        for closed in self.jobs.take_closed() {
            let job_id = closed.job_id;
            let mut asked = None;
            self.splits.piece_closed(&closed, now);
            let terminal = matches!(
                closed.how,
                Closure::Settled { .. }
                    | Closure::Rejected
                    | Closure::Expired
                    | Closure::FallBack(_)
            );
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
                Closure::WrongInDispute { node, cost } => {
                    self.forfeit(&node, |staked| cost.min(staked));
                    vec![]
                }
            };
            for entry in entries.into_iter().flatten() {
                self.record(entry).expect("a closed job can be settled");
            }
            if terminal {
                self.escrow_payer.remove(&job_id);
            }
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

    /// The jobs to run as an always-on fallback, each with what it reads.
    pub fn take_fall_backs(&mut self) -> Vec<(JobId, FallBackRun)> {
        let fall_backs = std::mem::take(&mut self.fall_backs);
        let blob = |hash: &ContentHash| -> Vec<u8> {
            self.blobs.get(hash).ok().flatten().unwrap_or_default()
        };
        fall_backs
            .into_iter()
            .map(|(job_id, job)| {
                let run = FallBackRun {
                    seed: job.seed,
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
                    control_key: self.control.public_key(),
                };
                (job_id, run)
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
/// Whether a settlement log entry posts a devices root.
fn posts_devices(entry: &Envelope) -> bool {
    matches!(
        entry.open::<LogEntry>(),
        Ok(LogEntry {
            posted: Posted::Devices { .. },
            ..
        })
    )
}

fn replay<T: DeserializeOwned>(
    store: &mut dyn StateStore,
    name: &str,
    mut apply: impl FnMut(T) -> Result<(), String>,
) -> io::Result<Log> {
    let log = store.log(name)?;
    for (index, line) in log.lines()?.enumerate() {
        let applied = serde_json::from_str(&line?)
            .map_err(|e| e.to_string())
            .and_then(&mut apply);
        applied.map_err(|reason| {
            let message = format!("the {name} log, line {}: {reason}", index + 1);
            io::Error::new(io::ErrorKind::InvalidData, message)
        })?;
    }
    Ok(log)
}

/// What a name's deposit is set aside under in the ledger.
fn name_deposit_id(name: &str) -> ContentHash {
    ContentHash::of(format!("decomp/name-deposit/v1\n{name}").as_bytes())
}
