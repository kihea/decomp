//! Credits: who holds them and how a job's fee moves between accounts (spec
//! section 5).
//!
//! Like `Accounts`, the ledger changes only by applying entries, so a log of
//! entries is a complete copy of every balance.

use std::collections::HashMap;
use std::fmt;
use std::ops::{Add, AddAssign, Sub};
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::keys::{AccountId, JobId};

/// An amount of credits, held as millionths of a credit so that fee shares are
/// exact. On the wire it is that whole number.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Credits(u64);

const MICROS_PER_CREDIT: u64 = 1_000_000;

/// What a new account is given so its owner can use apps at once.
pub const STARTER_GRANT: Credits = Credits::whole(25);

/// The price of one compute unit on one node. The spec lets this float with
/// network load; until that exists it is fixed.
pub const BASE_PRICE: Credits = Credits::whole(1);

/// The part of a fee that goes to the nodes whose results matched, split equally.
const NODE_SHARE_PERCENT: u64 = 85;
/// The part that funds verification. The protocol treasury receives the rest.
const VERIFICATION_SHARE_PERCENT: u64 = 10;

/// The part of its stake an account loses when one of its nodes is sent back to
/// probation.
pub const PROBATION_FORFEIT_PERCENT: u64 = 10;

impl Credits {
    pub const ZERO: Self = Self(0);

    pub const fn whole(credits: u64) -> Self {
        Self(credits * MICROS_PER_CREDIT)
    }

    pub fn percent(self, percent: u64) -> Self {
        Self(self.0 / 100 * percent + self.0 % 100 * percent / 100)
    }
}

impl Add for Credits {
    type Output = Self;
    fn add(self, other: Self) -> Self {
        Self(self.0 + other.0)
    }
}

impl AddAssign for Credits {
    fn add_assign(&mut self, other: Self) {
        self.0 += other.0;
    }
}

impl Sub for Credits {
    type Output = Self;
    fn sub(self, other: Self) -> Self {
        Self(self.0 - other.0)
    }
}

impl fmt::Display for Credits {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (whole, micros) = (self.0 / MICROS_PER_CREDIT, self.0 % MICROS_PER_CREDIT);
        write!(f, "{whole}.{:02}", micros / (MICROS_PER_CREDIT / 100))
    }
}

/// Reads an amount as people write it, such as `12` or `12.5`.
impl FromStr for Credits {
    type Err = &'static str;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        const INVALID: &str = "an amount of credits looks like 12 or 12.5";
        let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
        if fraction.len() > 6 || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(INVALID);
        }
        let whole: u64 = whole.parse().map_err(|_| INVALID)?;
        let micros: u64 = format!("{fraction:0<6}").parse().map_err(|_| INVALID)?;
        whole
            .checked_mul(MICROS_PER_CREDIT)
            .and_then(|whole| whole.checked_add(micros))
            .map(Self)
            .ok_or(INVALID)
    }
}

/// What `units` of compute cost on each of `nodes` nodes. With a job's compute
/// cap for `units`, this is the most the job can cost.
pub fn fee(units: u64, nodes: u32) -> Credits {
    Credits(
        units
            .saturating_mul(BASE_PRICE.0)
            .saturating_mul(u64::from(nodes)),
    )
}

/// How one job's fee is divided (spec section 5, "Fee split").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeeSplit {
    /// What the job's submitter pays.
    pub total: Credits,
    /// What each paid node's account receives.
    pub per_node: Credits,
    pub verification_fund: Credits,
    pub treasury: Credits,
}

impl FeeSplit {
    /// The split for a job where each of `nodes` nodes used `units` compute units.
    /// The treasury takes whatever rounding leaves, so the parts always add up.
    pub fn of(units: u64, nodes: u32) -> Self {
        let total = fee(units, nodes);
        let per_node = Credits(total.percent(NODE_SHARE_PERCENT).0 / u64::from(nodes.max(1)));
        let verification_fund = total.percent(VERIFICATION_SHARE_PERCENT);
        let paid_to_nodes = Credits(per_node.0 * u64::from(nodes));
        let treasury = total - paid_to_nodes - verification_fund;
        Self {
            total,
            per_node,
            verification_fund,
            treasury,
        }
    }
}

/// Adds splits up part by part, for a job made of several pieces.
impl std::iter::Sum for FeeSplit {
    fn sum<I: Iterator<Item = Self>>(splits: I) -> Self {
        let nothing = FeeSplit::of(0, 0);
        splits.fold(nothing, |sum, split| FeeSplit {
            total: sum.total + split.total,
            per_node: sum.per_node + split.per_node,
            verification_fund: sum.verification_fund + split.verification_fund,
            treasury: sum.treasury + split.treasury,
        })
    }
}

/// An account's credits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Balance {
    /// Starter credits. They can be spent but never staked, transferred, or cashed
    /// out, which limits what farming new accounts can earn.
    pub granted: Credits,
    pub earned: Credits,
    /// Earned credits put up as a guarantee for the account's nodes. They cannot
    /// be spent, and are lost if a node misbehaves.
    pub staked: Credits,
}

impl Balance {
    pub fn spendable(&self) -> Credits {
        self.granted + self.earned
    }

    /// Takes `amount` out, granted credits first, and returns what was taken.
    fn take(&mut self, amount: Credits) -> Balance {
        let granted = amount.min(self.granted);
        let taken = Balance {
            granted,
            earned: amount - granted,
            staked: Credits::ZERO,
        };
        self.granted = self.granted - taken.granted;
        self.earned = self.earned - taken.earned;
        taken
    }

