//! The control plane's interface (spec, "Interfaces"), independent of how it
//! is served: the devnet serves it over HTTP, and a canister over the
//! Internet Computer's HTTP gateway. Every request body is a signed envelope;
//! there are no sessions or tokens, so who is asking is always proven by the
//! signature.
//!
//! - `POST /v1/ops` takes an `AccountOp`: a change to an account, which is logged.
//! - `POST /v1/request` takes a `Request`: everything else.

use std::io;

use decomp_core::registry::MAX_DIRECTORY_PAGE;
use decomp_core::{
    Account, AccountId, Accounts, Addressed, BidError, Bids, Blob, BlobData, BlobRef, Call,
    CashOutAsked, CashOuts, ContentHash, Envelope, EnvelopeError, JobError, LedgerEntry,
    LedgerError, MAX_CLOCK_SKEW_SECS, MAX_VALUE_KEY_LABEL_BYTES, NameError, OpError, Outcome,
    PrivacyError, PublicKey, PublishedKey, Rates, RecordError, RecordList, Refusal, RelayError,
    Request, ShardNow, ShardRef, SponsorError, StorageError, Storing, Submitted, value_key_input,
};
use serde::Serialize;
use serde_json::json;

use crate::state::{
    ControlPlane, InclusionError, OpenRelayError, RegistryError, StoreError, SubmitError,
};

/// An HTTP status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatusCode(pub u16);

impl StatusCode {
    pub const OK: Self = Self(200);
    pub const BAD_REQUEST: Self = Self(400);
    pub const PAYMENT_REQUIRED: Self = Self(402);
    pub const FORBIDDEN: Self = Self(403);
    pub const NOT_FOUND: Self = Self(404);
    pub const CONFLICT: Self = Self(409);
    pub const TOO_MANY_REQUESTS: Self = Self(429);
    pub const INTERNAL_SERVER_ERROR: Self = Self(500);
    pub const NOT_IMPLEMENTED: Self = Self(501);
    pub const SERVICE_UNAVAILABLE: Self = Self(503);
}

/// An answer: a status and a JSON body.
pub struct Response {
    pub status: StatusCode,
    pub body: Vec<u8>,
}

/// A value answered as JSON with status 200.
struct Json<T>(T);

impl<T: Serialize> Json<T> {
    fn into_response(self) -> Response {
        Response {
            status: StatusCode::OK,
            body: serde_json::to_vec(&self.0).expect("answers serialize"),
        }
    }
}

/// Answers a request for `path`, with `body` as it arrived, at time `now`.
/// After it, the host settles the control plane and runs any disputes and
/// fallbacks it has queued (`take_cases`, `take_fall_backs`).
pub fn handle(control: &mut ControlPlane, path: &str, body: &[u8], now: u64) -> Response {
    let Ok(envelope) = serde_json::from_slice::<Envelope>(body) else {
        let message = "the body is not a signed envelope";
        return ApiError::new(StatusCode::BAD_REQUEST, "bad_payload", message).into_response();
    };
    let answer = match path {
        "/v1/ops" => control
            .apply_op(&envelope, now)
            .map(|status| Json(status).into_response())
            .map_err(ApiError::from),
        "/v1/request" => request(control, envelope, now),
        _ => Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "the calls are POST /v1/ops and POST /v1/request",
        )),
    };
    answer.unwrap_or_else(ApiError::into_response)
}

fn request(control: &mut ControlPlane, envelope: Envelope, now: u64) -> Result<Response, ApiError> {
    let Request { issued_at, call } = envelope.open()?;
    check_fresh(issued_at, now)?;

    match call {
        // Anyone may resolve a name, read the directory, or fetch a blob by its
        // hash or find the nodes that hold it, signed in or not (spec, "Blobs":
        // "Any client can compute where a chunk lives").
        Call::ResolveName { name } => control
            .resolve_name(&name)
            .map(|resolved| Json(resolved).into_response())
            .map_err(ApiError::from),
        Call::ListDirectory {
            verified_only,
            query,
            from,
            limit,
        } => {
            let limit = limit.map_or(MAX_DIRECTORY_PAGE, |limit| limit as usize);
            let page = control.directory(verified_only, query.as_deref(), from.as_deref(), limit);
            Ok(Json(page).into_response())
        }
        Call::LocateBlob { root } => control
            .locate(&root)
            .map(|located| Json(located).into_response())
            .ok_or_else(|| ApiError::from(StorageError::UnknownBlob)),
        Call::GetBlob { hash } => control
            .blobs
            .get(&hash)
            .map_err(ApiError::from)
            .and_then(|bytes| bytes.ok_or_else(|| unknown_blob(&hash)))
            .map(|bytes| Json(BlobData { data: Blob(bytes) }).into_response()),
        call => {
            let account = member(&control.accounts, &envelope.signer)?.clone();
            answer(control, call, &account, envelope, now)
        }
    }
}

