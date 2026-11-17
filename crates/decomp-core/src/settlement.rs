//! Settlement: batches of receipts, and the signed log they are posted to (spec
//! section 6).
//!
//! "The ledger records balances and batched proof of the work done; it never
//! sees individual jobs." A batch is one fingerprint over the receipts of a
//! period and the net change to each balance. "During development and testing,
//! a single coordinator and a local signed log stand in for the operators and
//! the ledger": the log here is that stand-in. Each entry is signed by the
//! operator and names the entry before it, so anyone who is given the log can
//! check it from the first entry and ends up with every balance.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::account::Accounts;
use crate::envelope::{Envelope, EnvelopeError};
use crate::job::Receipt;
use crate::keys::{AccountId, ContentHash, JobId, LedgerAddress, PublicKey};
use crate::ledger::{Credits, FeeSplit, Holder, Ledger, LedgerEntry};
use crate::merkle;
use crate::request::{Call, Request};

/// How often the coordinator posts a batch, in seconds (spec section 6: "Every
/// 10 minutes (starting value)"). A shorter period proves work sooner; a longer
/// one writes to the ledger less often.
pub const BATCH_PERIOD_SECS: u64 = 600;

/// How long after a batch is posted another operator may dispute it, in
/// seconds (spec section 6: operators "audit each other's batches during a
/// dispute window"). The spec gives the window no length. An hour is six
/// batch periods: time for an operator that was away for a while to catch up.
/// Until it has passed, the operator that posted the batch cannot take its
/// stake back.
pub const DISPUTE_WINDOW_SECS: u64 = 3600;

/// The most log entries handed over in one answer. Whoever wants more asks
/// again from where the answer ended.
pub const MAX_LOG_PAGE: usize = 256;

/// The names of the protocol's two pools, which their ledger addresses are made
/// from (see `LedgerAddress::of_pool`).
const VERIFICATION_FUND: &str = "verification-fund";
const TREASURY: &str = "treasury";

/// The net change to one balance over a batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Change {
    /// Whose balance, by ledger address and never by account ID.
    pub address: LedgerAddress,
    /// The change in millionths of a credit: negative if the balance fell.
    pub change: i64,
}

/// How far into the operator's records a batch reaches: how many account
/// changes, credit movements, and receipts there had been when it was made.
/// The receipts a batch combines are those after the batch before it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Through {
    pub ops: u64,
    pub entries: u64,
    pub receipts: u64,
}

/// What the coordinator posts each period (spec section 6, "Batches": it
/// "combines the new receipts into one fingerprint and posts it with the net
/// balance change per account").
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Batch {
    /// Batches are numbered from zero, in the order they are posted.
    pub number: u64,
    /// The root of a hash tree over the period's receipts, in the order they
    /// were made (see `merkle::list_root`). Each receipt goes in as the hash of
    /// its text.
    pub fingerprint: ContentHash,
    /// One entry for every balance that changed, in order of address.
    pub changes: Vec<Change>,
    pub through: Through,
}

/// One entry of the settlement log, which travels signed by the operator.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogEntry {
    /// The hash of the entry before this one (see `entry_hash`), or of nothing
    /// for the first. This is what makes the log append-only: an entry cannot
    /// be changed or removed without every later one ceasing to fit.
    pub previous: ContentHash,
    /// When the entry was made, in seconds since 1970.
    pub at: u64,
    pub posted: Posted,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Posted {
    Batch(Batch),
    Dispute(UpheldDispute),
}

/// A dispute that was upheld: another operator audited a batch, found that the
/// records behind it give something else, and the keeper of the log found the
/// same (spec section 6: operators "audit each other's batches", and "an
/// operator that posts a bad batch forfeits stake").
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpheldDispute {
    /// The auditing operator's signed request, whose call is `DisputeBatch`. It
    /// names the batch and gives the fingerprint the batch should have had,
    /// which is the batch's fingerprint from here on.
    pub request: Envelope,
}

/// The records behind an operator's batches, as one operator hands them to
/// another: signed account changes, credit movements, and receipts as text.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct BatchRecords {
    pub ops: Vec<Envelope>,
    pub entries: Vec<LedgerEntry>,
    pub receipts: Vec<String>,
}

