//! The coordinator's HTTP API. Every request body is a signed envelope; there are
//! no sessions or tokens, so who is asking is always proven by the signature.
//!
//! - `POST /v1/ops` takes an `AccountOp`: a change to an account, which is logged.
//! - `POST /v1/request` takes a `Request`: everything else.

use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use decomp_core::{
    Account, AccountStatus, Accounts, Addressed, Blob, BlobData, BlobRef, Call, ContentHash,
    Envelope, EnvelopeError, JobError, LedgerEntry, LedgerError, MAX_CLOCK_SKEW_SECS,
    MAX_MESSAGE_BYTES, OpError, Outcome, PrivacyError, PublicKey, PublishedKey, Refusal,
    RelayError, Request, ShardNow, ShardRef, SponsorError, StorageError, Storing, Submitted,
};
use decomp_sandbox::Sandbox;
use serde_json::json;
use tower_http::cors::CorsLayer;

use crate::referee::referee;
use crate::state::{Coordinator, InclusionError, OpenRelayError, StoreError, SubmitError};

struct App {
    coordinator: Mutex<Coordinator>,
    /// For refereeing disputes, which means running the disputed job.
    sandbox: Sandbox,
}

/// The coordinator's API, and the clock that posts a batch to the settlement
/// log every `batch_period`.
pub fn router(coordinator: Coordinator, batch_period: Duration) -> Router {
    let app = Arc::new(App {
        coordinator: Mutex::new(coordinator),
        sandbox: Sandbox::new(),
    });
    tokio::spawn(post_batches(Arc::clone(&app), batch_period));
    Router::new()
        .route("/v1/ops", post(submit_op))
        .route("/v1/request", post(request))
        .layer(DefaultBodyLimit::max(MAX_MESSAGE_BYTES))
        // Requests carry no cookies or credentials, so any web origin may call.
        .layer(CorsLayer::permissive())
        .with_state(app)
}

async fn post_batches(app: Arc<App>, period: Duration) {
    let mut ticks = tokio::time::interval(period);
    loop {
        ticks.tick().await;
        let mut coordinator = app.coordinator.lock().expect("coordinator lock");
        coordinator.post_batch(unix_now());
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after 1970")
        .as_secs()
}

async fn submit_op(
    State(app): State<Arc<App>>,
    Json(envelope): Json<Envelope>,
) -> Result<Json<AccountStatus>, ApiError> {
    let status = app
        .coordinator
        .lock()
        .expect("coordinator lock")
        .apply_op(&envelope)?;
    Ok(Json(status))
}

async fn request(
    State(app): State<Arc<App>>,
    Json(envelope): Json<Envelope>,
) -> Result<Response, ApiError> {
    let Request { issued_at, call } = envelope.open()?;
    let now = unix_now();
    if now.abs_diff(issued_at) > MAX_CLOCK_SKEW_SECS {
        let message = "the request's issued_at is too far from the coordinator's clock";
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "stale_request",
            message,
        ));
    }

    let (answer, cases) = {
        let mut coordinator = app.coordinator.lock().expect("coordinator lock");
        let account = member(&coordinator.accounts, &envelope.signer)?.clone();
        let answer = answer(
            &mut coordinator,
            &app.sandbox,
            call,
            &account,
            envelope,
            now,
        );
        // Any call can find that a job's deadline has passed, so settle after each.
        coordinator.settle(now);
        (answer, coordinator.take_cases())
    };
    // Refereeing runs the disputed job, which can take as long as the job did, so
    // it happens off to the side and the coordinator stays free meanwhile.
    for case in cases {
        let app = Arc::clone(&app);
        tokio::task::spawn_blocking(move || {
            let verdict = referee(&app.sandbox, &case);
            let mut coordinator = app.coordinator.lock().expect("coordinator lock");
            coordinator.resolve(&case, &verdict, unix_now());
        });
    }
    answer
}

