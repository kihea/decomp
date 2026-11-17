//! Private jobs: how one is run so that the spec's promise holds, "no node ever
//! sees the data unencrypted" (spec section 7, "Privacy modes" and "How private
//! mode runs a job").
//!
//! A job is private if its module says so in the description it gives of
//! itself, and its manifest must then say the same. The device that wants the
//! job run takes "the first route that fits and reports which one it used":
//!
//! | Order | Route | Taken when |
//! | --- | --- | --- |
//! | 1 | Local | "The job fits the user's device": it is run there, and reaches no node |
//! | 2 | Private totals | "The job only adds, counts, averages, or applies fixed weights": nodes run it on shares (see `shares`) |
//!
//! When no route fits, the job is not run, and the device gets `NoPrivateRoute`.
//! The coordinator applies the same rule to what it is sent, so a private job
//! that reaches it by no route is refused before any node is handed it.

use decomp_schema::Signature;
use serde::{Deserialize, Serialize};

pub use decomp_schema::Privacy;

use crate::job::Manifest;

/// The way a private job was run (spec section 7, "How private mode runs a job").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrivateRoute {
    /// "Local runs the module on the user's device with no verification."
    Local,
    /// "Private totals splits each input into shares held by nodes on different
    /// accounts."
    PrivateTotals,
}

/// No route fits a private job, so it is not run (spec section 7, "Errors":
/// `NoPrivateRoute`, raised "at submit time").
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "no private route fits this job: it does not fit its user's device, and nodes run a private job only as shares, which a job can be cut into only if all it does is add, count, average, or apply fixed weights"
)]
pub struct NoPrivateRoute;

/// The first route that fits a private job with this signature. `fits_device`
/// is whether the device asking can run the job itself.
pub fn route(signature: &Signature, fits_device: bool) -> Result<PrivateRoute, NoPrivateRoute> {
    if fits_device {
        Ok(PrivateRoute::Local)
    } else if signature.is_totals() {
        Ok(PrivateRoute::PrivateTotals)
    } else {
        Err(NoPrivateRoute)
    }
}

/// Why a job may not be handed to nodes in the mode its manifest gives.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum PrivacyError {
    #[error(
        "the manifest's privacy must be what the job's module declares, and only a private job runs as shares"
    )]
    Mismatch,
    #[error(transparent)]
    NoRoute(#[from] NoPrivateRoute),
    #[error("a private job cannot be handed state, which anyone can read")]
    State,
}

/// Checks the mode of a job sent to the coordinator against what its module
/// declares. `declared` is the module's description of itself; a module that
/// gives none is open.
///
/// Nodes may be handed a private job by one route only: as a share of a job
/// that fits the private totals route. Any other private job is refused, because
/// nodes would see its data. So is a share that is handed a shard of state: a
/// job could leave its share there, and anyone can read a shard.
pub fn check(manifest: &Manifest, declared: Option<&Signature>) -> Result<(), PrivacyError> {
    let mode = declared.map_or(Privacy::Open, |signature| signature.privacy);
    if manifest.privacy != mode {
        return Err(PrivacyError::Mismatch);
    }
    let is_share = manifest.share.is_some();
    match manifest.privacy {
        Privacy::Open if is_share => Err(PrivacyError::Mismatch),
        Privacy::Open => Ok(()),
        Privacy::Private if !is_share || !declared.is_some_and(Signature::is_totals) => {
            Err(NoPrivateRoute.into())
        }
        Privacy::Private if manifest.state.is_some() => Err(PrivacyError::State),
        Privacy::Private => Ok(()),
    }
}