/// What names a signed log entry in the entry after it: the hash of its text
/// and its signature.
pub fn entry_hash(entry: &Envelope) -> ContentHash {
    let mut hasher = Sha256::new();
    hasher.update(&entry.payload);
    hasher.update(entry.signature.as_bytes());
    ContentHash::from_bytes(hasher.finalize().into())
}

/// A page of signed entries, oldest first: of the settlement log, or of the
/// draws the operator has published.
#[derive(Debug, Serialize, Deserialize)]
pub struct LogPage {
    pub entries: Vec<Envelope>,
}

/// Proof that a receipt is in a batch (spec section 6: "Any node can check that
/// its own receipts are included").
#[derive(Debug, Serialize, Deserialize)]
pub struct Inclusion {
    /// The number of the batch.
    pub batch: u64,
    /// The receipt's place among the batch's receipts, counting from zero.
    pub index: u64,
    /// The receipt, as the exact text whose hash went into the fingerprint.
    pub receipt: String,
    /// The hashes that link the receipt to the fingerprint.
    pub proof: Vec<ContentHash>,
}

#[derive(Debug, thiserror::Error)]
pub enum LogError {
    #[error(transparent)]
    Envelope(#[from] EnvelopeError),
    #[error("the entry is not signed by the operator that signed the first entry")]
    NotTheOperator,
    #[error("the entry does not follow the entry before it")]
    BrokenChain,
    #[error("the entry is dated before the entry before it")]
    OutOfOrder,
    #[error("the batch is numbered {found}, and the next batch is number {expected}")]
    WrongNumber { expected: u64, found: u64 },
    #[error("the batch reaches less far into the records than the batch before it")]
    GoesBack,
    #[error("the batch's changes are not one for each address, in order, and none of nothing")]
    BadChanges,
    #[error("the batch takes the balance at {0} below nothing")]
    Overdrawn(LedgerAddress),
    #[error("the dispute does not carry a signed request to dispute a batch")]
    NotADispute,
    #[error("the dispute is the operator's own")]
    OwnDispute,
    #[error("the dispute names a batch that is not in the log")]
    NoSuchBatch,
    #[error("the batch was already disputed")]
    AlreadyDisputed,
}

/// The settlement log as far as it has been checked: who signs it, where it
/// ends, its batches, and the balance at every ledger address. It changes only
/// by `apply`, which checks each entry against the ones before it.
#[derive(Default)]
pub struct SettlementLog {
    operator: Option<PublicKey>,
    /// The hash of the last entry, and when it was made.
    last: Option<(ContentHash, u64)>,
    batches: Vec<Batch>,
    /// When each batch was posted, and the numbers of those that were disputed.
    posted_at: Vec<u64>,
    disputed: BTreeSet<u64>,
    balances: BTreeMap<LedgerAddress, Credits>,
}

impl SettlementLog {
    /// Checks the next entry of the log and takes it in. On any error the log
    /// is left as it was.
    pub fn apply(&mut self, entry: &Envelope) -> Result<(), LogError> {
        let LogEntry {
            previous,
            at,
            posted,
        } = entry.open()?;
        if self
            .operator
            .is_some_and(|operator| operator != entry.signer)
        {
            return Err(LogError::NotTheOperator);
        }
        let (expected, last_at) = self.last.unwrap_or((ContentHash::of(&[]), 0));
        if previous != expected {
            return Err(LogError::BrokenChain);
        }
        if at < last_at {
            return Err(LogError::OutOfOrder);
        }
        match posted {
            Posted::Batch(batch) => {
                self.take_batch(batch)?;
                self.posted_at.push(at);
            }
            Posted::Dispute(dispute) => self.take_dispute(&dispute, &entry.signer)?,
        }
        self.operator = Some(entry.signer);
        self.last = Some((entry_hash(entry), at));
        Ok(())
    }

    fn take_batch(&mut self, batch: Batch) -> Result<(), LogError> {
        let expected = self.batches.len() as u64;
        if batch.number != expected {
            return Err(LogError::WrongNumber {
                expected,
                found: batch.number,
            });
        }
        let (before, through) = (self.through(), batch.through);
        if through.ops < before.ops
            || through.entries < before.entries
            || through.receipts < before.receipts
        {
            return Err(LogError::GoesBack);
        }
        let in_order = batch
            .changes
            .windows(2)
            .all(|pair| pair[0].address < pair[1].address);
        if !in_order || batch.changes.iter().any(|change| change.change == 0) {
            return Err(LogError::BadChanges);
        }
        let mut balances = self.balances.clone();
        for Change { address, change } in &batch.changes {
            let balance = balances.entry(*address).or_default();
            let after = balance.micros().checked_add_signed(*change);
            *balance = Credits::from_micros(after.ok_or(LogError::Overdrawn(*address))?);
        }
        self.balances = balances;
        self.batches.push(batch);
        Ok(())
    }

