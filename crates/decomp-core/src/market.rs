//! Prices, work preferences, standing bids, and the limits on provider leverage
//! (spec section 3, "Open market", and section 5, "Price of a job" and "Limits
//! on provider leverage").
//!
//! Rates are posted by formula: a base rate per metered unit, times a premium
//! for each job class and size tier that rises when the tier has a queue and
//! falls when it has idle nodes, held between a floor and a ceiling. The
//! ceiling is the cost of running the same job as an always-on fallback, so a
//! shortage of nodes can never price an app above that.
//!
//! A node owner shapes what the node is offered through [`WorkPreferences`]:
//! which classes and tiers it takes, how many slots it offers, and the lowest
//! rate it will work for. Preferences describe kinds of work, never a job or a
//! module, so a node still cannot choose which job it confirms.
//!
//! Accounts that opt out of standard allocation for the large tiers take work
//! there through standing [`Bid`]s. A bid names a class, a tier range, how many
//! jobs, and a price per unit or per job. Bids can only go below the posted
//! rate: a bid above it waits until the rate rises to meet it. A filled bid is
//! paid what it asked, and the job's payer keeps the difference.
//!
//! Nothing here does I/O. Every amount is a whole number of microcredits, and
//! every multiplier is a whole number of thousandths, so that every party that
//! applies the same events reaches the same prices.

use std::collections::{BTreeMap, HashMap, VecDeque};

use serde::{Deserialize, Serialize};

use crate::keys::{AccountId, ContentHash};
use crate::ledger::Credits;

/// The base rate for compute: microcredits per compute unit (one unit is one
/// billion WebAssembly instructions). At this rate one credit buys 4,000 units,
/// about one hour of one slot on a reference core. A starting value.
pub const BASE_MICROS_PER_UNIT: u64 = 250;

/// Multipliers are counted in thousandths.
pub const PREMIUM_SCALE: u64 = 1_000;

/// The premium every class and tier starts at: 1.0.
pub const STARTING_PREMIUM: u64 = PREMIUM_SCALE;

/// The lowest a premium may fall: half the base rate. A starting value.
pub const PREMIUM_FLOOR: u64 = 500;

/// What running a job as an always-on fallback costs, as a multiple of the base
/// rate. It is the ceiling for every posted rate. A starting value, to be
/// replaced by the measured cost of running the same work in a canister.
pub const FALLBACK_PREMIUM: u64 = 4_000;

/// How often premiums are retuned, in seconds.
pub const RETUNE_SECS: u64 = 60;

/// How far one retune moves a premium up when a tier has a queue, and down when
/// it has idle nodes, in thousandths of the premium.
const PREMIUM_RISE: u64 = 100;
const PREMIUM_FALL: u64 = 50;

/// A tier counts as having idle nodes when more than this many nodes are
/// asking for work in it per job waiting.
const IDLE_ASKERS_PER_WAITING_JOB: u64 = 2;

/// The lowest tier whose jobs are listed for bids (spec section 3: "Jobs in
/// tier 3 and above can also be listed for bids").
pub const MARKET_FIRST_TIER: u32 = 3;

/// The most standing bids one account may hold at once.
pub const MAX_BIDS_PER_ACCOUNT: usize = 64;

/// The most jobs one bid may ask for.
pub const MAX_BID_QUANTITY: u32 = 10_000;

/// The longest a bid may stand, in seconds.
pub const MAX_BID_SECS: u64 = 30 * 24 * 3600;

/// A new account's share cap: the fraction of a tier's leases it may be granted,
/// in millionths (0.1%).
pub const STARTING_SHARE_CAP_PPM: u64 = 1_000;

/// The share cap no account passes, however reliable or old (2%).
pub const SHARE_CAP_CEILING_PPM: u64 = 20_000;

/// The account age at which age stops raising the share cap, in seconds.
const SHARE_CAP_FULL_AGE_SECS: u64 = 90 * 24 * 3600;

/// The reliability at which reliability stops raising the share cap. Below the
/// starting score of 0.5 it adds nothing.
const SHARE_CAP_FULL_RELIABILITY: f64 = 0.99;
const SHARE_CAP_ZERO_RELIABILITY: f64 = 0.5;

