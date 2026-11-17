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
//! | 3 | Encrypted compute | "Any other job, up to a size cap": nodes run it on encrypted values (see `encrypted`) |
//! | 4 | None | "The job is over the size cap": it is not run |
//!
//! When no route fits, the job is not run, and the device gets `NoPrivateRoute`.
//! The coordinator applies the same rule to what it is sent, so a private job
//! that reaches it by no route is refused before any node is handed it.
//!
//! The third route's "any other job" is any other job written with the private
//! values that route can compute on: "The code must use the SDK's private
//! number and true-or-false types" (spec section 7, "Limits of 'general'"). A
//! job that takes a private value of another kind has no route but the first.

use decomp_schema::{Mismatch, Signature};
use serde::{Deserialize, Serialize};

pub use decomp_schema::Privacy;

use crate::job::Manifest;

/// The largest job, in compute units, that is run by encrypted compute: its
/// "size cap" (spec section 7). A private job allowed more than this has no
/// third route. The spec has the cap set from measurements: "Benchmark standard
/// encrypted operations and set the size cap from the results."
///
/// A starting value. Measured on one computer, a job spends 5.1 units getting
/// its key ready, 1.25 units on each gate between two encrypted bits, and about
/// two seconds on each unit. So this cap is a job of about 200 gates, which is
/// one addition, or one comparison and a choice, of two 32-bit numbers worked
/// out bit by bit, and about nine minutes on each node: the far end of the
/// spec's "network jobs finish in seconds to minutes".
pub const ENCRYPTED_COMPUTE_MAX_UNITS: u64 = 256;

/// The way a private job was run (spec section 7, "How private mode runs a job").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrivateRoute {
    /// "Local runs the module on the user's device with no verification."
    Local,
    /// "Private totals splits each input into shares held by nodes on different
    /// accounts."
    PrivateTotals,
    /// "Encrypted compute uses homomorphic encryption, so a node computes on
    /// values it cannot read."
    EncryptedCompute,
}

/// No route fits a private job, so it is not run (spec section 7, "Errors":
/// `NoPrivateRoute`, raised "at submit time").
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "no private route fits this job. It does not fit its user's device. Nodes run a private job on shares only if all it does is add, count, average, or apply fixed weights. They run one on encrypted values only if its private values are all numbers and true-or-false values that can be computed on while encrypted, and it is allowed no more than {ENCRYPTED_COMPUTE_MAX_UNITS} compute units"
)]
pub struct NoPrivateRoute;

/// The first route that fits a private job with this signature that is allowed
/// `compute_cap` compute units. `fits_device` is whether the device asking can
/// run the job itself.
pub fn route(
    signature: &Signature,
    compute_cap: u64,
    fits_device: bool,
) -> Result<PrivateRoute, NoPrivateRoute> {
    if fits_device {
        Ok(PrivateRoute::Local)
    } else if signature.is_totals() {
        Ok(PrivateRoute::PrivateTotals)
    } else if fits_encrypted_compute(signature, compute_cap) {
        Ok(PrivateRoute::EncryptedCompute)
    } else {
        Err(NoPrivateRoute)
    }
}

/// Whether a job can be run by encrypted compute: its private values are all
/// of the kinds that can be computed on while encrypted, and it is within the
/// size cap.
fn fits_encrypted_compute(signature: &Signature, compute_cap: u64) -> bool {
    signature.is_encrypted() && compute_cap <= ENCRYPTED_COMPUTE_MAX_UNITS
}

/// The route a private job sent to nodes is on, which its manifest shows: a
/// share is on the private totals route, and nodes are handed no other private
/// job but one to compute on encrypted values. `None` for an open job.
pub fn route_to_nodes(manifest: &Manifest) -> Option<PrivateRoute> {
    match (manifest.privacy, manifest.share) {
        (Privacy::Open, _) => None,
        (Privacy::Private, Some(_)) => Some(PrivateRoute::PrivateTotals),
        (Privacy::Private, None) => Some(PrivateRoute::EncryptedCompute),
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
/// Nodes may be handed a private job by two routes only: as a share of a job
/// that fits the private totals route, or whole, to compute on encrypted
/// values, if it fits the encrypted compute route. Any other private job is
/// refused, because nodes would see its data. So is a private job that is
/// handed a shard of state: a job could leave a share there, and anyone can
/// read a shard.
pub fn check(manifest: &Manifest, declared: Option<&Signature>) -> Result<(), PrivacyError> {
    let mode = declared.map_or(Privacy::Open, |signature| signature.privacy);
    if manifest.privacy != mode {
        return Err(PrivacyError::Mismatch);
    }
    let fits = |route| match (route, declared) {
        (PrivateRoute::PrivateTotals, Some(signature)) => signature.is_totals(),
        (PrivateRoute::EncryptedCompute, Some(signature)) => {
            fits_encrypted_compute(signature, manifest.compute_cap)
        }
        _ => false,
    };
    match route_to_nodes(manifest) {
        None if manifest.share.is_some() => Err(PrivacyError::Mismatch),
        None => Ok(()),
        Some(route) if !fits(route) => Err(NoPrivateRoute.into()),
        Some(_) if manifest.state.is_some() => Err(PrivacyError::State),
        Some(_) => Ok(()),
    }
}

/// Checks a job's input against the types its module declares, in the form
/// nodes are handed it. On the encrypted compute route every private value in
/// it must be encrypted, so a device that sent one as it is has it refused.
/// The coordinator cannot tell a share from a number, or whether what is in
/// the place of an encrypted value was encrypted well.
pub fn check_input(
    manifest: &Manifest,
    signature: &Signature,
    input: &[u8],
) -> Result<(), Mismatch> {
    match route_to_nodes(manifest) {
        Some(PrivateRoute::EncryptedCompute) => signature.check_encrypted_input(input),
        _ => signature.check_input(input),
    }
}