fn check_fresh(issued_at: u64, now: u64) -> Result<(), ApiError> {
    if now.abs_diff(issued_at) > MAX_CLOCK_SKEW_SECS {
        let message = "the request's issued_at is too far from the control plane's clock";
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "stale_request",
            message,
        ));
    }
    Ok(())
}

/// A request for a value key that has been checked: signed by a device of
/// `account`, fresh, and with a label that is not too long.
pub struct ValueKeyRequest {
    pub account: AccountId,
    /// What the threshold key derives the key from (`value_key_input`).
    pub input: Vec<u8>,
    pub transport_key: Vec<u8>,
}

/// If the request for `path` asks for a value key, checks it. The host
/// derives the key with its threshold key and answers with a `ValueKey`; a
/// host without one lets `handle` answer that it has none.
pub fn value_key_request(
    control: &ControlPlane,
    path: &str,
    body: &[u8],
    now: u64,
) -> Option<Result<ValueKeyRequest, Response>> {
    if path != "/v1/request" {
        return None;
    }
    let envelope = serde_json::from_slice::<Envelope>(body).ok()?;
    let Request { issued_at, call } = envelope.open().ok()?;
    let Call::GetValueKey {
        label,
        transport_key,
    } = call
    else {
        return None;
    };
    let checked = (|| {
        check_fresh(issued_at, now)?;
        if label.len() > MAX_VALUE_KEY_LABEL_BYTES {
            let message = format!("a label is at most {MAX_VALUE_KEY_LABEL_BYTES} bytes");
            return Err(ApiError::new(StatusCode::BAD_REQUEST, "bad_label", message));
        }
        let account = member(&control.accounts, &envelope.signer)?.id;
        Ok(ValueKeyRequest {
            account,
            input: value_key_input(&account, &label),
            transport_key: transport_key.0,
        })
    })();
    Some(checked.map_err(ApiError::into_response))
}

