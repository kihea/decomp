//! The DeComp control plane on the Internet Computer.
//!
//! This canister holds the accounts, registry, and records services (spec,
//! "Planes and components"): account records, device lists, controllers, and
//! balances; names, manifests, and review; and the current version of each
//! record. It applies the same rules as the devnet, from `decomp-core`, and
//! answers the same signed calls over the HTTP gateway (`POST /v1/ops` and
//! `POST /v1/request`), so nodes, apps, and the launcher reach it unchanged.
//! The job board and receipts run as canisters of their own.
//!
//! Every request is answered as an update call: its answer is certified by
//! the subnet's consensus, so the gateway passes it on without the canister
//! certifying query responses.
//!
//! State changes only by applying logged entries, as on the devnet. The logs
//! live in the canister's heap and are written to stable memory before an
//! upgrade and replayed after it.

use std::cell::RefCell;
use std::collections::HashMap;

use candid::{CandidType, Deserialize};
use decomp_core::registry::{MAX_DIRECTORY_PAGE, check_syntax};
use decomp_core::{
    Account, AccountId, AccountStatus, Accounts, Activity, Call, ContentHash, Directory, Envelope,
    Ledger, LedgerEntry, MAX_CLOCK_SKEW_SECS, NAME_DEPOSIT, NameError, NameRecord, PublicKey,
    PublishedKey, RECOVERY_WAIT_SECS, Record, RecordError, RecordList, Records, Refusal, Registry,
    RegistryEntry, Request, Resolved, SealingKeys,
};
use serde::Serialize;

/// What the canister is installed with.
#[derive(CandidType, Deserialize, Default)]
pub struct Settings {
    /// Account IDs, as hex, allowed to review apps and assign held names.
    pub reviewers: Vec<String>,
    /// Names held for their brands' owners.
    pub reserved_names: Vec<String>,
    /// How long a recovery waits before it takes effect, in seconds. Left
    /// out, the spec's 48 hours.
    pub recovery_wait_secs: Option<u64>,
}

/// Everything the canister keeps, rebuilt from its logs.
struct State {
    settings: Settings,
    reviewers: Vec<AccountId>,
    accounts: Accounts,
    ledger: Ledger,
    registry: Registry,
    records: Records,
    sealing_keys: SealingKeys,
    created: HashMap<AccountId, u64>,
    logs: Logs,
}

/// The logs that are the canister's durable state.
#[derive(Default, CandidType, Deserialize)]
struct Logs {
    /// Signed account changes, each with when it was applied.
    ops: Vec<(String, u64)>,
    /// Credit movements, as JSON.
    ledger: Vec<String>,
    /// Registry entries, as JSON.
    registry: Vec<String>,
    /// Records, as JSON.
    records: Vec<String>,
    /// Each device's signed request publishing its sealing key, as JSON.
    sealing_keys: Vec<String>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

impl State {
    fn new(settings: Settings) -> Self {
        let wait = settings.recovery_wait_secs.unwrap_or(RECOVERY_WAIT_SECS);
        let reviewers = settings
            .reviewers
            .iter()
            .filter_map(|id| id.parse().ok())
            .collect();
        let mut registry = Registry::default();
        for name in &settings.reserved_names {
            registry.apply(&RegistryEntry::Reserve { name: name.clone() });
        }
        Self {
            settings,
            reviewers,
            accounts: Accounts::waiting(wait),
            ledger: Ledger::default(),
            registry,
            records: Records::default(),
            sealing_keys: SealingKeys::default(),
            created: HashMap::new(),
            logs: Logs::default(),
        }
    }

    /// Rebuilds the state from its logs.
    fn replay(settings: Settings, logs: Logs) -> Self {
        let mut state = Self::new(settings);
        for (op, at) in &logs.ops {
            let envelope: Envelope = serde_json::from_str(op).expect("logged ops parse");
            let account = state
                .accounts
                .apply_at(&envelope, *at)
                .expect("logged ops apply")
                .id;
            state.ledger.open(account);
            state.created.entry(account).or_insert(*at);
        }
        for entry in &logs.ledger {
            let entry: LedgerEntry = serde_json::from_str(entry).expect("logged entries parse");
            state.ledger.apply(&entry).expect("logged entries apply");
        }
        for entry in &logs.registry {
            let entry: RegistryEntry = serde_json::from_str(entry).expect("logged entries parse");
            state.registry.apply(&entry);
        }
        for proof in &logs.sealing_keys {
            let proof: Envelope = serde_json::from_str(proof).expect("logged keys parse");
            state.sealing_keys.apply(&proof).expect("logged keys apply");
        }
        for record in &logs.records {
            let record: Record = serde_json::from_str(record).expect("logged records parse");
            state.records.apply(record);
        }
        state.logs = logs;
        state
    }

