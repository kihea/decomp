//! The DeComp control plane on the Internet Computer.
//!
//! This canister runs the control plane's services (accounts, registry, job
//! board, receipts) from `decomp-control`, the same library the devnet runs,
//! and answers the same signed calls over the HTTP gateway (`POST /v1/ops`
//! and `POST /v1/request`), so nodes, apps, and the launcher reach it
//! unchanged. The spec splits the job board across canisters by job class and
//! tier; that split waits until load calls for it (spec section 12,
//! "Control-plane throughput").
//!
//! - **Randomness.** Leases and audits are drawn from the Internet
//!   Computer's verifiable randomness (spec section 6: "Lease grants draw on
//!   ICP's verifiable randomness"), fetched every few seconds, which also
//!   reseeds the control plane's own generator.
//! - **Time.** A timer settles jobs whose windows or deadlines pass, and
//!   posts a batch of receipts each period.
//! - **Disputes and fallbacks** run in the canister on the interpreter
//!   (`decomp-sandbox` with `wasmi`).
//! - **Persistence.** The control plane's logs live in the heap, are saved to
//!   stable memory before an upgrade, and are replayed after it.
//!
//! Every request is answered as an update call, so consensus certifies each
//! answer and the gateway passes it on without the canister certifying query
//! responses.

use std::cell::RefCell;
use std::time::Duration;

use candid::{CandidType, Deserialize, Principal};
use decomp_control::referee::{self, referee};
use decomp_control::{ControlPlane, MemorySnapshot, MemoryStorage, Settings};
use decomp_core::{
    BATCH_PERIOD_SECS, BEACON_PERIOD_SECS, Beacon, ContentHash, RandomnessSource, round_at,
};
use decomp_sandbox::Sandbox;

/// What the canister is installed with (`control.did`).
#[derive(CandidType, Deserialize, Clone, Default)]
pub struct InstallSettings {
    /// Account IDs, as hex, allowed to review apps and assign held names.
    pub reviewers: Vec<String>,
    /// Names held for their brands' owners.
    pub reserved_names: Vec<String>,
    /// How long a recovery waits before it takes effect, in seconds. Left
    /// out, the spec's 48 hours.
    pub recovery_wait_secs: Option<u64>,
}

/// How long a stored blob goes between storage challenges, in seconds.
const CHALLENGE_INTERVAL_SECS: u64 = 3600;

/// How often time is let pass for jobs nobody is asking about.
const TICK: Duration = Duration::from_secs(1);

struct Canister {
    install: InstallSettings,
    storage: MemoryStorage,
    control: ControlPlane,
    sandbox: Sandbox,
}

thread_local! {
    static CANISTER: RefCell<Option<Canister>> = const { RefCell::new(None) };
}

fn now() -> u64 {
    ic_cdk::api::time() / 1_000_000_000
}

/// Seed bytes from what the canister knows at install time. They seed the
/// control plane only until the network's randomness arrives, seconds later.
fn install_seed(purpose: &str) -> [u8; 32] {
    let mut bytes = purpose.as_bytes().to_vec();
    bytes.extend_from_slice(&ic_cdk::api::time().to_le_bytes());
    bytes.extend_from_slice(ic_cdk::api::canister_self().as_slice());
    *ContentHash::of(&bytes).as_bytes()
}

fn open(install: InstallSettings, mut storage: MemoryStorage) -> Canister {
    let settings = Settings {
        challenge_interval: CHALLENGE_INTERVAL_SECS,
        recovery_wait: install
            .recovery_wait_secs
            .unwrap_or(decomp_core::RECOVERY_WAIT_SECS),
        reviewers: install
            .reviewers
            .iter()
            .filter_map(|id| id.parse().ok())
            .collect(),
        reserved_names: install.reserved_names.clone(),
        entropy: install_seed("decomp/entropy"),
    };
    let control =
        ControlPlane::open(&mut storage, settings).expect("the control plane's logs replay");
    Canister {
        install,
        storage,
        control,
        sandbox: Sandbox::new(),
    }
}

fn with_canister<R>(f: impl FnOnce(&mut Canister) -> R) -> R {
    CANISTER.with_borrow_mut(|slot| f(slot.as_mut().expect("the canister is installed")))
}

/// Settles what time has decided and runs the disputes and fallbacks that
/// came of it, until none are left.
fn settle(canister: &mut Canister) {
    loop {
        let at = now();
        canister.control.settle(at);
        let cases = canister.control.take_cases();
        let fall_backs = canister.control.take_fall_backs();
        if cases.is_empty() && fall_backs.is_empty() {
            return;
        }
        for case in cases {
            let verdict = referee(&canister.sandbox, &case);
            canister.control.resolve(&case, &verdict, at);
        }
        for (job_id, _, case) in fall_backs {
            let (result, produced) = referee::run(&canister.sandbox, &case, job_id);
            canister.control.fell_back(job_id, result, produced, at);
        }
    }
}

