//! Credits: who holds them and how a job's fee moves between accounts (spec
//! section 5).
//!
//! Like `Accounts`, the ledger changes only by applying entries, so a log of
//! entries is a complete copy of every balance.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::ops::{Add, AddAssign, Sub};
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::keys::{AccountId, ContentHash, JobId};

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

/// The longest name a payment may be given: the payment credits were bought
/// with, or the payment a cash-out was paid by. It bounds the size of a ledger
/// entry.
pub const MAX_PAYMENT_NAME_BYTES: usize = 200;

/// The part of its stake an account loses when one of its nodes is sent back to
/// probation.
pub const PROBATION_FORFEIT_PERCENT: u64 = 10;

impl Credits {
    pub const ZERO: Self = Self(0);

    pub const fn whole(credits: u64) -> Self {
        Self(credits * MICROS_PER_CREDIT)
    }

    /// An amount given in millionths of a credit.
    pub const fn from_micros(micros: u64) -> Self {
        Self(micros)
    }

    /// The amount in millionths of a credit.
    pub const fn micros(self) -> u64 {
        self.0
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
    /// Credits the account earned or bought. The spec sets granted credits
    /// apart and no others, so the two are counted together.
    pub earned: Credits,
    /// Earned credits put up as a guarantee for the account's nodes. They cannot
    /// be spent, and are lost if a node misbehaves.
    pub staked: Credits,
    /// Earned credits set aside for cash-outs the account has asked for, until
    /// each is paid out or refused. They cannot be spent meanwhile.
    #[serde(default)]
    pub cashing_out: Credits,
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
            ..Balance::default()
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
    /// Sets aside the whole fee for a data service, out of the balance of the
    /// account that asked for it. `id` is a stored blob's root or a relay
    /// channel's name.
    DataEscrow {
        id: ContentHash,
        account: AccountId,
        amount: Credits,
    },
    /// Pays a node for part of a data service (a stretch of holding a blob, or
    /// bytes relayed) out of what was set aside. The amount is divided like a job
    /// fee: most to the node, the rest to the verification fund and the treasury.
    DataPay {
        id: ContentHash,
        account: AccountId,
        amount: Credits,
    },
    /// Returns what is left of a data service's fee to the account that paid.
    DataRefund { id: ContentHash },
    /// Moves earned credits into the account's stake.
    Stake { account: AccountId, amount: Credits },
    /// Moves stake back into earned credits.
    Unstake { account: AccountId, amount: Credits },
    /// Takes stake from an account whose node misbehaved, for the treasury. Takes
    /// what there is if the stake is smaller.
    Forfeit { account: AccountId, amount: Credits },
    /// Credits an account with credits it bought (spec section 5: "Credits
    /// can be bought and cashed out for money"). `payment` names the payment
    /// they were bought with, and one payment is credited once. The credits
    /// are new: they come from no other balance.
    Buy {
        account: AccountId,
        amount: Credits,
        payment: String,
    },
    /// Sets earned credits aside for a cash-out the account asked for. Granted
    /// credits are never taken ("Granted credits can be spent but not
    /// transferred or cashed out"). `id` names the cash-out.
    CashOut {
        id: ContentHash,
        account: AccountId,
        amount: Credits,
    },
    /// Closes a cash-out that was paid: the credits set aside for it are gone
    /// for good. `payout` names the payment the account was paid by.
    CashOutPaid { id: ContentHash, payout: String },
    /// Closes a cash-out that was refused: its credits go back to the account.
    CashOutRefused { id: ContentHash },
}

#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("the account's balance does not cover the fee that must be set aside")]
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
    #[error("only earned credits can be cashed out, and the account has too few")]
    TooFewToCashOut,
    #[error("an amount of credits to buy or cash out must be more than nothing")]
    NoAmount,
    #[error("this payment has already been credited")]
    PaymentCredited,
    #[error("the name of a payment is empty or too long")]
    BadPaymentName,
    #[error("this cash-out has already been asked for")]
    CashOutExists,
    #[error("no such cash-out is waiting")]
    UnknownCashOut,
}

struct Escrow {
    account: AccountId,
    held: Balance,
}

/// A cash-out that an account asked for and that is neither paid nor refused.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct CashOut {
    pub id: ContentHash,
    pub account: AccountId,
    pub amount: Credits,
}

/// Who holds credits: an account, or one of the protocol's two pools.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Holder {
    Account(AccountId),
    /// Pays spot checkers and known-answer jobs (spec section 5, "Fee split").
    VerificationFund,
    Treasury,
}

/// Every balance, plus the credits set aside for jobs that are still open.
#[derive(Default)]
pub struct Ledger {
    balances: HashMap<AccountId, Balance>,
    escrows: HashMap<JobId, Escrow>,
    /// Credits set aside for data services: by root for a stored blob, by name
    /// for a relay channel.
    data_escrows: HashMap<ContentHash, Escrow>,
    /// Cash-outs that wait to be paid or refused, by their names.
    cash_outs: BTreeMap<ContentHash, CashOut>,
    /// The names of the payments credits were bought with, so that none is
    /// credited twice.
    payments: HashSet<String>,
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