fn answer(
    control: &mut ControlPlane,
    call: Call,
    account: &Account,
    envelope: Envelope,
    now: u64,
) -> Result<Response, ApiError> {
    let signer = envelope.signer;
    Ok(match call {
        Call::GetAccount => Json(control.status(account.clone())).into_response(),
        Call::PutBlob { data } => Json(BlobRef {
            hash: control.blobs.put(&data.0)?,
        })
        .into_response(),
        Call::SubmitJob { manifest } => {
            let job_id = control.submit_job(&envelope, account.id, manifest, now)?;
            Json(Submitted { job_id }).into_response()
        }
        Call::GetReceipt { job_id } => {
            Json(control.job_status(job_id, account.id, now)?).into_response()
        }
        Call::QuoteJob { manifest } => Json(control.quote(&manifest)?).into_response(),
        Call::WriteRecord {
            key,
            version,
            value,
        } => {
            require_blob(control, &value)?;
            let record = control.write_record(account.id, key, version, value, signer, now)?;
            Json(record).into_response()
        }
        Call::GetRecord { owner, key } => {
            let owner = owner.unwrap_or(account.id);
            Json(control.get_record(&owner, &key)?).into_response()
        }
        Call::ListRecords {
            owner,
            prefix,
            from,
        } => {
            let owner = owner.unwrap_or(account.id);
            Json(RecordList {
                records: control.list_records(&owner, &prefix, from.as_deref()),
            })
            .into_response()
        }
        Call::GetState { app, shard } => Json(ShardNow {
            contents: control.shard_now(&ShardRef { app, shard }),
        })
        .into_response(),
        Call::Stake { amount } => {
            control.record(LedgerEntry::Stake {
                account: account.id,
                amount,
            })?;
            Json(control.status(account.clone())).into_response()
        }
        Call::Unstake { amount } => {
            // Stake must still be there when a job in progress turns out wrong.
            if control.jobs.is_working(&account.id) {
                let message =
                    "stake cannot be taken back while one of the account's nodes is on a job";
                return Err(ApiError::new(StatusCode::CONFLICT, "node_working", message));
            }
            control.record(LedgerEntry::Unstake {
                account: account.id,
                amount,
            })?;
            Json(control.status(account.clone())).into_response()
        }
        Call::GetStanding => Json(control.standing(&signer, &account.id)).into_response(),
        Call::RequestLease { address } => {
            if let Some(address) = &address {
                check_address(address)?;
            }
            Json(control.request_lease(signer, account.id, address, now)).into_response()
        }
        Call::CommitResult { job_id, commitment } => {
            control.commit_result(signer, job_id, commitment, now)?;
            Json(json!({})).into_response()
        }
        Call::RevealResult { result, salt } => {
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
                    require_blob(control, hash)?;
                }
            }
            control.reveal_result(signer, result, salt, envelope, now)?;
            Json(json!({})).into_response()
        }
        Call::GetWorkPreferences { device } => {
            Json(control.preferences(&account.id, &device)?).into_response()
        }
        Call::SetWorkPreferences {
            device,
            preferences,
        } => Json(control.set_preferences(&account.id, device, preferences)?).into_response(),
        Call::PlaceBid { bid } => {
            Json(control.place_bid(account.id, bid, &envelope, now)?).into_response()
        }
        Call::CancelBid { id } => {
            control.cancel_bid(account.id, id)?;
            Json(json!({})).into_response()
        }
        Call::ListBids => Json(Bids {
            bids: control.jobs.bids_of(&account.id, now),
        })
        .into_response(),
        Call::GetRates => Json(Rates {
            rates: control.jobs.rates(),
        })
        .into_response(),
        Call::GetActivity { since } => Json(control.activity(&account.id, since)).into_response(),
        Call::RegisterName { name } => {
            Json(control.register_name(account, name, now)?).into_response()
        }
        Call::RenewName { name } => {
            Json(control.renew_name(&account.id, name, now)?).into_response()
        }
        Call::ReleaseName { name } => {
            control.release_name(&account.id, name)?;
            Json(json!({})).into_response()
        }
        Call::SetVerified { name, verified } => {
            Json(control.set_verified(&account.id, name, verified)?).into_response()
        }
        Call::AssignName { name, app } => {
            Json(control.assign_name(&account.id, name, app, now)?).into_response()
        }
        Call::ResolveName { .. }
        | Call::ListDirectory { .. }
        | Call::GetBlob { .. }
        | Call::LocateBlob { .. } => {
            unreachable!("answered before membership is checked")
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
            Json(control.store_blob(blob, now)?).into_response()
        }
        Call::StoreProduced { job_id, root, size } => {
            Json(control.store_produced(&signer, job_id, root, size, now)?).into_response()
        }
        Call::ConfirmStored { root } => {
            control.confirm_stored(root, signer, now)?;
            Json(json!({})).into_response()
        }
        Call::DeleteBlob { root } => {
            control.delete_blob(&root, &account.id)?;
            Json(json!({})).into_response()
        }
        Call::AnswerChallenge {
            challenge,
            chunk,
            proof,
        } => {
            control.answer_challenge(signer, challenge, &chunk.0, &proof, now)?;
            Json(json!({})).into_response()
        }
        Call::PublishSealingKey { .. } => {
            control.publish_sealing_key(&envelope)?;
            Json(json!({})).into_response()
        }
        Call::GetValueKey { .. } => {
            let message = "value keys come from the network's threshold key, which only the                 control canister has; run the control plane as a canister";
            return Err(ApiError::new(
                StatusCode::NOT_IMPLEMENTED,
                "no_threshold_key",
                message,
            ));
        }
        Call::GetSealingKey { device } => Json(PublishedKey {
            proof: control.sealing_key_proof(&device),
        })
        .into_response(),
        Call::OpenRelay { to, bytes } => {
            let payload = &envelope.payload;
            Json(control.open_relay(signer, account.id, payload, to, bytes, now)?).into_response()
        }
        Call::ListIncoming => Json(control.incoming(&signer, now)).into_response(),
        Call::RedeemRelay { header, receipt } => {
            control.redeem_relay(&signer, &header, &receipt)?;
            Json(json!({})).into_response()
        }
        Call::GetLog { from } => Json(control.log_page(from)?).into_response(),
        Call::GetInclusion { job_id } => Json(control.inclusion(&job_id)?).into_response(),
        Call::GetDraws { from } => Json(control.draws_page(from)?).into_response(),
        Call::CreditPurchase {
            account: buyer,
            amount,
            payment,
        } => {
            treasury_only(control, &signer)?;
            control.record(LedgerEntry::Buy {
                account: buyer,
                amount,
                payment,
            })?;
            Json(json!({})).into_response()
        }
        Call::CashOut { amount } => {
            if signer != account.account_key {
                let message = "a cash-out must be signed by the account key itself";
                return Err(ApiError::new(
                    StatusCode::FORBIDDEN,
                    "not_account_key",
                    message,
                ));
            }
            // The request's signature names the cash-out, so the same request
            // sent twice asks for one cash-out.
            let id = ContentHash::of(envelope.signature.as_bytes());
            control.record(LedgerEntry::CashOut {
                id,
                account: account.id,
                amount,
            })?;
            Json(CashOutAsked { id }).into_response()
        }
        Call::GetCashOuts => {
            treasury_only(control, &signer)?;
            let waiting = control.cash_outs();
            Json(CashOuts { waiting }).into_response()
        }
        Call::PayCashOut { id, payout } => {
            treasury_only(control, &signer)?;
            control.record(LedgerEntry::CashOutPaid { id, payout })?;
            Json(json!({})).into_response()
        }
        Call::RefuseCashOut { id } => {
            treasury_only(control, &signer)?;
            control.record(LedgerEntry::CashOutRefused { id })?;
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
    if account.is_recovering(signer) {
        let message = "this device's recovery of the account has not taken effect yet";
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "recovery_waiting",
            message,
        ));
    }
    if !account.authorizes(signer) {
        let message = "this device has been revoked, or its recovery was cancelled";
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "device_revoked",
            message,
        ));
    }
    Ok(account)
}