    fn put_back(&mut self, returned: Balance) {
        self.granted += returned.granted;
        self.earned += returned.earned;
    }
}

/// A change to balances. The starter grant is not an entry because it follows
/// from the account being created.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LedgerEntry {
    /// Sets aside the most a job may cost, out of its submitter's balance.
    Escrow {
        job_id: JobId,
        account: AccountId,
        amount: Credits,
    },
    /// Pays for a verified job out of its escrow and returns what is left.
    Settle {
        job_id: JobId,
        split: FeeSplit,
        node_accounts: Vec<AccountId>,
    },
    /// Returns a job's whole escrow: it expired, was disputed, or was lost.
    Refund { job_id: JobId },
    /// Pays an account from the verification fund, for checking work: a spot
    /// check or a job with a known answer. Pays less if the fund is short.
    PayChecker { account: AccountId, amount: Credits },
    /// Moves earned credits into the account's stake.
    Stake { account: AccountId, amount: Credits },
    /// Moves stake back into earned credits.
    Unstake { account: AccountId, amount: Credits },
    /// Takes stake from an account whose node misbehaved, for the treasury. Takes
    /// what there is if the stake is smaller.
    Forfeit { account: AccountId, amount: Credits },
}

#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("the account's balance does not cover the job's highest possible fee")]
    InsufficientCredits,
    #[error("only earned credits can be staked, and the account has too few")]
    InsufficientEarnedCredits,
    #[error("the account has less than that staked")]
    InsufficientStake,
    #[error("no such account")]
    UnknownAccount,
    #[error("the job already has credits set aside")]
    AlreadyEscrowed,
    #[error("the job has no credits set aside")]
    NothingEscrowed,
    #[error("the fee is more than was set aside for the job")]
    FeeExceedsEscrow,
}

struct Escrow {
    account: AccountId,
    held: Balance,
}

/// Every balance, plus the credits set aside for jobs that are still open.
#[derive(Default)]
pub struct Ledger {
    balances: HashMap<AccountId, Balance>,
    escrows: HashMap<JobId, Escrow>,
    verification_fund: Credits,
    treasury: Credits,
}

impl Ledger {
    /// Gives a new account its starter grant. Does nothing for an account the
    /// ledger already knows, so it is safe to call after every account change.
    pub fn open(&mut self, account: AccountId) {
        let starter = Balance {
            granted: STARTER_GRANT,
            ..Balance::default()
        };
        self.balances.entry(account).or_insert(starter);
    }

    pub fn balance(&self, account: &AccountId) -> Balance {
        self.balances.get(account).copied().unwrap_or_default()
    }

    /// Jobs with credits still set aside.
    pub fn escrowed_jobs(&self) -> Vec<JobId> {
        self.escrows.keys().copied().collect()
    }

    /// Applies one entry. On any error the ledger is left as it was.
    pub fn apply(&mut self, entry: &LedgerEntry) -> Result<(), LedgerError> {
        match entry {
            LedgerEntry::Escrow {
                job_id,
                account,
                amount,
            } => {
                if self.escrows.contains_key(job_id) {
                    return Err(LedgerError::AlreadyEscrowed);
                }
                let balance = self.account(account)?;
                if balance.spendable() < *amount {
                    return Err(LedgerError::InsufficientCredits);
                }
                let held = balance.take(*amount);
                self.escrows.insert(
                    *job_id,
                    Escrow {
                        account: *account,
                        held,
                    },
                );
            }
            LedgerEntry::Settle {
                job_id,
                split,
                node_accounts,
            } => {
                let escrow = self
                    .escrows
                    .get(job_id)
                    .ok_or(LedgerError::NothingEscrowed)?;
                if escrow.held.spendable() < split.total {
                    return Err(LedgerError::FeeExceedsEscrow);
                }
                let Escrow { account, mut held } =
                    self.escrows.remove(job_id).expect("checked above");
                held.take(split.total);
                self.balances.entry(account).or_default().put_back(held);
                for node_account in node_accounts {
                    self.balances.entry(*node_account).or_default().earned += split.per_node;
                }
                self.verification_fund += split.verification_fund;
                self.treasury += split.treasury;
            }
            LedgerEntry::Refund { job_id } => {
                let Escrow { account, held } = self
                    .escrows
                    .remove(job_id)
                    .ok_or(LedgerError::NothingEscrowed)?;
                self.balances.entry(account).or_default().put_back(held);
            }
            LedgerEntry::PayChecker { account, amount } => {
                let paid = (*amount).min(self.verification_fund);
                self.verification_fund = self.verification_fund - paid;
                self.account(account)?.earned += paid;
            }
            LedgerEntry::Stake { account, amount } => {
                let balance = self.account(account)?;
                if balance.earned < *amount {
                    return Err(LedgerError::InsufficientEarnedCredits);
                }
                balance.earned = balance.earned - *amount;
                balance.staked += *amount;
            }
            LedgerEntry::Unstake { account, amount } => {
                let balance = self.account(account)?;
                if balance.staked < *amount {
                    return Err(LedgerError::InsufficientStake);
                }
                balance.staked = balance.staked - *amount;
                balance.earned += *amount;
            }
            LedgerEntry::Forfeit { account, amount } => {
                let balance = self.account(account)?;
                let taken = (*amount).min(balance.staked);
                balance.staked = balance.staked - taken;
                self.treasury += taken;
            }
        }
        Ok(())
    }

    fn account(&mut self, account: &AccountId) -> Result<&mut Balance, LedgerError> {
        self.balances
            .get_mut(account)
            .ok_or(LedgerError::UnknownAccount)
    }
}