fn answer(
    coordinator: &mut Coordinator,
    sandbox: &Sandbox,
    call: Call,
    account: &Account,
    envelope: Envelope,
    now: u64,
) -> Result<Response, ApiError> {
    let signer = envelope.signer;
    Ok(match call {
        Call::GetAccount => Json(coordinator.status(account.clone())).into_response(),
        Call::PutBlob { data } => Json(BlobRef {
            hash: coordinator.blobs.put(&data.0)?,
        })
        .into_response(),
        Call::GetBlob { hash } => {
            let bytes = coordinator
                .blobs
                .get(&hash)?
                .ok_or_else(|| unknown_blob(&hash))?;
            Json(BlobData { data: Blob(bytes) }).into_response()
        }
        Call::SubmitJob { manifest } => {
            let job_id = coordinator.submit_job(&envelope, account.id, manifest, now, sandbox)?;
            Json(Submitted { job_id }).into_response()
        }
        Call::GetJob { job_id } => {
            Json(coordinator.job_status(job_id, account.id, now)?).into_response()
        }
        Call::GetState { app, shard } => Json(ShardNow {
            contents: coordinator.shard_now(&ShardRef { app, shard }),
        })
        .into_response(),
        Call::Stake { amount } => {
            coordinator.record(LedgerEntry::Stake {
                account: account.id,
                amount,
            })?;
            Json(coordinator.status(account.clone())).into_response()
        }
        Call::Unstake { amount } => {
            // Stake must still be there when a job in progress turns out wrong.
            if coordinator.jobs.is_working(&account.id) {
                let message =
                    "stake cannot be taken back while one of the account's nodes is on a job";
                return Err(ApiError::new(StatusCode::CONFLICT, "node_working", message));
            }
            coordinator.record(LedgerEntry::Unstake {
                account: account.id,
                amount,
            })?;
            Json(coordinator.status(account.clone())).into_response()
        }
        Call::GetStanding => Json(coordinator.standing(&signer, &account.id)).into_response(),
        Call::CheckIn { address } => {
            if let Some(address) = &address {
                check_address(address)?;
            }
            Json(coordinator.check_in(signer, account.id, address, now)).into_response()
        }
        Call::StoreBlob {
            root,
            size,
            days,
            to,
        } => {
            let blob = Storing {
                root,
                size,
                owner: account.id,
                days,
                addressed: to.map(|to| Addressed { from: signer, to }),
            };
            Json(coordinator.store_blob(blob, now)?).into_response()
        }
        Call::ConfirmStored { root } => {
            coordinator.confirm_stored(root, signer, now)?;
            Json(json!({})).into_response()
        }
        Call::LocateBlob { root } => {
            let located = coordinator.locate(&root).ok_or(StorageError::UnknownBlob)?;
            Json(located).into_response()
        }
        Call::AnswerChallenge {
            challenge,
            chunk,
            proof,
        } => {
            coordinator.answer_challenge(signer, challenge, &chunk.0, &proof, now)?;
            Json(json!({})).into_response()
        }
        Call::PublishSealingKey { .. } => {
            coordinator.publish_sealing_key(&envelope)?;
            Json(json!({})).into_response()
        }
        Call::GetSealingKey { device } => Json(PublishedKey {
            proof: coordinator.sealing_key_proof(&device),
        })
        .into_response(),
        Call::OpenRelay { to, bytes } => {
            let payload = &envelope.payload;
            Json(coordinator.open_relay(signer, account.id, payload, to, bytes, now)?)
                .into_response()
        }
        Call::ListIncoming => Json(coordinator.incoming(&signer, now)).into_response(),
        Call::RedeemRelay { header, receipt } => {
            coordinator.redeem_relay(&signer, &header, &receipt)?;
            Json(json!({})).into_response()
        }
        Call::GetLog { from } => Json(coordinator.log_page(from)?).into_response(),
        Call::GetInclusion { job_id } => Json(coordinator.inclusion(&job_id)?).into_response(),
        Call::SubmitResult { result } => {
            if let Outcome::Completed {
                output,
                blobs,
                state,
                spawned,
            } = &result.outcome
            {
                let asked = spawned
                    .iter()
                    .flat_map(|spawn| [&spawn.module, &spawn.input]);
                for hash in blobs.iter().chain([output]).chain(state).chain(asked) {
                    require_blob(coordinator, hash)?;
                }
            }
            require_blob(coordinator, &result.checkpoints)?;
            coordinator
                .jobs
                .report(signer, result, envelope, now, &mut rand::rng())?;
            Json(json!({})).into_response()
        }
    })
}

/// The account `signer` may act for. Only approved devices (and the account key)
/// get past this, so every request below it comes from a member of an account.
fn member<'a>(accounts: &'a Accounts, signer: &PublicKey) -> Result<&'a Account, ApiError> {
    let account = accounts.account_of(signer).ok_or_else(|| {
        let message = "this key does not belong to an account";
        ApiError::new(StatusCode::NOT_FOUND, "not_linked", message)
    })?;
    if !account.authorizes(signer) {
        let message = "this device has been revoked";
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "device_revoked",
            message,
        ));
    }
    Ok(account)
}

/// The longest address a node may declare.
const MAX_ADDRESS_LENGTH: usize = 200;

/// Refuses an address that is not a web address other devices could use.
fn check_address(address: &str) -> Result<(), ApiError> {
    let is_web = address.starts_with("http://") || address.starts_with("https://");
    if is_web && address.len() <= MAX_ADDRESS_LENGTH {
        return Ok(());
    }
    let message = "a node's address must start with http:// or https://";
    Err(ApiError::new(
        StatusCode::BAD_REQUEST,
        "bad_address",
        message,
    ))
}

