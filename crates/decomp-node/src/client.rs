//! Client for the control plane's request endpoint, signing as this device.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use decomp_core::LogError;
use decomp_core::{
    AccountId, AccountOp, AccountStatus, Activity, AppManifest, Approval, Bid, BidOrder, Bids,
    Blob, BlobData, BlobRef, Call, CashOut, CashOutAsked, CashOuts, Challenge, ContentHash,
    Credits, Directory, DrawPage, Envelope, Inclusion, IncomingChannels, JobId, JobResult,
    JobStatus, LeasesGranted, Located, LogPage, MAX_LOG_PAGE, MAX_MESSAGE_BYTES, Manifest,
    NameRecord, Placement, PublicKey, PublishedKey, Quote, Rates, Refusal, Request, Resolved,
    Route, SealingKey, SettlementLog, ShardNow, SigningKey, Standing, Submitted, WorkPreferences,
    seal,
};
use serde::de::{DeserializeOwned, IgnoredAny};

/// How long one request may take, including sending or receiving a large blob.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The control plane understood the request and said no.
    #[error("{message}")]
    Refused { code: String, message: String },
    #[error("could not reach the control plane: {0}")]
    Unreachable(String),
    #[error("the control plane sent a blob that does not match its hash")]
    CorruptBlob,
    #[error("entry {0} of the settlement log does not hold: {1}")]
    BadLog(u64, LogError),
}

impl ClientError {
    /// Whether the coordinator refused with this error code.
    pub fn is(&self, expected: &str) -> bool {
        matches!(self, Self::Refused { code, .. } if code == expected)
    }
}

pub struct Client {
    /// Where requests go, and where changes to an account go.
    endpoint: String,
    ops_endpoint: String,
    key: SigningKey,
    http: ureq::Agent,
}

impl Client {
    pub fn new(control_url: &str, key: SigningKey) -> Self {
        let http = ureq::Agent::config_builder()
            // Read error bodies ourselves: they carry the coordinator's reason.
            .http_status_as_error(false)
            .timeout_global(Some(REQUEST_TIMEOUT))
            .build()
            .new_agent();
        // An address may carry a query, as a canister's does on a gateway
        // that names it as `?canisterId=...`; it stays on every request.
        let (base, query) = match control_url.split_once('?') {
            Some((base, query)) => (base, format!("?{query}")),
            None => (control_url, String::new()),
        };
        let base = base.trim_end_matches('/');
        Self {
            endpoint: format!("{base}/v1/request{query}"),
            ops_endpoint: format!("{base}/v1/ops{query}"),
            key,
            http,
        }
    }

    pub fn device_key(&self) -> PublicKey {
        self.key.public_key()
    }

    /// This device's key, for signing what goes to other devices and to nodes
    /// rather than to the coordinator.
    pub fn key(&self) -> &SigningKey {
        &self.key
    }

    pub fn account(&self) -> Result<AccountStatus, ClientError> {
        self.call(Call::GetAccount)
    }

    /// Publishes this device's account's app, or replaces the one it has
    /// published. The account's ID is the app's ID.
    pub fn publish_app(&self, app: AppManifest) -> Result<AccountStatus, ClientError> {
        let account = self.account()?.account;
        let op = AccountOp::PublishApp {
            account_id: account.id,
            seq: account.seq,
            app,
        };
        self.send(&self.ops_endpoint, &Envelope::seal(&self.key, &op))
    }

    /// Creates an account whose account key is this client's key, with `device`
    /// as its first device. The account key then signs only rarely: approving
    /// and revoking devices, changing controllers, and cash-outs.
    pub fn create_account(&self, device: PublicKey) -> Result<AccountStatus, ClientError> {
        let op = AccountOp::CreateAccount { device_key: device };
        self.send(&self.ops_endpoint, &Envelope::seal(&self.key, &op))
    }

    /// Starts the recovery of this device into the account `account_id`, with
    /// what the account's controllers signed to approve it.
    pub fn recover(
        &self,
        account_id: AccountId,
        approvals: Vec<Approval>,
    ) -> Result<AccountStatus, ClientError> {
        let op = AccountOp::RecoverDevice {
            account_id,
            at: unix_now(),
            approvals,
        };
        self.send(&self.ops_endpoint, &Envelope::seal(&self.key, &op))
    }