    /// Takes in an upheld dispute. The batch stays where it is, marked, and
    /// its fingerprint becomes the one the dispute gives. Balances are not
    /// touched: the next batch is the difference between what the records hold
    /// and what the log holds, so it puts right whatever this one got wrong.
    fn take_dispute(
        &mut self,
        dispute: &UpheldDispute,
        operator: &PublicKey,
    ) -> Result<(), LogError> {
        let request: Request = dispute.request.open()?;
        let Call::DisputeBatch {
            batch, fingerprint, ..
        } = request.call
        else {
            return Err(LogError::NotADispute);
        };
        if dispute.request.signer == *operator {
            return Err(LogError::OwnDispute);
        }
        if self.disputed.contains(&batch) {
            return Err(LogError::AlreadyDisputed);
        }
        let index = usize::try_from(batch).map_err(|_| LogError::NoSuchBatch)?;
        let found = self.batches.get_mut(index).ok_or(LogError::NoSuchBatch)?;
        found.fingerprint = fingerprint;
        self.disputed.insert(batch);
        Ok(())
    }

    /// When batch `number` was posted, in seconds since 1970.
    pub fn posted_at(&self, number: u64) -> Option<u64> {
        self.posted_at.get(usize::try_from(number).ok()?).copied()
    }

    /// Whether a dispute of batch `number` was upheld.
    pub fn is_disputed(&self, number: u64) -> bool {
        self.disputed.contains(&number)
    }

    /// The operator that signs the log, once it has an entry.
    pub fn operator(&self) -> Option<PublicKey> {
        self.operator
    }

    pub fn batches(&self) -> &[Batch] {
        &self.batches
    }

    /// The balance the log has at `address`.
    pub fn balance(&self, address: &LedgerAddress) -> Credits {
        self.balances.get(address).copied().unwrap_or_default()
    }

    /// How far into the operator's records the last batch reaches.
    pub fn through(&self) -> Through {
        self.batches
            .last()
            .map(|batch| batch.through)
            .unwrap_or_default()
    }

    /// Where the receipts of batch `number` begin and end among all receipts.
    pub fn receipts_of(&self, number: u64) -> Option<(u64, u64)> {
        let index = usize::try_from(number).ok()?;
        let end = self.batches.get(index)?.through.receipts;
        let start = match index.checked_sub(1) {
            Some(before) => self.batches[before].through.receipts,
            None => 0,
        };
        Some((start, end))
    }

    /// The batch that holds the receipt at place `receipt` among all receipts.
    pub fn batch_holding(&self, receipt: u64) -> Option<&Batch> {
        let index = self
            .batches
            .partition_point(|batch| batch.through.receipts <= receipt);
        self.batches.get(index)
    }

    /// The batch to post next, or `None` if there is nothing new to post: no
    /// receipt since the last batch, and no balance that differs from the log's.
    /// `receipts` are the hashes of the receipts since the last batch, and
    /// `holdings` is what sits at every ledger address now.
    pub fn next_batch(
        &self,
        receipts: &[ContentHash],
        holdings: &BTreeMap<LedgerAddress, Credits>,
        through: Through,
    ) -> Option<Batch> {
        let batch = self.batch_from(receipts, holdings, through);
        (!receipts.is_empty() || !batch.changes.is_empty()).then_some(batch)
    }

    /// The batch the records give at this point in the log, whether or not
    /// there is anything in it.
    fn batch_from(
        &self,
        receipts: &[ContentHash],
        holdings: &BTreeMap<LedgerAddress, Credits>,
        through: Through,
    ) -> Batch {
        let addresses = holdings.keys().chain(self.balances.keys());
        let addresses: BTreeSet<&LedgerAddress> = addresses.collect();
        let change = |address: &LedgerAddress| {
            let now = holdings.get(address).copied().unwrap_or_default();
            now.micros() as i64 - self.balance(address).micros() as i64
        };
        let changes: Vec<Change> = addresses
            .into_iter()
            .map(|address| Change {
                address: *address,
                change: change(address),
            })
            .filter(|change| change.change != 0)
            .collect();
        Batch {
            number: self.batches.len() as u64,
            fingerprint: merkle::list_root(receipts),
            changes,
            through,
        }
    }