/// Caps and the taper apply only once a tier has this many active accounts.
/// Below that every available node is needed.
pub const SHARE_CAP_MIN_ACCOUNTS: usize = 100;

/// The stretch of time over which each account's share of a tier is counted.
pub const SHARE_WINDOW_SECS: u64 = 3600;

/// What an account is paid per unit at its cap, in thousandths of the rate.
/// Pay falls steadily to this from half the cap.
const TAPER_AT_CAP: u64 = 700;

/// A node is a "balanced" account's when the account spent credits in spaces
/// within this long. Their lease requests are served first.
pub const BALANCED_LOOKBACK_SECS: u64 = 30 * 24 * 3600;

/// The three kinds of job (spec section 2, "Job classes").
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum JobClass {
    /// Hashing, signature checks, validating another node's result.
    #[default]
    Menial,
    /// Storing, serving, and relaying blobs; encoding images and video.
    Data,
    /// Model generation, embeddings, ranking with a model.
    Inference,
}

impl JobClass {
    pub const ALL: [JobClass; 3] = [JobClass::Menial, JobClass::Data, JobClass::Inference];
}

/// What a node owner allows the node to be offered (spec section 1, "Node
/// agent": "The owner sets contribution rules in the agent"). The job board
/// applies the parts it can see; the agent applies the rest (hours, battery,
/// metered networks) on the device before it asks for work.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkPreferences {
    /// Whether the device contributes at all.
    pub contributing: bool,
    /// The job classes the device accepts. Empty means none.
    pub classes: Vec<JobClass>,
    /// The tiers the device accepts, within those its standing allows.
    pub min_tier: u32,
    pub max_tier: u32,
    /// The lowest posted rate the device works for, in microcredits per unit.
    /// Zero takes any rate.
    pub floor_micros_per_unit: u64,
    /// The most slots the device offers at once.
    pub slots: u32,
    /// Local hours of the day the device works, start inclusive and end
    /// exclusive. `None` is any hour. Applied by the agent.
    #[serde(default)]
    pub hours: Option<(u8, u8)>,
    /// Whether the device works on battery power. Applied by the agent.
    #[serde(default)]
    pub on_battery: bool,
    /// Whether the device works on a metered network. Applied by the agent.
    #[serde(default)]
    pub on_metered: bool,
    /// The device takes work in the market tiers only through its account's
    /// bids, and is left out of standard allocation there.
    #[serde(default)]
    pub market_only: bool,
}

/// The most slots a device may offer.
pub const MAX_SLOTS: u32 = 256;

/// The memory one slot needs, in megabytes (spec section 1, "Slots": "a
/// sandbox with 1 CPU core, 512 MB of memory").
pub const SLOT_MEMORY_MB: u64 = 512;

/// What a device says its hardware is (spec section 1, "Node profile":
/// hardware fit is measured by "declared specs, confirmed by benchmark jobs").
/// The agent reads it from the system. It bounds the slots a device is offered,
/// and an inference job is offered only to a device that holds every model the
/// job calls, and has a GPU if one of them needs it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hardware {
    pub cores: u32,
    pub memory_mb: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gpus: Vec<Gpu>,
    /// The approved models the device holds and can run, by ID.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ContentHash>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gpu {
    pub name: String,
    pub memory_mb: u64,
}

impl Hardware {
    /// How many slots the hardware can run at once: one per core, as far as
    /// memory goes.
    pub fn slots(&self) -> u32 {
        let by_memory = self.memory_mb / SLOT_MEMORY_MB;
        self.cores.min(u32::try_from(by_memory).unwrap_or(u32::MAX))
    }
}

impl Default for WorkPreferences {
    /// Everything the node is eligible for, at any rate, one slot, on mains
    /// power and unmetered networks only. This is what the automatic
    /// orchestrator in the agent uses, and it is the recommended setting.
    fn default() -> Self {
        Self {
            contributing: true,
            classes: JobClass::ALL.to_vec(),
            min_tier: 0,
            max_tier: crate::job::MAX_TIER,
            floor_micros_per_unit: 0,
            slots: 1,
            hours: None,
            on_battery: false,
            on_metered: false,
            market_only: false,
        }
    }
}