    /// Finishes this device's recovery, once its waiting period is over.
    pub fn complete_recovery(&self, account_id: AccountId) -> Result<AccountStatus, ClientError> {
        let op = AccountOp::CompleteRecovery {
            account_id,
            at: unix_now(),
        };
        self.send(&self.ops_endpoint, &Envelope::seal(&self.key, &op))
    }

    /// Cancels the recovery of `device_key` that waits on this device's account.
    pub fn cancel_recovery(&self, device_key: PublicKey) -> Result<AccountStatus, ClientError> {
        let account = self.account()?.account;
        let op = AccountOp::CancelRecovery {
            account_id: account.id,
            seq: account.seq,
            device_key,
        };
        self.send(&self.ops_endpoint, &Envelope::seal(&self.key, &op))
    }

    /// As the treasury: records that `account` bought `amount`
    /// credits with the payment named `payment`.
    pub fn credit_purchase(
        &self,
        account: AccountId,
        amount: Credits,
        payment: String,
    ) -> Result<(), ClientError> {
        let call = Call::CreditPurchase {
            account,
            amount,
            payment,
        };
        self.call::<IgnoredAny>(call)?;
        Ok(())
    }

    /// As the coordinator's operator: the cash-outs that wait.
    pub fn cash_outs(&self) -> Result<Vec<CashOut>, ClientError> {
        let CashOuts { waiting } = self.call(Call::GetCashOuts)?;
        Ok(waiting)
    }

    /// As the treasury: records that a cash-out was paid by the
    /// payment named `payout`.
    pub fn pay_cash_out(&self, id: ContentHash, payout: String) -> Result<(), ClientError> {
        self.call::<IgnoredAny>(Call::PayCashOut { id, payout })?;
        Ok(())
    }

    /// As the coordinator's operator: refuses a cash-out.
    pub fn refuse_cash_out(&self, id: ContentHash) -> Result<(), ClientError> {
        self.call::<IgnoredAny>(Call::RefuseCashOut { id })?;
        Ok(())
    }

    pub fn stake(&self, amount: Credits) -> Result<AccountStatus, ClientError> {
        self.call(Call::Stake { amount })
    }

    pub fn unstake(&self, amount: Credits) -> Result<AccountStatus, ClientError> {
        self.call(Call::Unstake { amount })
    }

    /// How far the coordinator trusts this device as a node.
    pub fn standing(&self) -> Result<Standing, ClientError> {
        self.call(Call::GetStanding)
    }

    /// Leaves bytes with the control plane, for always-on work only (see
    /// `Call::PutBlob`); everything else goes to storage nodes (`store::put`).
    pub fn put_blob(&self, bytes: Vec<u8>) -> Result<ContentHash, ClientError> {
        let stored: BlobRef = self.call(Call::PutBlob { data: Blob(bytes) })?;
        Ok(stored.hash)
    }

    /// Fetches a blob the control plane keeps and checks it against its root,
    /// so a wrong or damaged blob is never run or handed on.
    pub fn get_blob(&self, hash: ContentHash) -> Result<Vec<u8>, ClientError> {
        let BlobData { data: Blob(bytes) } = self.call(Call::GetBlob { hash })?;
        if decomp_core::merkle::root(&bytes) != hash {
            return Err(ClientError::CorruptBlob);
        }
        Ok(bytes)
    }

    /// The hash of the contents a shard of an app's state has now, or `None` if
    /// the shard is empty.
    pub fn state(&self, app: AccountId, shard: String) -> Result<Option<ContentHash>, ClientError> {
        let now: ShardNow = self.call(Call::GetState { app, shard })?;
        Ok(now.contents)
    }

    pub fn submit_job(&self, manifest: Manifest) -> Result<JobId, ClientError> {
        let submitted: Submitted = self.call(Call::SubmitJob { manifest })?;
        Ok(submitted.job_id)
    }

    /// Where a job stands, with its receipt once it has one.
    pub fn job(&self, job_id: JobId) -> Result<JobStatus, ClientError> {
        self.call(Call::GetReceipt { job_id })
    }

    /// What a job would cost if submitted now.
    pub fn quote(&self, manifest: Manifest) -> Result<Quote, ClientError> {
        self.call(Call::QuoteJob { manifest })
    }

    /// Asks for work and learns what this node should be doing: its leases,
    /// results to reveal, blobs to take in, a storage challenge, and channels
    /// to carry. `address` is where other devices can connect to it, if they
    /// can.
    pub fn request_lease(&self, address: Option<&str>) -> Result<LeasesGranted, ClientError> {
        self.call(Call::RequestLease {
            address: address.map(String::from),
        })
    }