    fn record_ledger(&mut self, entry: LedgerEntry) -> Result<(), Answer> {
        self.ledger
            .apply(&entry)
            .map_err(|error| Answer::refuse(402, "ledger", error))?;
        self.logs.ledger.push(json(&entry));
        Ok(())
    }

    fn note_registry(&mut self, entry: RegistryEntry) {
        self.registry.apply(&entry);
        self.logs.registry.push(json(&entry));
    }

    fn status(&self, account: Account) -> AccountStatus {
        let balance = self.ledger.balance(&account.id);
        let created = self.created.get(&account.id).copied().unwrap_or(0);
        AccountStatus {
            account,
            balance,
            // Node scores live with the job board.
            reputation: 0.0,
            created,
        }
    }

    fn resolved(&self, record: &NameRecord) -> Resolved {
        let manifest = self
            .accounts
            .get(&record.app)
            .and_then(|account| account.app.clone())
            .expect("a named app has a manifest");
        Resolved {
            record: record.clone(),
            manifest,
        }
    }
}

/// An HTTP answer: a status and a JSON body.
struct Answer {
    status: u16,
    body: String,
}

impl Answer {
    fn ok<T: Serialize>(value: &T) -> Self {
        Self {
            status: 200,
            body: json(value),
        }
    }

    fn refuse(status: u16, code: &str, message: impl ToString) -> Self {
        let refusal = Refusal {
            error: code.into(),
            message: message.to_string(),
        };
        Self {
            status,
            body: json(&refusal),
        }
    }
}

fn json<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("values serialize")
}

fn now() -> u64 {
    ic_cdk::api::time() / 1_000_000_000
}

fn name_error(error: NameError) -> Answer {
    let (status, code) = match error {
        NameError::BadName => (400, "bad_name"),
        NameError::Taken => (409, "name_taken"),
        NameError::LookAlike(_) => (409, "look_alike_name"),
        NameError::Reserved => (409, "reserved_name"),
        NameError::UnknownName => (404, "unknown_name"),
        NameError::NotOwner => (403, "not_owner"),
        NameError::AlreadyNamed => (409, "already_named"),
    };
    Answer::refuse(status, code, error)
}

fn record_error(error: RecordError) -> Answer {
    let (status, code) = match error {
        RecordError::BadKey => (400, "bad_key"),
        RecordError::StaleVersion { .. } => (409, "stale_version"),
        RecordError::TooMany => (409, "too_many_records"),
        RecordError::UnknownRecord => (404, "unknown_record"),
    };
    Answer::refuse(status, code, error)
}

/// What a name's deposit is set aside under in the ledger.
fn name_deposit_id(name: &str) -> ContentHash {
    ContentHash::of(format!("decomp/name-deposit/v1\n{name}").as_bytes())
}

fn submit_op(state: &mut State, body: &[u8]) -> Answer {
    let Ok(envelope) = serde_json::from_slice::<Envelope>(body) else {
        return Answer::refuse(400, "bad_payload", "the body is not a signed envelope");
    };
    let at = now();
    let account = match state.accounts.apply_at(&envelope, at) {
        Ok(account) => account.clone(),
        Err(error) => return Answer::refuse(400, "op_refused", error),
    };
    state.ledger.open(account.id);
    state.created.entry(account.id).or_insert(at);
    state.logs.ops.push((json(&envelope), at));
    Answer::ok(&state.status(account))
}