impl WorkPreferences {
    /// Why these preferences cannot be stored, if they cannot.
    pub fn problem(&self) -> Option<&'static str> {
        if self.min_tier > self.max_tier || self.max_tier > crate::job::MAX_TIER {
            return Some("the tier range must run from low to high, within 0 to 4");
        }
        if self.slots == 0 || self.slots > MAX_SLOTS {
            return Some("a device offers between 1 and 256 slots");
        }
        if self
            .hours
            .is_some_and(|(start, end)| start > 23 || end > 24)
        {
            return Some("hours are local hours of the day, 0 to 24");
        }
        let mut classes = self.classes.clone();
        classes.sort();
        classes.dedup();
        if classes.len() != self.classes.len() {
            return Some("each job class is listed once");
        }
        None
    }

    /// Whether a job of this class and tier, at this rate, may be offered to
    /// the device in standard allocation.
    pub fn accepts(&self, class: JobClass, tier: u32, micros_per_unit: u64) -> bool {
        self.contributing
            && self.classes.contains(&class)
            && (self.min_tier..=self.max_tier).contains(&tier)
            && micros_per_unit >= self.floor_micros_per_unit
            && !(self.market_only && tier >= MARKET_FIRST_TIER)
    }
}

/// What a bid asks to be paid.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Price {
    /// Microcredits per metered unit.
    PerUnit(u64),
    /// Microcredits for one whole job, whatever it uses up to its cap. It is
    /// paid in full only when the posted fee for the work done is at least as
    /// much; otherwise the posted fee is paid.
    PerJob(u64),
}

/// A standing order to run jobs at a price (spec section 3, "Open market").
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BidOrder {
    pub class: JobClass,
    pub min_tier: u32,
    pub max_tier: u32,
    /// How many jobs the bid asks for.
    pub quantity: u32,
    pub price: Price,
    /// When the bid lapses, in seconds since 1970.
    pub expires_at: u64,
}

/// A bid as the job board holds it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bid {
    pub id: ContentHash,
    pub account: AccountId,
    #[serde(flatten)]
    pub order: BidOrder,
    /// Jobs granted against the bid so far.
    pub filled: u32,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum BidError {
    #[error("bids are taken for tiers {MARKET_FIRST_TIER} to 4 only")]
    BelowMarketTier,
    #[error("the tier range must run from low to high, within 3 to 4")]
    BadTierRange,
    #[error("a bid asks for between 1 and {MAX_BID_QUANTITY} jobs")]
    BadQuantity,
    #[error("a bid's price must be more than nothing")]
    NoPrice,
    #[error("a bid must lapse in the future, and within 30 days")]
    BadExpiry,
    #[error("an account holds at most {MAX_BIDS_PER_ACCOUNT} bids")]
    TooManyBids,
    #[error("no such bid")]
    UnknownBid,
}

impl BidOrder {
    pub fn check(&self, now: u64) -> Result<(), BidError> {
        if self.min_tier < MARKET_FIRST_TIER {
            return Err(BidError::BelowMarketTier);
        }
        if self.min_tier > self.max_tier || self.max_tier > crate::job::MAX_TIER {
            return Err(BidError::BadTierRange);
        }
        if self.quantity == 0 || self.quantity > MAX_BID_QUANTITY {
            return Err(BidError::BadQuantity);
        }
        if matches!(self.price, Price::PerUnit(0) | Price::PerJob(0)) {
            return Err(BidError::NoPrice);
        }
        if self.expires_at <= now || self.expires_at > now + MAX_BID_SECS {
            return Err(BidError::BadExpiry);
        }
        Ok(())
    }
}

impl Bid {
    pub fn is_live(&self, now: u64) -> bool {
        self.filled < self.order.quantity && self.expires_at() > now
    }

    pub fn expires_at(&self) -> u64 {
        self.order.expires_at
    }

