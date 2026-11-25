//! The bridge to the token's ICRC ledger (see `decomp_core::token`): mints a
//! copy for every token withdrawn, and burns the copies sent to an account's
//! deposit address and gives the tokens back.
//!
//! The control canister is the ledger's only minting account. A transfer from
//! it is a mint, and a transfer to it is a burn.

use candid::{Nat, Principal};
use decomp_core::{AccountId, ICRC_TRANSFER_FEE, LedgerEntry, Subaccount, Tokens, Withdrawal};
use icrc_ledger_types::icrc1::account::Account;
use icrc_ledger_types::icrc1::transfer::{Memo, TransferArg, TransferError};

/// What one call to the ICRC ledger came to.
enum Minted {
    /// The copies are on the ledger, in this block.
    Block(u64),
    /// The ledger refused, for good.
    Refused,
    /// The ledger could not be reached or asked to wait: try again later.
    Later,
}

const NANOS_PER_SEC: u64 = 1_000_000_000;

fn block_number(block: &Nat) -> Option<u64> {
    u64::try_from(&block.0).ok()
}

/// Sends `transfer` to the ICRC ledger.
async fn transfer(ledger: Principal, transfer: TransferArg) -> Minted {
    let reply = ic_cdk::call::Call::unbounded_wait(ledger, "icrc1_transfer")
        .with_arg(transfer)
        .await;
    let Ok(reply) = reply else {
        return Minted::Later;
    };
    match reply.candid::<Result<Nat, TransferError>>() {
        Ok(Ok(block)) => block_number(&block).map_or(Minted::Refused, Minted::Block),
        // The same withdrawal minted before: its first mint stands.
        Ok(Err(TransferError::Duplicate { duplicate_of })) => {
            block_number(&duplicate_of).map_or(Minted::Refused, Minted::Block)
        }
        Ok(Err(TransferError::TemporarilyUnavailable)) | Err(_) => Minted::Later,
        Ok(Err(_)) => Minted::Refused,
    }
}

/// Mints the copies of one withdrawal, and says what to record. `None`
/// leaves it waiting, to be tried again.
pub async fn mint(ledger: Principal, withdrawal: &Withdrawal) -> Option<LedgerEntry> {
    let id = withdrawal.id;
    let Ok(owner) = Principal::from_text(&withdrawal.to.owner) else {
        return Some(LedgerEntry::WithdrawalFailed { id });
    };
    let to = Account {
        owner,
        subaccount: withdrawal.to.subaccount.map(|sub| *sub.as_bytes()),
    };
    // The withdrawal's name and time make the ledger turn away a second mint
    // of it, should the first one's answer be lost.
    let arg = TransferArg {
        from_subaccount: None,
        to,
        fee: None,
        created_at_time: Some(withdrawal.at * NANOS_PER_SEC),
        memo: Some(Memo::from(id.as_bytes().to_vec())),
        amount: Nat::from(withdrawal.amount.e8s()),
    };
    match transfer(ledger, arg).await {
        Minted::Block(block) => Some(LedgerEntry::TokensWithdrawn { id, block }),
        Minted::Refused => Some(LedgerEntry::WithdrawalFailed { id }),
        Minted::Later => None,
    }
}

/// Burns what sits at an account's deposit address, and says what to record.
pub async fn burn_deposit(
    ledger: Principal,
    account: AccountId,
    deposit: Subaccount,
) -> Option<LedgerEntry> {
    let minter = ic_cdk::api::canister_self();
    let at = Account {
        owner: minter,
        subaccount: Some(*deposit.as_bytes()),
    };
    let balance = ic_cdk::call::Call::unbounded_wait(ledger, "icrc1_balance_of")
        .with_arg(at)
        .await
        .ok()?
        .candid::<Nat>()
        .ok()?;
    let amount = Tokens::from_e8s(u64::try_from(&balance.0).ok()?);
    // The ledger burns no less than one transfer fee.
    if amount < ICRC_TRANSFER_FEE {
        return None;
    }
    let arg = TransferArg {
        from_subaccount: Some(*deposit.as_bytes()),
        to: Account {
            owner: minter,
            subaccount: None,
        },
        fee: None,
        created_at_time: None,
        memo: None,
        amount: balance,
    };
    match transfer(ledger, arg).await {
        Minted::Block(block) => Some(LedgerEntry::TokensDeposited {
            account,
            amount,
            block,
        }),
        Minted::Refused | Minted::Later => None,
    }
}
