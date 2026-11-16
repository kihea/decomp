//! Account records and the operations that change them (spec section 1,
//! "Accounts and identity").
//!
//! An account is created by its account key and then changed only by signed
//! operations. Applying the same operations in the same order always produces the
//! same records, so a log of operations is a complete copy of the state.

use std::collections::HashMap;
use std::collections::hash_map::Entry;

use serde::{Deserialize, Serialize};

use crate::app::AppManifest;
use crate::envelope::{Envelope, EnvelopeError};
use crate::keys::{AccountId, PublicKey};

/// A signed request to create or change an account.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AccountOp {
    /// Creates the account whose key signed this. `device_key` is its first device.
    CreateAccount { device_key: PublicKey },
    /// Adds a device. Signed by the account key or by an approved device.
    ApproveDevice {
        account_id: AccountId,
        seq: u64,
        device_key: PublicKey,
    },
    /// Revokes a device for good. Signed by the account key or by an approved device.
    RevokeDevice {
        account_id: AccountId,
        seq: u64,
        device_key: PublicKey,
    },
    /// Publishes the account's app, or replaces the one it has published (see
    /// `app`). Signed by the account key or by an approved device.
    PublishApp {
        account_id: AccountId,
        seq: u64,
        app: AppManifest,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Account {
    pub id: AccountId,
    pub account_key: PublicKey,
    /// How many changes the account has had. The next change must carry this value,
    /// which orders changes and stops a captured one from being replayed.
    pub seq: u64,
    pub devices: Vec<Device>,
    /// The app this account has published, if any. The account's ID is the app's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<AppManifest>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Device {
    pub key: PublicKey,
    pub revoked: bool,
}

impl Account {
    /// Whether `key` may act for this account: the account key or an approved device.
    pub fn authorizes(&self, key: &PublicKey) -> bool {
        self.account_key == *key || self.devices.iter().any(|d| d.key == *key && !d.revoked)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OpError {
    #[error(transparent)]
    Envelope(#[from] EnvelopeError),
    #[error("this account already exists")]
    AccountExists,
    #[error("no such account")]
    UnknownAccount,
    #[error("the account has changed; expected seq {expected}")]
    StaleSeq { expected: u64 },
    #[error("the signer may not act for this account")]
    NotAuthorized,
    #[error("this device key is already used by an account")]
    DeviceTaken,
    #[error("this device is not part of the account")]
    UnknownDevice,
    #[error("this device is already revoked")]
    AlreadyRevoked,
    #[error("{0}")]
    BadApp(&'static str),
}

/// Every account record, changed only through [`Accounts::apply`].
#[derive(Default)]
pub struct Accounts {
    by_id: HashMap<AccountId, Account>,
    /// A device key belongs to one account for good, revoked or not.
    by_device: HashMap<PublicKey, AccountId>,
}

impl Accounts {
    /// Verifies and applies one operation, returning the changed account.
    /// On any error the records are left as they were.
    pub fn apply(&mut self, envelope: &Envelope) -> Result<&Account, OpError> {
        let signer = envelope.signer;
        match envelope.open()? {
            AccountOp::CreateAccount { device_key } => {
                let id = AccountId::of(&signer);
                let Entry::Vacant(slot) = self.by_id.entry(id) else {
                    return Err(OpError::AccountExists);
                };
                claim_device(&mut self.by_device, device_key, id)?;
                Ok(slot.insert(Account {
                    id,
                    account_key: signer,
                    seq: 0,
                    devices: vec![Device {
                        key: device_key,
                        revoked: false,
                    }],
                    app: None,
                }))
            }
            AccountOp::ApproveDevice {
                account_id,
                seq,
                device_key,
            } => {
                let account = authorized(&mut self.by_id, account_id, seq, &signer)?;
                claim_device(&mut self.by_device, device_key, account_id)?;
                account.devices.push(Device {
                    key: device_key,
                    revoked: false,
                });
                account.seq += 1;
                Ok(account)
            }
            AccountOp::RevokeDevice {
                account_id,
                seq,
                device_key,
            } => {
                let account = authorized(&mut self.by_id, account_id, seq, &signer)?;
                let device = account
                    .devices
                    .iter_mut()
                    .find(|d| d.key == device_key)
                    .ok_or(OpError::UnknownDevice)?;
                if device.revoked {
                    return Err(OpError::AlreadyRevoked);
                }
                device.revoked = true;
                account.seq += 1;
                Ok(account)
            }
            AccountOp::PublishApp {
                account_id,
                seq,
                app,
            } => {
                let account = authorized(&mut self.by_id, account_id, seq, &signer)?;
                if let Some(problem) = app.problem() {
                    return Err(OpError::BadApp(problem));
                }
                account.app = Some(app);
                account.seq += 1;
                Ok(account)
            }
        }
    }

    pub fn get(&self, id: &AccountId) -> Option<&Account> {
        self.by_id.get(id)
    }

    /// The account `key` belongs to, as its account key or as one of its devices
    /// (revoked devices included).
    pub fn account_of(&self, key: &PublicKey) -> Option<&Account> {
        let id = self
            .by_device
            .get(key)
            .copied()
            .unwrap_or_else(|| AccountId::of(key));
        self.by_id.get(&id)
    }
}

/// The account to change, once the signer and `seq` are checked.
fn authorized<'a>(
    by_id: &'a mut HashMap<AccountId, Account>,
    account_id: AccountId,
    seq: u64,
    signer: &PublicKey,
) -> Result<&'a mut Account, OpError> {
    let account = by_id.get_mut(&account_id).ok_or(OpError::UnknownAccount)?;
    if !account.authorizes(signer) {
        return Err(OpError::NotAuthorized);
    }
    if account.seq != seq {
        return Err(OpError::StaleSeq {
            expected: account.seq,
        });
    }
    Ok(account)
}

fn claim_device(
    by_device: &mut HashMap<PublicKey, AccountId>,
    device_key: PublicKey,
    account_id: AccountId,
) -> Result<(), OpError> {
    match by_device.entry(device_key) {
        Entry::Occupied(_) => Err(OpError::DeviceTaken),
        Entry::Vacant(slot) => {
            slot.insert(account_id);
            Ok(())
        }
    }
}
