//! Reading the public randomness beacon, and checking a published draw against
//! it (spec section 4: "Each draw is published, so anyone can recheck that
//! nodes were not hand-picked").

use std::collections::HashMap;
use std::time::Duration;

use decomp_core::{
    BEACON_CHAIN, Beacon, Draw, DrawError, Envelope, EnvelopeError, PublicKey, RandomnessSource,
};

/// Where the drand beacon is read.
const BEACON_HOST: &str = "https://api.drand.sh";

/// How long one request to the beacon may take. A round lasts three seconds,
/// so an answer slower than this is of no use.
const BEACON_TIMEOUT: Duration = Duration::from_secs(10);

/// Why a published draw does not hold.
#[derive(Debug, thiserror::Error)]
pub enum DrawFault {
    #[error(transparent)]
    Signature(#[from] EnvelopeError),
    #[error("it is signed by {0}, not by the operator")]
    NotTheOperator(PublicKey),
    #[error(transparent)]
    Draw(#[from] DrawError),
    #[error("the beacon gave other randomness for round {0} than the draw records")]
    NotTheBeacon(u64),
    #[error("could not reach the beacon: {0}")]
    Unreachable(String),
}

/// The beacon, and the rounds already fetched from it.
pub struct Rounds {
    http: ureq::Agent,
    seen: HashMap<u64, Beacon>,
}

impl Default for Rounds {
    fn default() -> Self {
        let http = ureq::Agent::config_builder()
            .timeout_global(Some(BEACON_TIMEOUT))
            .build()
            .new_agent();
        Self {
            http,
            seen: HashMap::new(),
        }
    }
}

impl Rounds {
    /// The beacon's newest round.
    pub fn latest(&self) -> Result<Beacon, String> {
        self.fetch("latest")
    }

    /// The round with this number.
    pub fn round(&mut self, round: u64) -> Result<Beacon, String> {
        if let Some(beacon) = self.seen.get(&round) {
            return Ok(*beacon);
        }
        let beacon = self.fetch(&round.to_string())?;
        self.seen.insert(round, beacon);
        Ok(beacon)
    }

    fn fetch(&self, round: &str) -> Result<Beacon, String> {
        let url = format!("{BEACON_HOST}/{BEACON_CHAIN}/public/{round}");
        let mut response = self.http.get(&url).call().map_err(|e| e.to_string())?;
        response.body_mut().read_json().map_err(|e| e.to_string())
    }

    /// Checks one published draw: that `operator` signed it, that the rule
    /// draws the nodes it names from the round it names, that the round was
    /// the newest when the draw was made, and that the beacon itself gives
    /// that round the same randomness.
    pub fn check(&mut self, signed: &Envelope, operator: &PublicKey) -> Result<Draw, DrawFault> {
        let draw: Draw = signed.open()?;
        if signed.signer != *operator {
            return Err(DrawFault::NotTheOperator(signed.signer));
        }
        draw.check()?;
        // The network's own randomness cannot be fetched again; the subnet's
        // consensus stands behind it, so only the rule is checked.
        if draw.beacon.source == RandomnessSource::Icp {
            return Ok(draw);
        }
        let round = draw.beacon.round;
        if self.round(round).map_err(DrawFault::Unreachable)? != draw.beacon {
            return Err(DrawFault::NotTheBeacon(round));
        }
        Ok(draw)
    }
}
