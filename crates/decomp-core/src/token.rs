//! The token: a coin that can be traded, and that is destroyed to make credits
//! (spec section 5, "Funding without dollars": "Stable credits plus a floating
//! token ... tokens are destroyed to create credits").
//!
//! Tokens are held in the ledger beside credits, where moving them between
//! accounts is one more ledger entry, free and immediate. Exchanges and
//! wallets trade a copy of the token on an ICRC ledger (the Internet
//! Computer's token standards ICRC-1, ICRC-2, and ICRC-3), of which the control
//! plane is the only minter. Withdrawing tokens mints copies to an ICRC
//! account; depositing copies burns them and gives the tokens back. Every copy
//! is backed by a token in the ledger's bridge pool, so the ICRC ledger's
//! total supply is always what the bridge pool holds.

use std::fmt;
use std::ops::{Add, AddAssign, Sub};
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::amount;
use crate::keys::{AccountId, ContentHash, Subaccount};
use crate::ledger::Credits;

/// The decimal places of a token, as on the ICRC ledger: amounts are held in
/// hundred-millionths.
pub const TOKEN_DECIMALS: u8 = 8;
const E8S_PER_TOKEN: u64 = 10u64.pow(TOKEN_DECIMALS as u32);

/// Every token there will be, created in the treasury when the ledger starts.
/// Tokens are destroyed by burning them into credits, and never created after.
pub const TOKEN_SUPPLY: Tokens = Tokens::whole(1_000_000_000);

/// What burning one token makes, in millionths of a credit (starting value:
/// one credit). Credits keep app prices stable while the token's price
/// floats above what it burns into.
pub const CREDIT_MICROS_PER_TOKEN: u64 = 1_000_000;

/// The token's name and symbol on the ICRC ledger.
pub const TOKEN_NAME: &str = "DeComp";
pub const TOKEN_SYMBOL: &str = "DCMP";

/// What one transfer on the ICRC ledger costs, the Internet Computer's usual
/// fee. A deposit must be more than this, since burning copies costs as much
/// as moving them.
pub const ICRC_TRANSFER_FEE: Tokens = Tokens(10_000);

/// The longest principal in text form: 29 bytes as base32 in groups of five
/// with dashes.
const MAX_PRINCIPAL_TEXT: usize = 63;

/// An amount of tokens, held as hundred-millionths of a token. On the wire it
/// is that whole number.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Tokens(u64);

impl Tokens {
    pub const ZERO: Self = Self(0);

    pub const fn whole(tokens: u64) -> Self {
        Self(tokens * E8S_PER_TOKEN)
    }

    /// An amount in hundred-millionths of a token, as the ICRC ledger counts.
    pub const fn from_e8s(e8s: u64) -> Self {
        Self(e8s)
    }

    pub const fn e8s(self) -> u64 {
        self.0
    }

    /// The credits burning this many tokens makes.
    pub fn credits(self) -> Credits {
        let micros =
            u128::from(self.0) * u128::from(CREDIT_MICROS_PER_TOKEN) / u128::from(E8S_PER_TOKEN);
        Credits::from_micros(micros as u64)
    }
}

impl Add for Tokens {
    type Output = Self;
    fn add(self, other: Self) -> Self {
        Self(self.0 + other.0)
    }
}

impl AddAssign for Tokens {
    fn add_assign(&mut self, other: Self) {
        self.0 += other.0;
    }
}

impl Sub for Tokens {
    type Output = Self;
    fn sub(self, other: Self) -> Self {
        Self(self.0 - other.0)
    }
}

impl fmt::Display for Tokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&amount::format(self.0, TOKEN_DECIMALS.into()))
    }
}

/// Reads an amount as people write it, such as `12` or `12.5`.
impl FromStr for Tokens {
    type Err = &'static str;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        amount::parse(text, TOKEN_DECIMALS.into())
            .map(Self)
            .ok_or("an amount of tokens looks like 12 or 12.5")
    }
}

/// An account on the ICRC ledger: an owner's principal, in text form, and a
/// subaccount, all zeros when it is left out.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IcrcAccount {
    pub owner: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subaccount: Option<Subaccount>,
}

impl IcrcAccount {
    /// Whether the owner looks like a principal in text form. The canister
    /// that mints checks it fully.
    pub fn is_well_formed(&self) -> bool {
        !self.owner.is_empty()
            && self.owner.len() <= MAX_PRINCIPAL_TEXT
            && self
                .owner
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    }
}

/// The subaccount of the control plane's ICRC account that `account` deposits
/// copies to. It is made with a secret only the control plane holds, so the
/// ICRC ledger, which is public, does not show which account a deposit was for
/// (spec section 6: "No public balance under a user's identity").
pub fn deposit_subaccount(secret: &[u8; 32], account: &AccountId) -> Subaccount {
    let mut hasher = Sha256::new();
    hasher.update(b"decomp/token-deposit/v1\n");
    hasher.update(secret);
    hasher.update(account.as_bytes());
    Subaccount::from_bytes(hasher.finalize().into())
}

/// Tokens an account asked to withdraw as copies on the ICRC ledger, waiting
/// for the mint.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Withdrawal {
    pub id: ContentHash,
    pub account: AccountId,
    pub amount: Tokens,
    pub to: IcrcAccount,
    /// When it was asked for, in seconds since 1970. The mint carries it, so
    /// the ICRC ledger turns away a second mint of the same withdrawal.
    pub at: u64,
}