/// Fetches a round of the network's randomness, which leases and audits are
/// drawn from and which reseeds the control plane.
async fn draw_randomness() {
    let Ok(reply) =
        ic_cdk::call::Call::unbounded_wait(Principal::management_canister(), "raw_rand").await
    else {
        return;
    };
    let Ok(bytes) = reply.candid::<Vec<u8>>() else {
        return;
    };
    let Ok(randomness) = <[u8; 32]>::try_from(bytes.as_slice()) else {
        return;
    };
    with_canister(|canister| {
        canister.control.reseed(randomness);
        canister.control.set_beacon(Beacon {
            round: round_at(now()),
            randomness: ContentHash::from_bytes(randomness),
            source: RandomnessSource::Icp,
        });
    });
}

fn start_timers() {
    ic_cdk_timers::set_timer(Duration::ZERO, draw_randomness());
    ic_cdk_timers::set_timer_interval(Duration::from_secs(BEACON_PERIOD_SECS), draw_randomness);
    ic_cdk_timers::set_timer_interval(TICK, || async { with_canister(settle) });
    ic_cdk_timers::set_timer_interval(Duration::from_secs(BATCH_PERIOD_SECS), || async {
        with_canister(|canister| canister.control.post_batch(now()));
    });
}

// --- The HTTP gateway's interface ---

#[derive(CandidType, Deserialize)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    #[serde(with = "serde_bytes")]
    pub body: Vec<u8>,
}

#[derive(CandidType, Deserialize)]
pub struct HttpResponse {
    pub status_code: u16,
    pub headers: Vec<(String, String)>,
    #[serde(with = "serde_bytes")]
    pub body: Vec<u8>,
    pub upgrade: Option<bool>,
}

fn response(status_code: u16, body: Vec<u8>, upgrade: Option<bool>) -> HttpResponse {
    HttpResponse {
        status_code,
        headers: vec![
            ("content-type".into(), "application/json".into()),
            // Requests carry no cookies or credentials, so any web origin may call.
            ("access-control-allow-origin".into(), "*".into()),
            ("access-control-allow-headers".into(), "content-type".into()),
            (
                "access-control-allow-methods".into(),
                "POST, OPTIONS".into(),
            ),
        ],
        body,
        upgrade,
    }
}

/// Every request is passed on as an update call, whose answer consensus
/// certifies. A browser's preflight is answered here.
#[ic_cdk::query]
fn http_request(http: HttpRequest) -> HttpResponse {
    if http.method == "OPTIONS" {
        return response(204, Vec::new(), None);
    }
    response(200, b"{}".to_vec(), Some(true))
}

#[ic_cdk::update]
fn http_request_update(http: HttpRequest) -> HttpResponse {
    let path = http.url.split('?').next().unwrap_or_default().to_string();
    if http.method != "POST" {
        let body =
            br#"{"error":"not_found","message":"the calls are POST /v1/ops and POST /v1/request"}"#;
        return response(404, body.to_vec(), None);
    }
    let answer = with_canister(|canister| {
        let answer = decomp_control::handle(
            &mut canister.control,
            &canister.sandbox,
            &path,
            &http.body,
            now(),
        );
        // Any call can find that a job's window or deadline has passed.
        settle(canister);
        answer
    });
    response(answer.status.0, answer.body, None)
}

#[ic_cdk::init]
fn init(install: Option<InstallSettings>) {
    let storage = MemoryStorage::new(install_seed("decomp/control-key"));
    let canister = open(install.unwrap_or_default(), storage);
    CANISTER.with_borrow_mut(|slot| *slot = Some(canister));
    start_timers();
}

#[ic_cdk::pre_upgrade]
fn pre_upgrade() {
    CANISTER.with_borrow_mut(|slot| {
        let canister = slot.take().expect("the canister is installed");
        let (logs, blobs, key) = canister.storage.snapshot();
        ic_cdk::storage::stable_save((canister.install, logs, blobs, key)).expect("state saves");
    });
}

#[ic_cdk::post_upgrade]
fn post_upgrade() {
    type Saved = (
        InstallSettings,
        Vec<(String, Vec<String>)>,
        Vec<([u8; 32], Vec<u8>)>,
        [u8; 32],
    );
    let (install, logs, blobs, key): Saved =
        ic_cdk::storage::stable_restore().expect("state restores");
    let snapshot: MemorySnapshot = (logs, blobs, key);
    let canister = open(install, MemoryStorage::restore(snapshot));
    CANISTER.with_borrow_mut(|slot| *slot = Some(canister));
    start_timers();
}

/// A canister has no operating system to draw randomness from. Nothing on
/// this canister's path asks for any, since the control plane draws from its
/// own generator; if something did, it fails here instead of drawing
/// predictable bytes.
#[cfg(target_family = "wasm")]
#[unsafe(no_mangle)]
unsafe extern "Rust" fn __getrandom_v04_custom(
    _dest: *mut u8,
    _len: usize,
) -> Result<(), getrandom::Error> {
    Err(getrandom::Error::UNSUPPORTED)
}

ic_cdk::export_candid!();