fn request(state: &mut State, body: &[u8]) -> Answer {
    let Ok(envelope) = serde_json::from_slice::<Envelope>(body) else {
        return Answer::refuse(400, "bad_payload", "the body is not a signed envelope");
    };
    let Ok(Request { issued_at, call }) = envelope.open() else {
        return Answer::refuse(
            400,
            "bad_signature",
            "the request is not signed by its signer",
        );
    };
    let at = now();
    if at.abs_diff(issued_at) > MAX_CLOCK_SKEW_SECS {
        return Answer::refuse(
            400,
            "stale_request",
            "the request's issued_at is too far from the network's clock",
        );
    }

    // Anyone may resolve a name or read the directory, signed in or not.
    match &call {
        Call::ResolveName { name } => {
            return match state.registry.resolve(name) {
                Some(record) => Answer::ok(&state.resolved(record)),
                None => name_error(NameError::UnknownName),
            };
        }
        Call::ListDirectory {
            verified_only,
            query,
            from,
            limit,
        } => {
            let limit = limit.map_or(MAX_DIRECTORY_PAGE, |limit| limit as usize);
            let (records, next) =
                state
                    .registry
                    .page(*verified_only, query.as_deref(), from.as_deref(), limit);
            let entries = records
                .iter()
                .map(|record| state.resolved(record))
                .collect();
            return Answer::ok(&Directory { entries, next });
        }
        _ => {}
    }

    let signer: PublicKey = envelope.signer;
    let Some(account) = state.accounts.account_of(&signer).cloned() else {
        return Answer::refuse(404, "not_linked", "this key does not belong to an account");
    };
    if account.is_recovering(&signer) {
        return Answer::refuse(
            403,
            "recovery_waiting",
            "this device's recovery of the account has not taken effect yet",
        );
    }
    if !account.authorizes(&signer) {
        return Answer::refuse(
            403,
            "device_revoked",
            "this device has been revoked, or its recovery was cancelled",
        );
    }

    match call {
        Call::GetAccount => Answer::ok(&state.status(account)),
        // The signed request itself is kept, and handed to whoever asks, as
        // proof that the device chose the key.
        Call::PublishSealingKey { .. } => match state.sealing_keys.apply(&envelope) {
            Ok(changed) => {
                if changed {
                    state.logs.sealing_keys.push(json(&envelope));
                }
                Answer::ok(&serde_json::json!({}))
            }
            Err(error) => Answer::refuse(400, "bad_payload", error),
        },
        Call::GetSealingKey { device } => Answer::ok(&PublishedKey {
            proof: state.sealing_keys.proof(&device).cloned(),
        }),
        // Earnings and spending are recorded by the job board; this
        // canister holds only the balance.
        Call::GetActivity { .. } => Answer::ok(&Activity::default()),
        Call::RegisterName { name } => {
            if account.app.is_none() {
                return Answer::refuse(
                    400,
                    "no_app",
                    "only an account that has published an app can hold a name",
                );
            }
            if let Err(error) = state.registry.check(&name, &account.id) {
                return name_error(error);
            }
            let deposit = LedgerEntry::DataEscrow {
                id: name_deposit_id(&name),
                account: account.id,
                amount: NAME_DEPOSIT,
            };
            if let Err(answer) = state.record_ledger(deposit) {
                return answer;
            }
            state.note_registry(RegistryEntry::Register {
                name: name.clone(),
                app: account.id,
                at,
            });
            Answer::ok(state.registry.resolve(&name).expect("just registered"))
        }
        Call::RenewName { name } => match state.registry.owned(&name, &account.id) {
            Ok(_) => {
                state.note_registry(RegistryEntry::Renew {
                    name: name.clone(),
                    at,
                });
                Answer::ok(state.registry.resolve(&name).expect("still held"))
            }
            Err(error) => name_error(error),
        },
        Call::ReleaseName { name } => match state.registry.owned(&name, &account.id) {
            Ok(_) => {
                // A name assigned by a reviewer was never paid a deposit for.
                let _ = state.record_ledger(LedgerEntry::DataRefund {
                    id: name_deposit_id(&name),
                });
                state.note_registry(RegistryEntry::Release { name });
                Answer::ok(&serde_json::json!({}))
            }
            Err(error) => name_error(error),
        },
        Call::SetVerified { name, verified } => {
            if !state.reviewers.contains(&account.id) {
                return Answer::refuse(403, "not_reviewer", "only a reviewer may do this");
            }
            if state.registry.resolve(&name).is_none() {
                return name_error(NameError::UnknownName);
            }
            state.note_registry(RegistryEntry::SetVerified {
                name: name.clone(),
                verified,
            });
            Answer::ok(state.registry.resolve(&name).expect("still held"))
        }
        Call::AssignName { name, app } => {
            if !state.reviewers.contains(&account.id) {
                return Answer::refuse(403, "not_reviewer", "only a reviewer may do this");
            }
            if let Err(error) = check_syntax(&name) {
                return name_error(error);
            }
            if state.registry.resolve(&name).is_some() {
                return name_error(NameError::Taken);
            }
            if state
                .accounts
                .get(&app)
                .is_none_or(|owner| owner.app.is_none())
            {
                return Answer::refuse(
                    400,
                    "no_app",
                    "only an account that has published an app can hold a name",
                );
            }
            state.note_registry(RegistryEntry::Register {
                name: name.clone(),
                app,
                at,
            });
            Answer::ok(state.registry.resolve(&name).expect("just assigned"))
        }
        // The value is a blob kept by storage nodes; the record holds its hash
        // (spec, "Hashes, not data").
        Call::WriteRecord {
            key,
            version,
            value,
        } => match state
            .records
            .write(account.id, key, version, value, signer, at)
        {
            Ok(record) => {
                state.logs.records.push(json(&record));
                state.records.apply(record.clone());
                Answer::ok(&record)
            }
            Err(error) => record_error(error),
        },
        Call::GetRecord { owner, key } => {
            match state.records.get(&owner.unwrap_or(account.id), &key) {
                Some(record) => Answer::ok(record),
                None => record_error(RecordError::UnknownRecord),
            }
        }
        Call::ListRecords {
            owner,
            prefix,
            from,
        } => {
            let records =
                state
                    .records
                    .list(&owner.unwrap_or(account.id), &prefix, from.as_deref());
            Answer::ok(&RecordList { records })
        }
        _ => Answer::refuse(
            421,
            "other_canister",
            "this canister serves accounts, names, and records; ask the job board for jobs, storage, and relays",
        ),
    }
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

fn response(answer: Answer, upgrade: Option<bool>) -> HttpResponse {
    HttpResponse {
        status_code: answer.status,
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
        body: answer.body.into_bytes(),
        upgrade,
    }
}

/// Every request is passed on as an update call, whose answer consensus
/// certifies.
#[ic_cdk::query]
fn http_request(request: HttpRequest) -> HttpResponse {
    if request.method == "OPTIONS" {
        return response(Answer::ok(&serde_json::json!({})), None);
    }
    response(Answer::ok(&serde_json::json!({})), Some(true))
}

#[ic_cdk::update]
fn http_request_update(http: HttpRequest) -> HttpResponse {
    let path = http.url.split('?').next().unwrap_or_default().to_string();
    let answer = STATE.with_borrow_mut(|state| {
        let state = state.as_mut().expect("the canister is initialized");
        match (http.method.as_str(), path.as_str()) {
            ("POST", "/v1/ops") => submit_op(state, &http.body),
            ("POST", "/v1/request") => request(state, &http.body),
            _ => Answer::refuse(
                404,
                "not_found",
                "the calls are POST /v1/ops and POST /v1/request",
            ),
        }
    });
    response(answer, None)
}

#[ic_cdk::init]
fn init(settings: Option<Settings>) {
    let state = State::new(settings.unwrap_or_default());
    STATE.with_borrow_mut(|slot| *slot = Some(state));
}

#[ic_cdk::pre_upgrade]
fn pre_upgrade() {
    STATE.with_borrow_mut(|slot| {
        let state = slot.take().expect("the canister is initialized");
        ic_cdk::storage::stable_save((state.settings, state.logs)).expect("state saves");
    });
}

#[ic_cdk::post_upgrade]
fn post_upgrade() {
    let (settings, logs): (Settings, Logs) =
        ic_cdk::storage::stable_restore().expect("state restores");
    STATE.with_borrow_mut(|slot| *slot = Some(State::replay(settings, logs)));
}

/// A canister has no operating system to draw randomness from. Nothing on
/// this canister's path asks for any; if something did, it fails here
/// instead of drawing predictable bytes.
#[cfg(target_family = "wasm")]
#[unsafe(no_mangle)]
unsafe extern "Rust" fn __getrandom_v04_custom(
    _dest: *mut u8,
    _len: usize,
) -> Result<(), getrandom::Error> {
    Err(getrandom::Error::UNSUPPORTED)
}

ic_cdk::export_candid!();
