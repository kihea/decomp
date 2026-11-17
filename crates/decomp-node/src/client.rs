//! Client for the coordinator's request endpoint, signing as this device.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use decomp_core::LogError;
use decomp_core::{
    AccountId, AccountOp, AccountStatus, AppManifest, BatchRecords, Blob, BlobData, BlobRef, Call,
    Challenge, Change, CheckedIn, ContentHash, Credits, Envelope, Inclusion, IncomingChannels,
    JobId, JobResult, JobStatus, Located, LogPage, MAX_LOG_PAGE, MAX_MESSAGE_BYTES, Manifest,
    Placement, PublicKey, PublishedKey, Refusal, Request, Route, SealingKey, SettlementLog,
    ShardNow, SigningKey, Standing, Submitted, Through, seal,
};
use serde::de::{DeserializeOwned, IgnoredAny};

/// How long one request may take, including sending or receiving a large blob.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The coordinator understood the request and said no.
    #[error("{message}")]
    Refused { code: String, message: String },
    #[error("could not reach the coordinator: {0}")]
    Unreachable(String),
    #[error("the coordinator sent a blob that does not match its hash")]
    CorruptBlob,
    #[error("entry {0} of the coordinator's settlement log does not hold: {1}")]
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
    pub fn new(coordinator_url: &str, key: SigningKey) -> Self {
        let http = ureq::Agent::config_builder()
            // Read error bodies ourselves: they carry the coordinator's reason.
            .http_status_as_error(false)
            .timeout_global(Some(REQUEST_TIMEOUT))
            .build()
            .new_agent();
        let base = coordinator_url.trim_end_matches('/');
        Self {
            endpoint: format!("{base}/v1/request"),
            ops_endpoint: format!("{base}/v1/ops"),
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

    pub fn put_blob(&self, bytes: Vec<u8>) -> Result<ContentHash, ClientError> {
        let stored: BlobRef = self.call(Call::PutBlob { data: Blob(bytes) })?;
        Ok(stored.hash)
    }

    /// Fetches a blob and checks it against its hash, so a wrong or damaged blob
    /// is never run or handed on.
    pub fn get_blob(&self, hash: ContentHash) -> Result<Vec<u8>, ClientError> {
        let BlobData { data: Blob(bytes) } = self.call(Call::GetBlob { hash })?;
        if ContentHash::of(&bytes) != hash {
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

    pub fn job(&self, job_id: JobId) -> Result<JobStatus, ClientError> {
        self.call(Call::GetJob { job_id })
    }

    /// Reports that this node is online and learns what it should be doing.
    /// `address` is where other devices can connect to it, if they can.
    pub fn check_in(&self, address: Option<&str>) -> Result<CheckedIn, ClientError> {
        self.call(Call::CheckIn {
            address: address.map(String::from),
        })
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

    /// Where a stored blob can be fetched from.
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

    pub fn submit_result(&self, result: JobResult) -> Result<(), ClientError> {
        self.call::<IgnoredAny>(Call::SubmitResult { result })?;
        Ok(())
    }

    /// The coordinator's settlement log, checked from its first entry: every
    /// entry is signed by the one operator and follows the entry before it.
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

    /// A page of the coordinator's settlement log, from entry `from` on.
    pub fn log_page(&self, from: u64) -> Result<LogPage, ClientError> {
        self.call(Call::GetLog { from })
    }

    /// A page of the records behind the coordinator's batches, from the places
    /// `from` gives. Only an operator is answered.
    pub fn records(&self, from: Through) -> Result<BatchRecords, ClientError> {
        self.call(Call::GetRecords { from })
    }

    /// Disputes batch number `batch`: `fingerprint` and `changes` are what the
    /// records behind it give.
    pub fn dispute(
        &self,
        batch: u64,
        fingerprint: ContentHash,
        changes: Vec<Change>,
    ) -> Result<(), ClientError> {
        let call = Call::DisputeBatch {
            batch,
            fingerprint,
            changes,
        };
        self.call::<IgnoredAny>(call)?;
        Ok(())
    }

    /// A page of the draws the operator has published, from number `from` on.
    pub fn draws(&self, from: u64) -> Result<LogPage, ClientError> {
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