    /// Commits to a result for a leased job.
    pub fn commit_result(&self, job_id: JobId, commitment: ContentHash) -> Result<(), ClientError> {
        self.call::<IgnoredAny>(Call::CommitResult { job_id, commitment })?;
        Ok(())
    }

    /// Reveals a committed result.
    pub fn reveal_result(&self, result: JobResult, salt: ContentHash) -> Result<(), ClientError> {
        self.call::<IgnoredAny>(Call::RevealResult { result, salt })?;
        Ok(())
    }

    /// A device's work preferences. Any device of the same account may ask.
    pub fn work_preferences(&self, device: PublicKey) -> Result<WorkPreferences, ClientError> {
        self.call(Call::GetWorkPreferences { device })
    }

    pub fn set_work_preferences(
        &self,
        device: PublicKey,
        preferences: WorkPreferences,
    ) -> Result<WorkPreferences, ClientError> {
        self.call(Call::SetWorkPreferences {
            device,
            preferences,
        })
    }

    pub fn place_bid(&self, bid: BidOrder) -> Result<Bid, ClientError> {
        self.call(Call::PlaceBid { bid })
    }

    pub fn cancel_bid(&self, id: ContentHash) -> Result<(), ClientError> {
        self.call::<IgnoredAny>(Call::CancelBid { id })?;
        Ok(())
    }

    pub fn bids(&self) -> Result<Vec<Bid>, ClientError> {
        let bids: Bids = self.call(Call::ListBids)?;
        Ok(bids.bids)
    }

    pub fn rates(&self) -> Result<Rates, ClientError> {
        self.call(Call::GetRates)
    }

    pub fn activity(&self, since: u64) -> Result<Activity, ClientError> {
        self.call(Call::GetActivity { since })
    }

    /// Approves a device into this device's account, by the code the new
    /// device shows.
    pub fn approve_device(&self, device_key: PublicKey) -> Result<AccountStatus, ClientError> {
        let account = self.account()?.account;
        let op = AccountOp::ApproveDevice {
            account_id: account.id,
            seq: account.seq,
            device_key,
        };
        self.send(&self.ops_endpoint, &Envelope::seal(&self.key, &op))
    }

    /// Asks for earned credits to be cashed out. The client must sign with the
    /// account key itself.
    pub fn cash_out(&self, amount: Credits) -> Result<ContentHash, ClientError> {
        let asked: CashOutAsked = self.call(Call::CashOut { amount })?;
        Ok(asked.id)
    }

    pub fn renew_name(&self, name: String) -> Result<NameRecord, ClientError> {
        self.call(Call::RenewName { name })
    }

    pub fn release_name(&self, name: String) -> Result<(), ClientError> {
        self.call::<IgnoredAny>(Call::ReleaseName { name })?;
        Ok(())
    }

    pub fn register_name(&self, name: String) -> Result<NameRecord, ClientError> {
        self.call(Call::RegisterName { name })
    }

    pub fn resolve_name(&self, name: String) -> Result<Resolved, ClientError> {
        self.call(Call::ResolveName { name })
    }

    pub fn directory(
        &self,
        verified_only: bool,
        from: Option<String>,
    ) -> Result<Directory, ClientError> {
        self.call(Call::ListDirectory {
            verified_only,
            query: None,
            from,
            limit: None,
        })
    }

    pub fn set_verified(&self, name: String, verified: bool) -> Result<NameRecord, ClientError> {
        self.call(Call::SetVerified { name, verified })
    }

    /// Pays to have a blob held for `days` days; answers with where to upload it.
    /// With `to`, the blob is left for that device to collect.
    pub fn store_blob(
        &self,
        root: ContentHash,
        size: u64,
        days: u32,
        to: Option<PublicKey>,
    ) -> Result<Placement, ClientError> {
        self.call(Call::StoreBlob {
            root,
            size,
            days,
            to,
        })
    }

    pub fn confirm_stored(&self, root: ContentHash) -> Result<(), ClientError> {
        self.call::<IgnoredAny>(Call::ConfirmStored { root })?;
        Ok(())
    }

    /// Deletes a blob this account stored.
    pub fn delete_blob(&self, root: ContentHash) -> Result<(), ClientError> {
        self.call::<IgnoredAny>(Call::DeleteBlob { root })?;
        Ok(())
    }

