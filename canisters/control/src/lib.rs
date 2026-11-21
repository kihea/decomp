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
//! - **Fallbacks.** A job with an always-on fallback that no node finished
//!   runs in the canister on the interpreter (`decomp-sandbox` with
//!   `wasmi`). Such jobs leave their data here; every other job's data stays
//!   on storage nodes, and disputes are settled by further nodes.
//! - **Value keys.** Keys for an account's private values are derived by
//!   the subnet's threshold key (vetKD) and encrypted to the asking device
//!   (`decomp_core::value_keys`), so no single party holds them.
//! - **Persistence.** The control plane's logs live in the heap, are saved to
//!   stable memory before an upgrade, and are replayed after it.
//!
//! Every request is answered as an update call, so consensus certifies each
//! answer and the gateway passes it on without the canister certifying query
//! responses.

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::Duration;

use candid::{CandidType, Deserialize, Principal};
use decomp_control::fallback;
use decomp_control::{ControlPlane, MemorySnapshot, MemoryStorage, Settings, ValueKeyRequest};
use decomp_core::{
    AccountId, BATCH_PERIOD_SECS, BEACON_PERIOD_SECS, Beacon, Blob, ContentHash, RandomnessSource,
    Refusal, VALUE_KEY_CONTEXT, ValueKey, round_at,
};
use decomp_sandbox::Sandbox;
use ic_cdk_management_canister::{
    VetKDCurve, VetKDDeriveKeyArgs, VetKDKeyId, VetKDPublicKeyArgs, vetkd_derive_key,
    vetkd_public_key,
};

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

/// The subnet's threshold key that value keys are derived with: the
/// network's production key, which a local replica also provides.
const THRESHOLD_KEY: &str = "key_1";

/// How many value keys one account may ask for in a day. Each costs the
/// canister cycles; a device keeps the keys it has been given, so it asks
/// rarely.
const VALUE_KEYS_PER_ACCOUNT_PER_DAY: u32 = 100;

const SECS_PER_DAY: u64 = 86_400;

struct Canister {
    install: InstallSettings,
    storage: MemoryStorage,
    control: ControlPlane,
    sandbox: Sandbox,
}

thread_local! {
    static CANISTER: RefCell<Option<Canister>> = const { RefCell::new(None) };
    /// The threshold key's public key for the value-key context, fetched once.
    static VALUE_KEY_PUBLIC: RefCell<Option<Vec<u8>>> = const { RefCell::new(None) };
    /// Value keys asked for today, by account: the day and the count.
    static VALUE_KEYS_ASKED: RefCell<HashMap<AccountId, (u64, u32)>> =
        RefCell::new(HashMap::new());
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

/// Settles what time has decided and runs the fallbacks that came of it,
/// until none are left.
fn settle(canister: &mut Canister) {
    loop {
        let at = now();
        canister.control.settle(at);
        let fall_backs = canister.control.take_fall_backs();
        if fall_backs.is_empty() {
            return;
        }
        for (job_id, run) in fall_backs {
            let (result, produced) = fallback::run(&canister.sandbox, &run, job_id);
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

fn refusal(status_code: u16, error: &str, message: impl Into<String>) -> HttpResponse {
    let body = Refusal {
        error: error.into(),
        message: message.into(),
    };
    response(
        status_code,
        serde_json::to_vec(&body).expect("refusals serialize"),
        None,
    )
}

fn value_key_key_id() -> VetKDKeyId {
    VetKDKeyId {
        curve: VetKDCurve::Bls12_381_G2,
        name: THRESHOLD_KEY.into(),
    }
}

/// Counts a value key asked for by `account`, or refuses it if the account
/// has asked for as many as it may today.
fn count_value_key(account: AccountId) -> bool {
    let day = now() / SECS_PER_DAY;
    VALUE_KEYS_ASKED.with_borrow_mut(|asked| {
        asked.retain(|_, (counted_day, _)| *counted_day == day);
        let (_, count) = asked.entry(account).or_insert((day, 0));
        if *count >= VALUE_KEYS_PER_ACCOUNT_PER_DAY {
            return false;
        }
        *count += 1;
        true
    })
}

/// Has the threshold key derive a value key, and answers with it.
async fn value_key(request: ValueKeyRequest) -> HttpResponse {
    if !count_value_key(request.account) {
        let message = format!(
            "an account may ask for {VALUE_KEYS_PER_ACCOUNT_PER_DAY} value keys a day;              keep the keys this device has been given"
        );
        return refusal(429, "too_many_value_keys", message);
    }
    let derived = vetkd_derive_key(&VetKDDeriveKeyArgs {
        input: request.input,
        context: VALUE_KEY_CONTEXT.to_vec(),
        transport_public_key: request.transport_key,
        key_id: value_key_key_id(),
    })
    .await;
    let encrypted_key = match derived {
        Ok(derived) => derived.encrypted_key,
        Err(error) => return refusal(503, "threshold_key_failed", error.to_string()),
    };
    let public_key = match VALUE_KEY_PUBLIC.with_borrow(Clone::clone) {
        Some(key) => key,
        None => {
            let fetched = vetkd_public_key(&VetKDPublicKeyArgs {
                canister_id: None,
                context: VALUE_KEY_CONTEXT.to_vec(),
                key_id: value_key_key_id(),
            })
            .await;
            match fetched {
                Ok(fetched) => {
                    VALUE_KEY_PUBLIC.set(Some(fetched.public_key.clone()));
                    fetched.public_key
                }
                Err(error) => return refusal(503, "threshold_key_failed", error.to_string()),
            }
        }
    };
    let answer = ValueKey {
        encrypted_key: Blob(encrypted_key),
        public_key: Blob(public_key),
    };
    response(
        200,
        serde_json::to_vec(&answer).expect("answers serialize"),
        None,
    )
}

#[ic_cdk::update]
async fn http_request_update(http: HttpRequest) -> HttpResponse {
    let path = http.url.split('?').next().unwrap_or_default().to_string();
    if http.method != "POST" {
        return refusal(
            404,
            "not_found",
            "the calls are POST /v1/ops and POST /v1/request",
        );
    }
    let asked = with_canister(|canister| {
        decomp_control::value_key_request(&canister.control, &path, &http.body, now())
    });
    match asked {
        Some(Ok(request)) => return value_key(request).await,
        Some(Err(answer)) => return response(answer.status.0, answer.body, None),
        None => {}
    }
    let answer = with_canister(|canister| {
        let answer = decomp_control::handle(&mut canister.control, &path, &http.body, now());
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
