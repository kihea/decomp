//! Everything the coordinator knows, and where each part is kept.
//!
//! | Part | Kept in | Survives a restart |
//! | --- | --- | --- |
//! | Account records | `ops.jsonl`, replayed at startup | Yes |
//! | Balances | `ops.jsonl` (starter grants) and `ledger.jsonl`, replayed at startup | Yes |
//! | Node scores | `scores.jsonl`, replayed at startup | Yes |
//! | Blobs | `blobs/`, one file per hash | Yes |
//! | Receipts | `receipts.jsonl` | Yes |
//! | Which nodes hold which blobs | `storage.jsonl`, replayed at startup | Yes |
//! | Relay channels | `relays.jsonl`, replayed at startup | Yes |
//! | The sealing key each device published | `sealing.jsonl`, replayed at startup | Yes |
//! | What each shard of app state holds now | `state.jsonl`, replayed at startup | Yes |
//! | Open jobs, online nodes, and unanswered challenges | Memory | No, by design (spec section 6) |
//!
//! The log files are the "local signed log" that stands in for the ledger during
//! development (spec section 6).

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;

use decomp_core::{
    Account, AccountId, AccountStatus, Accounts, Assignment, Challenge, CheckedIn, Closure,
    Conduct, ContentHash, Credits, Envelope, EnvelopeError, FollowUps, Hosts, IncomingChannels,
    JobError, JobId, JobStatus, Jobs, Ledger, LedgerEntry, LedgerError, Located, MAX_PIECES,
    MAX_READS, MAX_SHARD_NAME_BYTES, Manifest, Merge, OpError, Opening, Outcome,
    PROBATION_FORFEIT_PERCENT, Penalty, Placement, PublicKey, RelayEntry, RelayError, Relays,
    Route, ScoreEvent, Scores, SealingKeys, ShardRef, Splits, SponsorError, Sponsored, Standing,
    StateEntry, States, Storage, StorageEntry, StorageError, Storing, channel_id, fee,
};
use decomp_sandbox::Sandbox;
use decomp_schema::records::{self, Records};
use decomp_schema::{Mismatch, Signature};
use serde::de::DeserializeOwned;

use crate::blobs::Blobs;
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
}

pub struct Coordinator {
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

impl Coordinator {
    pub fn open(data_dir: &Path, challenge_interval: u64) -> io::Result<Self> {
        fs::create_dir_all(data_dir)?;

        let mut accounts = Accounts::default();
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

        // Open jobs do not survive a restart, so hand back what was set aside for
        // any job that was still open when the coordinator last stopped.
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
            jobs: Jobs::default(),
            sponsored: Sponsored::default(),
            splits: Splits::default(),
            signatures: HashMap::new(),
            receipts: Log::open(&data_dir.join("receipts.jsonl"))?,
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

    /// Applies a change to an account and records it. A new account gets its
    /// starter grant.
    pub fn apply_op(&mut self, envelope: &Envelope) -> Result<AccountStatus, OpError> {
        let account = self.accounts.apply(envelope)?.clone();
        self.ledger.open(account.id);
        self.ops.append(envelope);
        Ok(self.status(account))
    }

    pub fn status(&self, account: Account) -> AccountStatus {
        let balance = self.ledger.balance(&account.id);
        AccountStatus { account, balance }
    }

    /// How far a node is trusted, given its scores and its account's stake.
    pub fn standing(&self, node: &PublicKey, account: &AccountId) -> Standing {
        self.scores
            .standing(node, self.ledger.balance(account).staked)
    }

    /// Records that a node is online and returns what it should be doing: the
    /// job it should be running, blobs to take in, a storage challenge, and
    /// channels to carry.
    pub fn check_in(
        &mut self,
        node: PublicKey,
        account: AccountId,
        address: Option<String>,
        now: u64,
    ) -> CheckedIn {
        self.hosts.seen(node, account, address, now);
        let interval = self.challenge_interval;
        CheckedIn {
            assignment: self.assignment_for(node, account, now),
            store: self.storage.orders_for(&node),
            challenge: self
                .storage
                .challenge_for(node, now, interval, &mut rand::rng()),
            relay: self.relays.orders_for(&node, now),
        }
    }

    fn assignment_for(
        &mut self,
        node: PublicKey,
        account: AccountId,
        now: u64,
    ) -> Option<Assignment> {
        let Self {
            jobs,
            scores,
            ledger,
            states,
            ..
        } = self;
        let standing = |node: &PublicKey, account: &AccountId| {
            scores.standing(node, ledger.balance(account).staked)
        };
        // `settle` runs after every request, so the contents read here already
        // include what every verified job left behind.
        let contents = |shard: &ShardRef| states.contents(shard);
        jobs.check_in(node, account, now, &mut rand::rng(), &standing, &contents)
    }

    /// Accepts a job: checks it, sets its highest possible fee aside, and queues
    /// it. The fee comes from the submitter's account, or from the app the
    /// manifest names as sponsor if that app agrees to pay. A job with a split rule is cut into pieces here, each queued as a job
    /// of its own on its own part of the input.
    pub fn submit_job(
        &mut self,
        signer: &PublicKey,
        account: AccountId,
        payload: &str,
        manifest: Manifest,
        now: u64,
        sandbox: &Sandbox,
    ) -> Result<JobId, SubmitError> {
        if manifest.reads.len() > MAX_READS {
            return Err(SubmitError::TooManyReads);
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
        self.check_types(&manifest, sandbox)?;
        let job_id = JobId::of(signer, payload);

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
                    max_fee: manifest.highest_fee(),
                    ..manifest.clone()
                };
                runs.push((
                    JobId::of(signer, &format!("{payload} piece {index}")),
                    piece,
                ));
            }
        }