    /// Where a stored blob can be fetched from.
    /// Asks for a blob a job produced to be stored, paid by the job's payer.
    pub fn store_produced(
        &self,
        job_id: JobId,
        root: ContentHash,
        size: u64,
    ) -> Result<Placement, ClientError> {
        self.call(Call::StoreProduced { job_id, root, size })
    }

    pub fn locate(&self, root: ContentHash) -> Result<Located, ClientError> {
        self.call(Call::LocateBlob { root })
    }

    pub fn answer_challenge(
        &self,
        challenge: Challenge,
        chunk: Blob,
        proof: Vec<ContentHash>,
    ) -> Result<(), ClientError> {
        self.call::<IgnoredAny>(Call::AnswerChallenge {
            challenge,
            chunk,
            proof,
        })?;
        Ok(())
    }

    /// Publishes the key that data for this device should be sealed to.
    pub fn publish_sealing_key(&self, key: SealingKey) -> Result<(), ClientError> {
        self.call::<IgnoredAny>(Call::PublishSealingKey { key })?;
        Ok(())
    }

    /// The sealing key `device` has published, checked against that device's
    /// own signature. `None` if it has published none.
    pub fn sealing_key(&self, device: PublicKey) -> Result<Option<SealingKey>, ClientError> {
        let published: PublishedKey = self.call(Call::GetSealingKey { device })?;
        Ok(published
            .proof
            .and_then(|proof| seal::published_key(&device, &proof)))
    }

    /// Opens a channel to the device `to` through a relay node, paying for up
    /// to `bytes` bytes; answers with where to send them.
    pub fn open_relay(&self, to: PublicKey, bytes: u64) -> Result<Route, ClientError> {
        self.call(Call::OpenRelay { to, bytes })
    }

    /// What is addressed to this device: open channels, and stored blobs left
    /// for it. Asking also shows that this device is waiting to receive.
    pub fn incoming(&self) -> Result<IncomingChannels, ClientError> {
        self.call(Call::ListIncoming)
    }

    /// Claims a relay node's pay with a frame header from a channel's sender and
    /// the receiver's signed word that it has that frame.
    pub fn redeem_relay(&self, header: Envelope, receipt: Envelope) -> Result<(), ClientError> {
        self.call::<IgnoredAny>(Call::RedeemRelay { header, receipt })?;
        Ok(())
    }

    /// The settlement log, checked from its first entry: every entry is signed
    /// by the one control-plane key and follows the entry before it.
    pub fn settlement_log(&self) -> Result<SettlementLog, ClientError> {
        let mut log = SettlementLog::default();
        let mut from = 0;
        loop {
            let LogPage { entries } = self.log_page(from)?;
            for entry in &entries {
                log.apply(entry)
                    .map_err(|error| ClientError::BadLog(from, error))?;
                from += 1;
            }
            if entries.len() < MAX_LOG_PAGE {
                return Ok(log);
            }
        }
    }

    /// A page of the settlement log, from entry `from` on.
    pub fn log_page(&self, from: u64) -> Result<LogPage, ClientError> {
        self.call(Call::GetLog { from })
    }

    /// A page of the draws the job board has published, from number `from` on.
    pub fn draws(&self, from: u64) -> Result<DrawPage, ClientError> {
        self.call(Call::GetDraws { from })
    }

    /// Proof that the receipt of a job is in a batch of the settlement log.
    pub fn inclusion(&self, job_id: JobId) -> Result<Inclusion, ClientError> {
        self.call(Call::GetInclusion { job_id })
    }

    fn call<R: DeserializeOwned>(&self, call: Call) -> Result<R, ClientError> {
        let request = Request {
            issued_at: unix_now(),
            call,
        };
        self.send(&self.endpoint, &Envelope::seal(&self.key, &request))
    }

    fn send<R: DeserializeOwned>(
        &self,
        endpoint: &str,
        envelope: &Envelope,
    ) -> Result<R, ClientError> {
        let unreachable = |error: ureq::Error| ClientError::Unreachable(error.to_string());

        let mut response = self
            .http
            .post(endpoint)
            .send_json(envelope)
            .map_err(unreachable)?;
        let succeeded = response.status().is_success();
        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_MESSAGE_BYTES as u64);
        if succeeded {
            return body.read_json().map_err(unreachable);
        }
        let Refusal { error, message } = body.read_json().map_err(unreachable)?;
        Err(ClientError::Refused {
            code: error,
            message,
        })
    }
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after 1970")
        .as_secs()
}