fn require_blob(coordinator: &Coordinator, hash: &ContentHash) -> Result<(), ApiError> {
    if coordinator.blobs.contains(hash) {
        Ok(())
    } else {
        Err(unknown_blob(hash))
    }
}

fn unknown_blob(hash: &ContentHash) -> ApiError {
    let message = format!("no blob with hash {hash} has been stored");
    ApiError::new(StatusCode::NOT_FOUND, "unknown_blob", message)
}

/// An error answer: an HTTP status and a `Refusal` body.
struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = Refusal {
            error: self.code.into(),
            message: self.message,
        };
        (self.status, Json(body)).into_response()
    }
}

impl From<EnvelopeError> for ApiError {
    fn from(error: EnvelopeError) -> Self {
        let code = match error {
            EnvelopeError::BadSignature => "bad_signature",
            EnvelopeError::BadPayload(_) => "bad_payload",
        };
        Self::new(StatusCode::BAD_REQUEST, code, error.to_string())
    }
}

impl From<OpError> for ApiError {
    fn from(error: OpError) -> Self {
        let (status, code) = match error {
            OpError::Envelope(inner) => return inner.into(),
            OpError::AccountExists => (StatusCode::CONFLICT, "account_exists"),
            OpError::UnknownAccount => (StatusCode::NOT_FOUND, "unknown_account"),
            OpError::StaleSeq { .. } => (StatusCode::CONFLICT, "stale_seq"),
            OpError::NotAuthorized => (StatusCode::FORBIDDEN, "not_authorized"),
            OpError::DeviceTaken => (StatusCode::CONFLICT, "device_taken"),
            OpError::UnknownDevice => (StatusCode::NOT_FOUND, "unknown_device"),
            OpError::AlreadyRevoked => (StatusCode::CONFLICT, "already_revoked"),
            OpError::BadApp(_) => (StatusCode::BAD_REQUEST, "bad_app"),
        };
        Self::new(status, code, error.to_string())
    }
}

impl From<JobError> for ApiError {
    fn from(error: JobError) -> Self {
        let (status, code) = match error {
            JobError::JobExists => (StatusCode::CONFLICT, "job_exists"),
            JobError::NoNodes => (StatusCode::BAD_REQUEST, "no_nodes"),
            JobError::DeadlinePassed => (StatusCode::BAD_REQUEST, "deadline_passed"),
            JobError::NoComputeCap => (StatusCode::BAD_REQUEST, "no_compute_cap"),
            JobError::UnknownJob => (StatusCode::NOT_FOUND, "unknown_job"),
            JobError::JobClosed => (StatusCode::CONFLICT, "job_closed"),
            JobError::NotAssigned => (StatusCode::FORBIDDEN, "not_assigned"),
            JobError::AlreadyReported => (StatusCode::CONFLICT, "already_reported"),
            JobError::UnitsOverCap => (StatusCode::BAD_REQUEST, "units_over_cap"),
        };
        Self::new(status, code, error.to_string())
    }
}

impl From<LedgerError> for ApiError {
    fn from(error: LedgerError) -> Self {
        let (status, code) = match error {
            LedgerError::InsufficientCredits => {
                (StatusCode::PAYMENT_REQUIRED, "insufficient_credits")
            }
            LedgerError::InsufficientEarnedCredits => {
                (StatusCode::PAYMENT_REQUIRED, "insufficient_earned_credits")
            }
            LedgerError::InsufficientStake => (StatusCode::CONFLICT, "insufficient_stake"),
            LedgerError::UnknownAccount => (StatusCode::NOT_FOUND, "unknown_account"),
            LedgerError::AlreadyEscrowed => (StatusCode::CONFLICT, "job_exists"),
            LedgerError::NothingEscrowed | LedgerError::FeeExceedsEscrow => {
                (StatusCode::INTERNAL_SERVER_ERROR, "ledger_failed")
            }
        };
        Self::new(status, code, error.to_string())
    }
}