    /// The entry that posts `posted` next in this log, for the operator to sign.
    pub fn entry(&self, at: u64, posted: Posted) -> LogEntry {
        let (previous, last_at) = self.last.unwrap_or((ContentHash::of(&[]), 0));
        LogEntry {
            previous,
            // A clock that has gone back must not make the log unreadable.
            at: at.max(last_at),
            posted,
        }
    }

    /// Whether `inclusion` shows its receipt to be in the batch it names.
    pub fn includes(&self, inclusion: &Inclusion) -> bool {
        let Some((start, end)) = self.receipts_of(inclusion.batch) else {
            return false;
        };
        let batch = &self.batches[inclusion.batch as usize];
        merkle::list_holds(
            &batch.fingerprint,
            end - start,
            inclusion.index,
            &merkle::item_hash(inclusion.receipt.as_bytes()),
            &inclusion.proof,
        )
    }
}

/// What sits at every ledger address: each account's holdings under its own
/// address, and the two pools under theirs. This is where account IDs are left
/// behind: nothing that is posted names one.
pub fn addressed(ledger: &Ledger, accounts: &Accounts) -> BTreeMap<LedgerAddress, Credits> {
    let address = |holder: &Holder| match holder {
        // Every balance belongs to an account that has a record.
        Holder::Account(id) => accounts.get(id).map(|account| account.ledger_address),
        Holder::VerificationFund => Some(LedgerAddress::of_pool(VERIFICATION_FUND)),
        Holder::Treasury => Some(LedgerAddress::of_pool(TREASURY)),
    };
    let holdings = ledger.holdings();
    let addressed = holdings
        .iter()
        .filter_map(|(holder, credits)| Some((address(holder)?, *credits)));
    addressed.collect()
}

/// What an audit found a batch should have been.
#[derive(Debug)]
pub struct Finding {
    /// The number of the batch that does not hold.
    pub batch: u64,
    /// The fingerprint and changes the records give for it.
    pub fingerprint: ContentHash,
    pub changes: Vec<Change>,
    /// What is wrong with the batch as posted.
    pub reason: String,
}

#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    #[error("the log does not hold: {0}")]
    Log(#[from] LogError),
    #[error("the records behind the log cannot be followed: {0}")]
    Records(String),
}

/// One operator's audit of another's batches (spec section 6: "Several staked
/// operators run coordinators and audit each other's batches during a dispute
/// window"). It follows the other operator's log entry by entry, with the
/// records behind each batch, and works every batch out again from them by
/// the rules the operator itself is held to.
///
/// What it checks of a batch: that its fingerprint is that of the period's
/// receipts; that each receipt holds reports signed by nodes that agree with
/// it; that each job paid for in the period has its receipt in the batch and
/// was paid as the receipt says, to accounts of nodes that signed it; and that
/// the balance changes are what the records give. Credit movements that no
/// receipt stands behind (data services, stake, forfeits, the pay of
/// checkers, credits bought, and cash-outs) are followed by the ledger's own
/// rules and otherwise taken on the operator's word.
#[derive(Default)]
pub struct Auditor {
    log: SettlementLog,
    accounts: Accounts,
    ledger: Ledger,
}

impl Auditor {
    /// An audit of an operator in whose records a recovery waits
    /// `recovery_wait` seconds (see `Accounts::waiting`).
    pub fn waiting(recovery_wait: u64) -> Self {
        Self {
            accounts: Accounts::waiting(recovery_wait),
            ..Self::default()
        }
    }

    /// The audited operator's log, as far as it has been followed.
    pub fn log(&self) -> &SettlementLog {
        &self.log
    }

    /// How far into the operator's records `entry` reaches, if it posts a
    /// batch. `audit` must be given the records from where the log reaches
    /// now up to there.
    pub fn reach(entry: &Envelope) -> Result<Option<Through>, LogError> {
        let LogEntry { posted, .. } = entry.open()?;
        Ok(match posted {
            Posted::Batch(batch) => Some(batch.through),
            Posted::Dispute(_) => None,
        })
    }