        let total = |amount: fn(&Manifest) -> Credits| {
            runs.iter()
                .fold(Credits::ZERO, |sum, (_, run)| sum + amount(run))
        };
        let highest = total(Manifest::highest_fee);
        if manifest.max_fee < highest {
            return Err(SubmitError::FeeCapTooLow(highest));
        }
        for (id, run) in &runs {
            self.jobs.check(id, run, now)?;
        }
        let to_set_aside = total(|run| run.max_fee);
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
            self.jobs.submit(*id, account, run.clone(), now)?;
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

    /// Refuses an input that does not fit the types the module says its job takes
    /// (spec section 7, "Typing"). A module that does not describe itself takes
    /// any input.
    fn check_types(&mut self, manifest: &Manifest, sandbox: &Sandbox) -> Result<(), SubmitError> {
        if !self.signatures.contains_key(&manifest.module) {
            let module = self.blobs.get(&manifest.module)?.unwrap_or_default();
            let described = sandbox.signature(&module).ok().flatten();
            let signature = described.and_then(|bytes| serde_json::from_slice(&bytes).ok());
            self.signatures.insert(manifest.module, signature);
        }
        let Some(signature) = &self.signatures[&manifest.module] else {
            return Ok(());
        };
        // Cutting typed input at byte boundaries would hand each piece broken data.
        if manifest.split.is_some() {
            return Err(SubmitError::TypedJobSplitByBytes);
        }
        let input = self.blobs.get(&manifest.input)?.unwrap_or_default();
        signature
            .check_input(&input)
            .map_err(SubmitError::TypeMismatch)
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
        let (placed, escrow) = self.storage.place(blob, online, now, &mut rand::rng())?;
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
        for (entry, refund) in self.storage.sweep(now) {
            self.note_storage(entry, refund);
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
                Closure::Verified {
                    receipt,
                    node_accounts,
                    checker,
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
                    self.receipts.append(&receipt);
                    let amount = receipt.fee.per_node;
                    let settle = LedgerEntry::Settle {
                        job_id,
                        split: receipt.fee,
                        node_accounts,
                    };
                    let pay = checker.map(|account| LedgerEntry::PayChecker { account, amount });
                    vec![Some(settle), pay]
                }
                Closure::Disputed | Closure::Expired => {
                    vec![Some(LedgerEntry::Refund { job_id })]
                }
                Closure::KnownAnswerPassed { account, reward } => {
                    vec![Some(LedgerEntry::PayChecker {
                        account,
                        amount: reward,
                    })]
                }
            };
            for entry in entries.into_iter().flatten() {
                self.record(entry).expect("a closed job can be settled");
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
            spare = spare - amount;
            self.jobs
                .submit(id, submitter, manifest, now)
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
        for node in &verdict.wrong {
            self.forfeit(node, |_| fee(verdict.units, 1));
        }
        self.settle(now);
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

/// Opens a log and applies every entry already in it.
fn replay<T: DeserializeOwned>(
    path: &Path,
    mut apply: impl FnMut(T) -> Result<(), String>,
) -> io::Result<Log> {
    let log = Log::open(path)?;
    for (index, line) in log.lines().enumerate() {
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