impl From<SubmitError> for ApiError {
    fn from(error: SubmitError) -> Self {
        let message = error.to_string();
        match error {
            SubmitError::Job(inner) => inner.into(),
            SubmitError::Ledger(inner) => inner.into(),
            SubmitError::Storage(inner) => inner.into(),
            SubmitError::UnknownBlob(_) => {
                Self::new(StatusCode::NOT_FOUND, "unknown_blob", message)
            }
            SubmitError::FeeCapTooLow(_) => {
                Self::new(StatusCode::BAD_REQUEST, "over_fee_cap", message)
            }
            SubmitError::TypeMismatch(_) | SubmitError::TypedJobSplitByBytes => {
                Self::new(StatusCode::BAD_REQUEST, "type_mismatch", message)
            }
            SubmitError::TooManyPieces => {
                Self::new(StatusCode::BAD_REQUEST, "too_many_pieces", message)
            }
            SubmitError::TooManyReads => {
                Self::new(StatusCode::BAD_REQUEST, "too_many_reads", message)
            }
            SubmitError::KeptFromTooMany => {
                Self::new(StatusCode::BAD_REQUEST, "kept_from_too_many", message)
            }
            SubmitError::StateNotAllowed => {
                Self::new(StatusCode::FORBIDDEN, "state_not_allowed", message)
            }
            SubmitError::BadShardName | SubmitError::SplitWithState => {
                Self::new(StatusCode::BAD_REQUEST, "bad_state", message)
            }
            SubmitError::Privacy(inner) => {
                let code = match inner {
                    PrivacyError::Mismatch => "privacy_mismatch",
                    PrivacyError::NoRoute(_) => "no_private_route",
                    PrivacyError::State => "bad_state",
                };
                Self::new(StatusCode::BAD_REQUEST, code, message)
            }
            SubmitError::Sponsor(inner) => {
                let (status, code) = match inner {
                    SponsorError::UnknownApp => (StatusCode::NOT_FOUND, "unknown_app"),
                    SponsorError::NotSponsoring | SponsorError::NotItsModule => {
                        (StatusCode::FORBIDDEN, "not_sponsored")
                    }
                    SponsorError::OverDailyCap => {
                        (StatusCode::PAYMENT_REQUIRED, "over_sponsor_cap")
                    }
                };
                Self::new(status, code, message)
            }
        }
    }
}

impl From<StorageError> for ApiError {
    fn from(error: StorageError) -> Self {
        let (status, code) = match error {
            StorageError::AlreadyStored => (StatusCode::CONFLICT, "already_stored"),
            StorageError::BadDuration => (StatusCode::BAD_REQUEST, "bad_duration"),
            StorageError::NotEnoughNodes => (StatusCode::SERVICE_UNAVAILABLE, "not_enough_nodes"),
            StorageError::UnknownBlob => (StatusCode::NOT_FOUND, "unknown_blob"),
            StorageError::NotAsked => (StatusCode::FORBIDDEN, "not_asked"),
            StorageError::NotChallenged => (StatusCode::CONFLICT, "not_challenged"),
        };
        Self::new(status, code, error.to_string())
    }
}

impl From<RelayError> for ApiError {
    fn from(error: RelayError) -> Self {
        let (status, code) = match error {
            RelayError::ChannelExists => (StatusCode::CONFLICT, "channel_exists"),
            RelayError::BadSize => (StatusCode::BAD_REQUEST, "bad_size"),
            RelayError::NoRelay => (StatusCode::SERVICE_UNAVAILABLE, "no_relay"),
            RelayError::ReceiverAway => (StatusCode::CONFLICT, "receiver_away"),
            RelayError::UnknownChannel => (StatusCode::NOT_FOUND, "unknown_channel"),
            RelayError::NotTheRelay => (StatusCode::FORBIDDEN, "not_the_relay"),
            RelayError::NotFromSender => (StatusCode::BAD_REQUEST, "not_from_sender"),
            RelayError::OverBudget => (StatusCode::BAD_REQUEST, "over_budget"),
            RelayError::NotCollected => (StatusCode::BAD_REQUEST, "not_collected"),
        };
        Self::new(status, code, error.to_string())
    }
}

impl From<OpenRelayError> for ApiError {
    fn from(error: OpenRelayError) -> Self {
        let message = error.to_string();
        match error {
            OpenRelayError::Relay(inner) => inner.into(),
            OpenRelayError::Ledger(inner) => inner.into(),
            OpenRelayError::UnknownDevice => {
                Self::new(StatusCode::NOT_FOUND, "unknown_device", message)
            }
        }
    }
}

impl From<StoreError> for ApiError {
    fn from(error: StoreError) -> Self {
        let message = error.to_string();
        match error {
            StoreError::Storage(inner) => inner.into(),
            StoreError::Ledger(inner) => inner.into(),
            StoreError::UnknownDevice => {
                Self::new(StatusCode::NOT_FOUND, "unknown_device", message)
            }
        }
    }
}

impl From<InclusionError> for ApiError {
    fn from(error: InclusionError) -> Self {
        let message = error.to_string();
        match error {
            InclusionError::NoReceipt => {
                Self::new(StatusCode::NOT_FOUND, "unknown_receipt", message)
            }
            InclusionError::NotBatched => Self::new(StatusCode::CONFLICT, "not_batched", message),
            InclusionError::Storage(inner) => inner.into(),
        }
    }
}

impl From<io::Error> for ApiError {
    fn from(error: io::Error) -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "storage_failed",
            error.to_string(),
        )
    }
}