    /// Follows the next entry of the log. For a batch, `records` are the
    /// operator's records from the batch before up to this one, and the
    /// answer is what the batch should have been if it does not hold.
    pub fn audit(
        &mut self,
        entry: &Envelope,
        records: &BatchRecords,
    ) -> Result<Option<Finding>, AuditError> {
        let Some(through) = Self::reach(entry)? else {
            self.log.apply(entry)?;
            return Ok(None);
        };
        let LogEntry { posted, .. } = entry.open().map_err(LogError::from)?;
        let Posted::Batch(posted) = posted else {
            unreachable!("only a batch reaches into the records");
        };

        let before = self.log.through();
        for op in &records.ops {
            let applied = self.accounts.apply(op);
            let account = applied.map_err(|error| AuditError::Records(error.to_string()))?;
            self.ledger.open(account.id);
        }
        for entry in &records.entries {
            let applied = self.ledger.apply(entry);
            applied.map_err(|error| AuditError::Records(error.to_string()))?;
        }
        let hash = |receipt: &String| merkle::item_hash(receipt.as_bytes());
        let hashes: Vec<ContentHash> = records.receipts.iter().map(hash).collect();
        let holdings = addressed(&self.ledger, &self.accounts);
        let expected = self.log.batch_from(&hashes, &holdings, through);

        let handed = Through {
            ops: before.ops + records.ops.len() as u64,
            entries: before.entries + records.entries.len() as u64,
            receipts: before.receipts + records.receipts.len() as u64,
        };
        let reason = if handed != through {
            Some("the operator did not hand over the records the batch reaches to".into())
        } else if expected.fingerprint != posted.fingerprint {
            Some("its fingerprint is not that of the period's receipts".into())
        } else if expected.changes != posted.changes {
            Some("its balance changes are not what the records give".into())
        } else {
            unbacked(records, &self.accounts)
        };
        self.log.apply(entry)?;
        Ok(reason.map(|reason| Finding {
            batch: posted.number,
            fingerprint: expected.fingerprint,
            changes: expected.changes,
            reason,
        }))
    }
}

/// What is wrong with the receipts of a period or the payments made on them,
/// if anything is.
fn unbacked(records: &BatchRecords, accounts: &Accounts) -> Option<String> {
    let mut receipts: HashMap<JobId, Receipt> = HashMap::new();
    for text in &records.receipts {
        let Ok(receipt) = serde_json::from_str::<Receipt>(text) else {
            return Some("one of its receipts cannot be read".into());
        };
        if !is_sound(&receipt) {
            let job_id = receipt.job_id;
            return Some(format!(
                "the receipt of job {job_id} is not backed by its nodes' signed reports"
            ));
        }
        receipts.insert(receipt.job_id, receipt);
    }
    for entry in &records.entries {
        let LedgerEntry::Settle {
            job_id,
            split,
            node_accounts,
        } = entry
        else {
            continue;
        };
        let Some(receipt) = receipts.get(job_id) else {
            return Some(format!(
                "job {job_id} was paid for and has no receipt in the batch"
            ));
        };
        let signers: Vec<AccountId> = receipt
            .results
            .iter()
            .filter_map(|report| accounts.account_of(&report.signer))
            .map(|account| account.id)
            .collect();
        let as_receipted = *split == receipt.fee
            && *split == FeeSplit::of(receipt.units, node_accounts.len() as u32)
            && !node_accounts.is_empty()
            && node_accounts.iter().all(|paid| signers.contains(paid));
        if !as_receipted {
            return Some(format!("job {job_id} was not paid for as its receipt says"));
        }
    }
    None
}

/// Whether a receipt holds at least one report, each signed and each saying
/// what the receipt says: the same job, units, and outcome.
fn is_sound(receipt: &Receipt) -> bool {
    let agrees = |report: &Envelope| {
        let Ok(Request { call, .. }) = report.open() else {
            return false;
        };
        let Call::SubmitResult { result } = call else {
            return false;
        };
        result.job_id == receipt.job_id
            && result.units == receipt.units
            && result.outcome == receipt.outcome
    };
    !receipt.results.is_empty() && receipt.results.iter().all(agrees)
}