    /// What every holder has, as settlement counts it: an account's credits and
    /// any stake (the spec's account record has "`balance`: Credits and any
    /// stake"), together with what is set aside for its open jobs, its data
    /// services, and its cash-outs, which stays the account's until it is paid
    /// out.
    pub fn holdings(&self) -> BTreeMap<Holder, Credits> {
        let mut holdings = BTreeMap::new();
        for (account, balance) in &self.balances {
            let all = balance.spendable() + balance.staked + balance.cashing_out;
            holdings.insert(Holder::Account(*account), all);
        }
        for escrow in self.escrows.values().chain(self.data_escrows.values()) {
            *holdings.entry(Holder::Account(escrow.account)).or_default() +=
                escrow.held.spendable();
        }
        holdings.insert(Holder::VerificationFund, self.verification_fund);
        holdings.insert(Holder::Treasury, self.treasury);
        holdings
    }

    /// The cash-outs that wait to be paid or refused.
    pub fn cash_outs(&self) -> Vec<CashOut> {
        self.cash_outs.values().copied().collect()
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
            LedgerEntry::DataEscrow {
                id,
                account,
                amount,
            } => {
                if self.data_escrows.contains_key(id) {
                    return Err(LedgerError::AlreadyEscrowed);
                }
                let balance = self.account(account)?;
                if balance.spendable() < *amount {
                    return Err(LedgerError::InsufficientCredits);
                }
                let held = balance.take(*amount);
                let escrow = Escrow {
                    account: *account,
                    held,
                };
                self.data_escrows.insert(*id, escrow);
            }
            LedgerEntry::DataPay {
                id,
                account,
                amount,
            } => {
                let escrow = self
                    .data_escrows
                    .get_mut(id)
                    .ok_or(LedgerError::NothingEscrowed)?;
                if escrow.held.spendable() < *amount {
                    return Err(LedgerError::FeeExceedsEscrow);
                }
                escrow.held.take(*amount);
                let to_node = amount.percent(NODE_SHARE_PERCENT);
                let to_fund = amount.percent(VERIFICATION_SHARE_PERCENT);
                self.balances.entry(*account).or_default().earned += to_node;
                self.verification_fund += to_fund;
                self.treasury += *amount - to_node - to_fund;
            }
            LedgerEntry::DataRefund { id } => {
                let Escrow { account, held } = self
                    .data_escrows
                    .remove(id)
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
            LedgerEntry::Buy {
                account,
                amount,
                payment,
            } => {
                if *amount == Credits::ZERO {
                    return Err(LedgerError::NoAmount);
                }
                check_payment_name(payment)?;
                if self.payments.contains(payment) {
                    return Err(LedgerError::PaymentCredited);
                }
                self.account(account)?.earned += *amount;
                self.payments.insert(payment.clone());
            }
            LedgerEntry::CashOut {
                id,
                account,
                amount,
            } => {
                if *amount == Credits::ZERO {
                    return Err(LedgerError::NoAmount);
                }
                if self.cash_outs.contains_key(id) {
                    return Err(LedgerError::CashOutExists);
                }
                let balance = self.account(account)?;
                if balance.earned < *amount {
                    return Err(LedgerError::TooFewToCashOut);
                }
                balance.earned = balance.earned - *amount;
                balance.cashing_out += *amount;
                let cash_out = CashOut {
                    id: *id,
                    account: *account,
                    amount: *amount,
                };
                self.cash_outs.insert(*id, cash_out);
            }
            LedgerEntry::CashOutPaid { id, payout } => {
                check_payment_name(payout)?;
                let CashOut {
                    account, amount, ..
                } = self.close_cash_out(id)?;
                let balance = self.balances.entry(account).or_default();
                balance.cashing_out = balance.cashing_out - amount;
            }
            LedgerEntry::CashOutRefused { id } => {
                let CashOut {
                    account, amount, ..
                } = self.close_cash_out(id)?;
                let balance = self.balances.entry(account).or_default();
                balance.cashing_out = balance.cashing_out - amount;
                balance.earned += amount;
            }
        }
        Ok(())
    }

    fn close_cash_out(&mut self, id: &ContentHash) -> Result<CashOut, LedgerError> {
        self.cash_outs.remove(id).ok_or(LedgerError::UnknownCashOut)
    }

    fn account(&mut self, account: &AccountId) -> Result<&mut Balance, LedgerError> {
        self.balances
            .get_mut(account)
            .ok_or(LedgerError::UnknownAccount)
    }
}

fn check_payment_name(name: &str) -> Result<(), LedgerError> {
    if name.is_empty() || name.len() > MAX_PAYMENT_NAME_BYTES {
        return Err(LedgerError::BadPaymentName);
    }
    Ok(())
}