    /// Whether the bid can take a job of this class and tier, whose posted rate
    /// is `micros_per_unit` and whose cap is `compute_cap` units. Bids can only
    /// go below the posted rate.
    pub fn matches(
        &self,
        class: JobClass,
        tier: u32,
        micros_per_unit: u64,
        compute_cap: u64,
    ) -> bool {
        self.order.class == class
            && (self.order.min_tier..=self.order.max_tier).contains(&tier)
            && match self.order.price {
                Price::PerUnit(price) => price <= micros_per_unit,
                Price::PerJob(price) => price <= micros_per_unit.saturating_mul(compute_cap),
            }
    }

    /// The bid's price for a job of `compute_cap` units, per unit, for ranking
    /// bids against each other. A per-job price is spread over the whole cap.
    pub fn rank_price(&self, compute_cap: u64) -> u64 {
        match self.order.price {
            Price::PerUnit(price) => price,
            Price::PerJob(price) => price / compute_cap.max(1),
        }
    }
}

/// The price a node runs one lease at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LeasePrice {
    /// The posted rate, locked when the job was submitted.
    Posted { micros_per_unit: u64 },
    /// What a filled bid asked.
    Bid { bid: ContentHash, price: Price },
}

impl LeasePrice {
    /// What a run that used `units` earns before any taper, given the job's
    /// posted rate.
    pub fn fee(&self, units: u64, posted_micros_per_unit: u64) -> Credits {
        let posted = units.saturating_mul(posted_micros_per_unit);
        Credits::from_micros(match *self {
            LeasePrice::Posted { micros_per_unit } => units.saturating_mul(micros_per_unit),
            LeasePrice::Bid {
                price: Price::PerUnit(price),
                ..
            } => units.saturating_mul(price),
            LeasePrice::Bid {
                price: Price::PerJob(price),
                ..
            } => price.min(posted),
        })
    }
}

/// The rate posted now for one class and tier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rate {
    pub class: JobClass,
    pub tier: u32,
    pub micros_per_unit: u64,
    /// The tier's premium, in thousandths.
    pub premium: u64,
    /// Jobs waiting for a node in this tier at the last retune.
    pub waiting: u64,
    /// Nodes asking for work in this tier at the last retune.
    pub asking: u64,
}

/// The base rate of each class, in microcredits per metered unit. Only compute
/// has a starting value in the spec; the inference meter (tokens) is to be set
/// from testing, so until then inference is metered in compute units at the
/// compute rate.
pub fn base_rate(class: JobClass) -> u64 {
    match class {
        JobClass::Menial | JobClass::Data | JobClass::Inference => BASE_MICROS_PER_UNIT,
    }
}

/// The rate an always-on fallback is charged at, which is also the ceiling of
/// every posted rate.
pub fn fallback_rate(class: JobClass) -> u64 {
    base_rate(class) * FALLBACK_PREMIUM / PREMIUM_SCALE
}

/// Every class and tier's premium, retuned from supply and demand.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Rates {
    premiums: BTreeMap<(JobClass, u32), Rate>,
    retuned_at: u64,
}

impl Rates {
    /// The rate posted now for a class and tier.
    pub fn posted(&self, class: JobClass, tier: u32) -> Rate {
        self.premiums.get(&(class, tier)).copied().unwrap_or(Rate {
            class,
            tier,
            micros_per_unit: base_rate(class),
            premium: STARTING_PREMIUM,
            waiting: 0,
            asking: 0,
        })
    }

    /// Every class and tier's posted rate.
    pub fn all(&self) -> Vec<Rate> {
        JobClass::ALL
            .iter()
            .flat_map(|&class| (0..=crate::job::MAX_TIER).map(move |tier| (class, tier)))
            .map(|(class, tier)| self.posted(class, tier))
            .collect()
    }

    /// Whether a retune is due.
    pub fn is_due(&self, now: u64) -> bool {
        now >= self.retuned_at + RETUNE_SECS
    }

