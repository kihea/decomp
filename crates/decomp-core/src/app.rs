//! Applications and the jobs they pay for (spec section 1, "Application
//! identity", and section 5, "Who pays").
//!
//! An application is published by its developer's account. Its app ID is that
//! account's ID, which is the hash of the developer's signing key, and its credit
//! pool is that account's balance. So an app needs no records or credits of its
//! own kind: publishing one is a change to an account, like any other.
//!
//! A job is charged to the account that submits it unless its manifest names an
//! app as sponsor. The app then pays, provided the job runs one of the app's own
//! modules and the submitter is within what the app allows one account in a day.
//!
//! An app can also require its users to contribute compute (spec section 5,
//! "Contribution required for use"): a job that the app pays for, or that is
//! handed the app's state, runs only if the submitting account earned, over
//! the last `CONTRIBUTION_WINDOW_SECS`, at least a set share of what it spent,
//! counting the job. Jobs whose every model call is to a model the app exempts
//! are not held to it.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::keys::{AccountId, ContentHash};
use crate::ledger::Credits;

/// The longest name an app may have, in bytes.
pub const MAX_APP_NAME_BYTES: usize = 64;

/// The most modules an app may list.
pub const MAX_APP_MODULES: usize = 64;

const SECS_PER_DAY: u64 = 24 * 3600;

/// How far back an account's earning and spending are counted against an
/// app's contribution rule: the spec's "over the last 30 days".
pub const CONTRIBUTION_WINDOW_SECS: u64 = 30 * SECS_PER_DAY;

/// The most models an app may exempt from its contribution rule.
pub const MAX_EXEMPT_MODELS: usize = 64;

/// What an app is made of, as registered with the control plane (spec section
/// 7, "App manifest": modules, schemas, permissions, and sponsorship policy).
/// Module schemas travel inside each module (see `decomp-schema`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppManifest {
    pub name: String,
    /// The app's job modules, by hash. Each describes its own input and output
    /// types (see `decomp-schema`).
    pub modules: Vec<ContentHash>,
    /// Which jobs the app pays for. Left out, it pays for none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sponsorship: Option<Sponsorship>,
    /// The hash of the app's client files, which a gateway serves at the app's
    /// name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<ContentHash>,
    /// The hash of a blob of metadata the application layer defines (for
    /// instance, how the app presents itself in a launcher). The protocol
    /// stores the hash and never reads the blob.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<ContentHash>,
    /// How much compute the app requires its users to contribute. Left out,
    /// it requires none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contribution: Option<Contribution>,
}

/// An app's rule that its users contribute compute.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Contribution {
    /// The share of what an account spent, in percent, that it must have
    /// earned over the window: 100 asks a user to earn all they spend.
    pub percent: u32,
    /// Approved models, by ID, that jobs may call without the rule applying.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exempt_models: Vec<ContentHash>,
}

impl Contribution {
    /// Whether a job calling `models` is held to the rule: any job that calls
    /// no model, or calls one the app does not exempt.
    pub fn covers(&self, models: &[ContentHash]) -> bool {
        models.is_empty()
            || models
                .iter()
                .any(|model| !self.exempt_models.contains(model))
    }

    /// Whether an account that earned `earned` and spent `spent` over the
    /// window may set `amount` aside for one more job.
    pub fn allows(&self, earned: Credits, spent: Credits, amount: Credits) -> bool {
        let owed = (spent + amount).micros() as u128 * u128::from(self.percent);
        earned.micros() as u128 * 100 >= owed
    }
}

/// An app's rule for paying for its users' jobs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sponsorship {
    /// The most the app sets aside in one day for the jobs of one account. This
    /// counts the highest possible fee of each job, as set aside when it is
    /// submitted, not what the job turns out to cost.
    pub per_account_daily: Credits,
}

impl AppManifest {
    /// Why this manifest cannot be published, if it cannot.
    pub fn problem(&self) -> Option<&'static str> {
        if self.name.trim().is_empty() || self.name.len() > MAX_APP_NAME_BYTES {
            return Some("an app's name is between 1 and 64 bytes");
        }
        if self.modules.len() > MAX_APP_MODULES {
            return Some("an app lists at most 64 modules");
        }
        if let Some(rule) = &self.contribution {
            if rule.percent == 0 {
                return Some("a contribution rule asks for more than 0 percent");
            }
            if rule.exempt_models.len() > MAX_EXEMPT_MODELS {
                return Some("an app exempts at most 64 models");
            }
        }
        None
    }
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum SponsorError {
    #[error("no app is published under that ID")]
    UnknownApp,
    #[error("the app does not pay for jobs")]
    NotSponsoring,
    #[error("the app pays only for jobs that run its own modules")]
    NotItsModule,
    #[error("the app has set aside all it allows this account today")]
    OverDailyCap,
}

/// What each app has set aside today for each account's jobs. Kept in memory
/// only: it starts again from nothing each day anyway.
#[derive(Default)]
pub struct Sponsored {
    /// Days since 1970 that `set_aside` is for.
    day: u64,
    /// By app, then by the account whose jobs it paid for.
    set_aside: HashMap<(AccountId, AccountId), Credits>,
}

impl Sponsored {
    /// Checks that the app with this manifest pays for a job of `account`'s that
    /// runs `module` and has `amount` set aside for it.
    pub fn check(
        &self,
        app: (AccountId, Option<&AppManifest>),
        account: AccountId,
        module: &ContentHash,
        amount: Credits,
        now: u64,
    ) -> Result<(), SponsorError> {
        let (app, manifest) = app;
        let manifest = manifest.ok_or(SponsorError::UnknownApp)?;
        let rule = manifest.sponsorship.ok_or(SponsorError::NotSponsoring)?;
        if !manifest.modules.contains(module) {
            return Err(SponsorError::NotItsModule);
        }
        let today = now / SECS_PER_DAY == self.day;
        let so_far = self.set_aside.get(&(app, account)).filter(|_| today);
        if so_far.copied().unwrap_or(Credits::ZERO) + amount > rule.per_account_daily {
            return Err(SponsorError::OverDailyCap);
        }
        Ok(())
    }

    /// Counts `amount` as set aside by `app` for `account`'s jobs.
    pub fn count(&mut self, app: AccountId, account: AccountId, amount: Credits, now: u64) {
        let day = now / SECS_PER_DAY;
        if day != self.day {
            self.day = day;
            self.set_aside.clear();
        }
        *self
            .set_aside
            .entry((app, account))
            .or_insert(Credits::ZERO) += amount;
    }
}