/// Refuses a request that only the treasury may make: saying that credits were
/// paid for, and paying or refusing a cash-out.
fn treasury_only(control: &ControlPlane, signer: &PublicKey) -> Result<(), ApiError> {
    if *signer == control.control_key() {
        return Ok(());
    }
    let message = "only the treasury may do this";
    Err(ApiError::new(
        StatusCode::FORBIDDEN,
        "not_treasury",
        message,
    ))
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

/// Refuses a hash that names no blob: one neither placed on storage nodes nor
/// kept by the control plane for always-on work.
fn require_blob(control: &ControlPlane, hash: &ContentHash) -> Result<(), ApiError> {
    if control.locate(hash).is_some() || control.blobs.contains(hash) {
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

impl ApiError {
    fn into_response(self) -> Response {
        let body = Refusal {
            error: self.code.into(),
            message: self.message,
        };
        Response {
            status: self.status,
            body: serde_json::to_vec(&body).expect("refusals serialize"),
        }
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
            OpError::NotAccountKey => (StatusCode::FORBIDDEN, "not_account_key"),
            OpError::BadControllers(_) => (StatusCode::BAD_REQUEST, "bad_controllers"),
            OpError::NotApproved => (StatusCode::FORBIDDEN, "not_approved"),
            OpError::TooManyRecoveries => (StatusCode::CONFLICT, "too_many_recoveries"),
            OpError::NoSuchRecovery => (StatusCode::NOT_FOUND, "unknown_recovery"),
            OpError::RecoveryWaiting { .. } => (StatusCode::CONFLICT, "recovery_waiting"),
            OpError::WrongTime => (StatusCode::BAD_REQUEST, "stale_request"),
        };
        Self::new(status, code, error.to_string())
    }
}

impl From<JobError> for ApiError {
    fn from(error: JobError) -> Self {
        let message = error.to_string();
        let (status, code) = match &error {
            JobError::JobExists => (StatusCode::CONFLICT, "job_exists"),
            JobError::DeadlinePassed => (StatusCode::BAD_REQUEST, "deadline_passed"),
            JobError::NoComputeCap => (StatusCode::BAD_REQUEST, "no_compute_cap"),
            JobError::BadSlots => (StatusCode::BAD_REQUEST, "bad_slots"),
            JobError::UnknownJob => (StatusCode::NOT_FOUND, "unknown_job"),
            JobError::NotTakingResults => (StatusCode::CONFLICT, "not_taking_results"),
            JobError::NotLeased => (StatusCode::FORBIDDEN, "not_leased"),
            JobError::AlreadyCommitted => (StatusCode::CONFLICT, "already_committed"),
            JobError::NotTheCommitment => (StatusCode::BAD_REQUEST, "not_the_commitment"),
            JobError::UnitsOverCap => (StatusCode::BAD_REQUEST, "units_over_cap"),
            JobError::BadPreferences(_) => (StatusCode::BAD_REQUEST, "bad_preferences"),
            JobError::Bid(inner) => {
                let code = match inner {
                    BidError::UnknownBid => "unknown_bid",
                    BidError::TooManyBids => "too_many_bids",
                    _ => "bad_bid",
                };
                (StatusCode::BAD_REQUEST, code)
            }
        };
        Self::new(status, code, message)
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
            LedgerError::TooFewToCashOut => {
                (StatusCode::PAYMENT_REQUIRED, "insufficient_earned_credits")
            }
            LedgerError::NoAmount => (StatusCode::BAD_REQUEST, "no_amount"),
            LedgerError::PaymentCredited => (StatusCode::CONFLICT, "payment_credited"),
            LedgerError::BadPaymentName => (StatusCode::BAD_REQUEST, "bad_payment_name"),
            LedgerError::CashOutExists => (StatusCode::CONFLICT, "cash_out_exists"),
            LedgerError::UnknownCashOut => (StatusCode::NOT_FOUND, "unknown_cash_out"),
            LedgerError::NothingEscrowed
            | LedgerError::FeeExceedsEscrow
            | LedgerError::PayoutsDoNotAddUp => {
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
            SubmitError::NotTakingJobs => {
                Self::new(StatusCode::SERVICE_UNAVAILABLE, "not_taking_jobs", message)
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
            StorageError::NotOwner => (StatusCode::FORBIDDEN, "not_owner"),
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
            StoreError::NotLeased => Self::new(StatusCode::FORBIDDEN, "not_leased", message),
            StoreError::NoPayer => Self::new(StatusCode::FORBIDDEN, "no_payer", message),
        }
    }
}

impl From<RecordError> for ApiError {
    fn from(error: RecordError) -> Self {
        let message = error.to_string();
        let (status, code) = match error {
            RecordError::BadKey => (StatusCode::BAD_REQUEST, "bad_key"),
            RecordError::StaleVersion { .. } => (StatusCode::CONFLICT, "stale_version"),
            RecordError::TooMany => (StatusCode::CONFLICT, "too_many_records"),
            RecordError::UnknownRecord => (StatusCode::NOT_FOUND, "unknown_record"),
        };
        Self::new(status, code, message)
    }
}

impl From<RegistryError> for ApiError {
    fn from(error: RegistryError) -> Self {
        let message = error.to_string();
        let (status, code) = match error {
            RegistryError::Ledger(inner) => return inner.into(),
            RegistryError::NoApp => (StatusCode::BAD_REQUEST, "no_app"),
            RegistryError::NotReviewer => (StatusCode::FORBIDDEN, "not_reviewer"),
            RegistryError::Name(inner) => match inner {
                NameError::BadName => (StatusCode::BAD_REQUEST, "bad_name"),
                NameError::Taken => (StatusCode::CONFLICT, "name_taken"),
                NameError::LookAlike(_) => (StatusCode::CONFLICT, "look_alike_name"),
                NameError::Reserved => (StatusCode::CONFLICT, "reserved_name"),
                NameError::UnknownName => (StatusCode::NOT_FOUND, "unknown_name"),
                NameError::NotOwner => (StatusCode::FORBIDDEN, "not_owner"),
                NameError::AlreadyNamed => (StatusCode::CONFLICT, "already_named"),
            },
        };
        Self::new(status, code, message)
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