    /// Moves each tier's premium toward balance. `load` gives, for each class
    /// and tier, the jobs waiting for a node and the nodes asking for work. A
    /// tier with a queue costs more; a tier with idle nodes costs less.
    pub fn retune(&mut self, load: &HashMap<(JobClass, u32), (u64, u64)>, now: u64) {
        self.retuned_at = now;
        for class in JobClass::ALL {
            for tier in 0..=crate::job::MAX_TIER {
                let (waiting, asking) = load.get(&(class, tier)).copied().unwrap_or((0, 0));
                let mut rate = self.posted(class, tier);
                let premium = rate.premium;
                rate.premium = if waiting > asking {
                    premium + premium * PREMIUM_RISE / PREMIUM_SCALE
                } else if asking > waiting.saturating_mul(IDLE_ASKERS_PER_WAITING_JOB) {
                    premium - premium * PREMIUM_FALL / PREMIUM_SCALE
                } else {
                    premium
                }
                .clamp(PREMIUM_FLOOR, FALLBACK_PREMIUM);
                rate.micros_per_unit = base_rate(class) * rate.premium / PREMIUM_SCALE;
                rate.waiting = waiting;
                rate.asking = asking;
                self.premiums.insert((class, tier), rate);
            }
        }
    }
}

/// What the share caps need to know about an account.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AccountStanding {
    /// The combined reliability of the account's nodes.
    pub reliability: f64,
    /// When the account first appeared, in seconds since 1970.
    pub created: u64,
    /// The account spent credits in spaces within `BALANCED_LOOKBACK_SECS`.
    pub balanced: bool,
}

/// An account's share cap in a tier, in millionths of the tier's leases. It
/// grows with reliability and age toward the ceiling, and never past it.
pub fn share_cap_ppm(standing: &AccountStanding, now: u64) -> u64 {
    let reliability = ((standing.reliability - SHARE_CAP_ZERO_RELIABILITY)
        / (SHARE_CAP_FULL_RELIABILITY - SHARE_CAP_ZERO_RELIABILITY))
        .clamp(0.0, 1.0);
    let age =
        (now.saturating_sub(standing.created) as f64 / SHARE_CAP_FULL_AGE_SECS as f64).min(1.0);
    let room = (SHARE_CAP_CEILING_PPM - STARTING_SHARE_CAP_PPM) as f64;
    STARTING_SHARE_CAP_PPM + (room * reliability * age).round() as u64
}

/// What an account is paid per unit, in thousandths of the price, when it
/// already holds `share_ppm` of a tier's leases against a cap of `cap_ppm`.
/// Full pay up to half the cap, then falling steadily to 70% at the cap.
pub fn taper(share_ppm: u64, cap_ppm: u64) -> u64 {
    let half = cap_ppm / 2;
    if share_ppm <= half || cap_ppm == 0 {
        return PREMIUM_SCALE;
    }
    let over = (share_ppm - half).min(cap_ppm - half);
    PREMIUM_SCALE - (PREMIUM_SCALE - TAPER_AT_CAP) * over / (cap_ppm - half).max(1)
}

/// Who was granted the leases of each tier lately, for share caps and the taper.
#[derive(Clone, Debug, Default)]
pub struct Shares {
    grants: BTreeMap<u32, VecDeque<(u64, AccountId)>>,
}

impl Shares {
    pub fn record(&mut self, tier: u32, account: AccountId, now: u64) {
        self.grants
            .entry(tier)
            .or_default()
            .push_back((now, account));
    }

    fn trim(&mut self, now: u64) {
        for grants in self.grants.values_mut() {
            while grants
                .front()
                .is_some_and(|(at, _)| now.saturating_sub(*at) > SHARE_WINDOW_SECS)
            {
                grants.pop_front();
            }
        }
    }

    /// For a tier: how many distinct accounts were granted leases in the window,
    /// and each account's share in millionths.
    pub fn of_tier(&mut self, tier: u32, now: u64) -> (usize, HashMap<AccountId, u64>) {
        self.trim(now);
        let Some(grants) = self.grants.get(&tier) else {
            return (0, HashMap::new());
        };
        let total = grants.len() as u64;
        let mut counts: HashMap<AccountId, u64> = HashMap::new();
        for (_, account) in grants {
            *counts.entry(*account).or_default() += 1;
        }
        let accounts = counts.len();
        let shares = counts
            .into_iter()
            .map(|(account, count)| (account, count * 1_000_000 / total.max(1)))
            .collect();
        (accounts, shares)
    }
}
