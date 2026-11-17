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
use crate::controller::{self, Approval, Controller, MAX_WAITING_RECOVERIES, RECOVERY_WAIT_SECS};
use crate::envelope::{Envelope, EnvelopeError};
use crate::keys::{AccountId, LedgerAddress, PublicKey};
use crate::request::MAX_CLOCK_SKEW_SECS;

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
    /// Replaces the account's controllers and how many of them must agree to
    /// a recovery (see `controller`). Signed by the account key and by nothing
    /// else: the spec has the account key sign "changing controllers".
    SetControllers {
        account_id: AccountId,
        seq: u64,
        controllers: Vec<Controller>,
        methods_required: u32,
    },
    /// Starts a recovery: the account's controllers approve the device whose
    /// key signed this. `approvals` are their signatures over
    /// `controller::statement`. The device joins the account when it sends
    /// `CompleteRecovery` after the waiting period, unless a device of the
    /// account cancels first. `at` is the time now, in seconds since 1970; a
    /// coordinator refuses one far from its clock when it first takes the
    /// request in.
    RecoverDevice {
        account_id: AccountId,
        at: u64,
        approvals: Vec<Approval>,
    },
    /// Finishes the recovery of the device whose key signed this, once its
    /// waiting period is over. `at` is the time now, checked as in
    /// `RecoverDevice`, so it can run ahead of the waiting period by no more
    /// than a clock may be off.
    CompleteRecovery { account_id: AccountId, at: u64 },
    /// Cancels a recovery that is waiting. Signed by the account key or by an
    /// approved device. The device's key is never approved after this.
    CancelRecovery {
        account_id: AccountId,
        seq: u64,
        device_key: PublicKey,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Account {
    pub id: AccountId,
    pub account_key: PublicKey,
    /// Where the account's balance sits on the ledger (see `LedgerAddress`).
    /// Only the account's own devices are shown it.
    pub ledger_address: LedgerAddress,
    /// How many changes the account has had. The next change must carry this value,
    /// which orders changes and stops a captured one from being replayed.
    pub seq: u64,
    pub devices: Vec<Device>,
    /// The ways the user added to sign in and to recover the account, besides
    /// the account key (spec section 1, `controllers`: "Public keys allowed
    /// to approve or revoke devices, and how many must agree").
    pub controllers: Vec<Controller>,
    /// How many of `controllers` must approve a device for its recovery to
    /// start: one, or two ("an account can require two methods to agree").
    pub methods_required: u32,
    /// Recoveries that are waiting to take effect.
    pub recoveries: Vec<Recovery>,
    /// The app this account has published, if any. The account's ID is the app's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<AppManifest>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Device {
    pub key: PublicKey,
    pub revoked: bool,
}

/// A device that the account's controllers approved and that has not joined
/// the account yet.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Recovery {
    pub device_key: PublicKey,
    /// When the device may join, in seconds since 1970.
    pub effective_at: u64,
}

impl Account {
    /// Whether `key` is a device waiting for its recovery to take effect.
    pub fn is_recovering(&self, key: &PublicKey) -> bool {
        self.recoveries.iter().any(|r| r.device_key == *key)
    }

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
    #[error("only the account key may change the account's controllers")]
    NotAccountKey,
    #[error("{0}")]
    BadControllers(&'static str),
    #[error("too few of the account's controllers approved this device")]
    NotApproved,
    #[error("the account already has as many recoveries waiting as it may")]
    TooManyRecoveries,
    #[error("no recovery of this device is waiting")]
    NoSuchRecovery,
    #[error("the recovery takes effect at {effective_at}, in seconds since 1970, and not before")]
    RecoveryWaiting { effective_at: u64 },
    #[error("the request's time is too far from the coordinator's clock")]
    WrongTime,
}

/// Every account record, changed only through [`Accounts::apply`].
pub struct Accounts {
    by_id: HashMap<AccountId, Account>,
    /// A device key belongs to one account for good: approved, revoked, or
    /// waiting on a recovery, even one that was cancelled.
    by_device: HashMap<PublicKey, AccountId>,
    /// How long a recovery waits before it can be completed, in seconds.
    recovery_wait: u64,
}

impl Default for Accounts {
    fn default() -> Self {
        Self::waiting(RECOVERY_WAIT_SECS)
    }
}

impl Accounts {
    /// Records in which a recovery waits `recovery_wait` seconds. Everyone
    /// who replays the same operations must use the same wait.
    pub fn waiting(recovery_wait: u64) -> Self {
        Self {
            by_id: HashMap::new(),
            by_device: HashMap::new(),
            recovery_wait,
        }
    }

