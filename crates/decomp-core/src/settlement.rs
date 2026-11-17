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

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::account::Accounts;
use crate::envelope::{Envelope, EnvelopeError};
use crate::keys::{ContentHash, LedgerAddress, PublicKey};
use crate::ledger::{Credits, Holder, Ledger};
use crate::merkle;

/// How often the coordinator posts a batch, in seconds (spec section 6: "Every
/// 10 minutes (starting value)"). A shorter period proves work sooner; a longer
/// one writes to the ledger less often.
pub const BATCH_PERIOD_SECS: u64 = 600;

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
}

/// What names a signed log entry in the entry after it: the hash of its text
/// and its signature.
pub fn entry_hash(entry: &Envelope) -> ContentHash {
    let mut hasher = Sha256::new();
    hasher.update(&entry.payload);
    hasher.update(entry.signature.as_bytes());
    ContentHash::from_bytes(hasher.finalize().into())
}

/// A page of the settlement log, oldest entry first.
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
            Posted::Batch(batch) => self.take_batch(batch)?,
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
        if receipts.is_empty() && changes.is_empty() {
            return None;
        }
        Some(Batch {
            number: self.batches.len() as u64,
            fingerprint: merkle::list_root(receipts),
            changes,
            through,
        })
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