    /// Verifies and applies one operation that was accepted before, returning
    /// the changed account. On any error the records are left as they were.
    pub fn apply(&mut self, envelope: &Envelope) -> Result<&Account, OpError> {
        self.apply_checked(envelope, None)
    }

    /// Verifies and applies an operation that has just arrived, at time
    /// `now`. Unlike `apply`, it refuses one whose own time is far from `now`.
    pub fn apply_at(&mut self, envelope: &Envelope, now: u64) -> Result<&Account, OpError> {
        self.apply_checked(envelope, Some(now))
    }

    fn apply_checked(
        &mut self,
        envelope: &Envelope,
        now: Option<u64>,
    ) -> Result<&Account, OpError> {
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
                    ledger_address: LedgerAddress::of(&envelope.signature),
                    seq: 0,
                    devices: vec![Device {
                        key: device_key,
                        revoked: false,
                    }],
                    controllers: Vec::new(),
                    methods_required: 1,
                    recoveries: Vec::new(),
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
            AccountOp::SetControllers {
                account_id,
                seq,
                controllers,
                methods_required,
            } => {
                let account = authorized(&mut self.by_id, account_id, seq, &signer)?;
                if signer != account.account_key {
                    return Err(OpError::NotAccountKey);
                }
                let problem = controller::problem(&controllers, methods_required, &account_id);
                if let Some(problem) = problem {
                    return Err(OpError::BadControllers(problem));
                }
                account.controllers = controllers;
                account.methods_required = methods_required;
                account.seq += 1;
                Ok(account)
            }
            AccountOp::RecoverDevice {
                account_id,
                at,
                approvals,
            } => {
                on_time(at, now)?;
                let account = self.by_id.get(&account_id).ok_or(OpError::UnknownAccount)?;
                let statement = controller::statement(&account_id, &signer);
                let acts_for = |key: &PublicKey| {
                    let account = self.account_of(key)?;
                    account.authorizes(key).then_some(account.id)
                };
                let agreed =
                    controller::agreeing(&account.controllers, &approvals, &statement, acts_for);
                if agreed < account.methods_required as usize {
                    return Err(OpError::NotApproved);
                }
                if account.recoveries.len() >= MAX_WAITING_RECOVERIES {
                    return Err(OpError::TooManyRecoveries);
                }
                claim_device(&mut self.by_device, signer, account_id)?;
                let account = self.by_id.get_mut(&account_id).expect("found above");
                account.recoveries.push(Recovery {
                    device_key: signer,
                    effective_at: at.saturating_add(self.recovery_wait),
                });
                account.seq += 1;
                Ok(account)
            }
            AccountOp::CompleteRecovery { account_id, at } => {
                on_time(at, now)?;
                let account = self
                    .by_id
                    .get_mut(&account_id)
                    .ok_or(OpError::UnknownAccount)?;
                let place = account
                    .recoveries
                    .iter()
                    .position(|recovery| recovery.device_key == signer)
                    .ok_or(OpError::NoSuchRecovery)?;
                let effective_at = account.recoveries[place].effective_at;
                if at < effective_at {
                    return Err(OpError::RecoveryWaiting { effective_at });
                }
                account.recoveries.remove(place);
                account.devices.push(Device {
                    key: signer,
                    revoked: false,
                });
                account.seq += 1;
                Ok(account)
            }
            AccountOp::CancelRecovery {
                account_id,
                seq,
                device_key,
            } => {
                let account = authorized(&mut self.by_id, account_id, seq, &signer)?;
                let place = account
                    .recoveries
                    .iter()
                    .position(|recovery| recovery.device_key == device_key)
                    .ok_or(OpError::NoSuchRecovery)?;
                account.recoveries.remove(place);
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

/// Refuses an operation that has just arrived if its own time, `at`, is far
/// from `now`. An operation read back from a log has no `now` and passes.
fn on_time(at: u64, now: Option<u64>) -> Result<(), OpError> {
    match now {
        Some(now) if now.abs_diff(at) > MAX_CLOCK_SKEW_SECS => Err(OpError::WrongTime),
        _ => Ok(()),
    }
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
