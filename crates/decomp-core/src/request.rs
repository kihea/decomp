//! Requests a device sends to the coordinator and the shapes of its answers.
//! Changes to an account are not here: those are `AccountOp`s, which are logged.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::account::Account;
use crate::job::{Assignment, JobResult, Manifest};
use crate::keys::{ContentHash, JobId};
use crate::ledger::{Balance, Credits};

/// The largest request or answer either side accepts. A blob travels inside one
/// as base64, so this allows blobs of up to about 48 MB.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

/// A signed request. `issued_at` (seconds since 1970) must be close to the
/// coordinator's clock, which bounds how long a captured request can be replayed.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub issued_at: u64,
    pub call: Call,
}

/// What a request asks for. The comment on each variant names the answer's type.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Call {
    /// The account the signer belongs to. Answer: `AccountStatus`.
    GetAccount,
    /// Stores bytes and names them by their hash. Answer: `BlobRef`.
    PutBlob { data: Blob },
    /// Answer: `BlobData`.
    GetBlob { hash: ContentHash },
    /// Submits a job, billed to the signer's account. Answer: `Submitted`.
    SubmitJob { manifest: Manifest },
    /// Answer: `JobStatus`.
    GetJob { job_id: JobId },
    /// Puts earned credits up as stake behind the account's nodes.
    /// Answer: `AccountStatus`.
    Stake { amount: Credits },
    /// Takes stake back. Refused while one of the account's nodes is on a job.
    /// Answer: `AccountStatus`.
    Unstake { amount: Credits },
    /// How far the signing node is trusted. Answer: `Standing`.
    GetStanding,
    /// A node reporting that it is online and asking for its work. Answer: `CheckedIn`.
    CheckIn,
    /// A node reporting what a job produced. Answer: an empty object.
    SubmitResult { result: JobResult },
}

/// An account record together with its credits. Changing an account answers
/// with this too.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AccountStatus {
    #[serde(flatten)]
    pub account: Account,
    pub balance: Balance,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BlobRef {
    pub hash: ContentHash,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BlobData {
    pub data: Blob,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Submitted {
    pub job_id: JobId,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CheckedIn {
    pub assignment: Option<Assignment>,
}

/// The body of every error answer. Programs branch on `error`, a stable code;
/// `message` is for people.
#[derive(Debug, Serialize, Deserialize)]
pub struct Refusal {
    pub error: String,
    pub message: String,
}

/// Raw bytes, which travel as base64 text inside JSON.
#[derive(Clone, PartialEq, Eq)]
pub struct Blob(pub Vec<u8>);

impl std::fmt::Debug for Blob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Blob({} bytes)", self.0.len())
    }
}

impl Serialize for Blob {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&BASE64.encode(&self.0))
    }
}

impl<'de> Deserialize<'de> for Blob {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        BASE64
            .decode(text)
            .map(Blob)
            .map_err(serde::de::Error::custom)
    }
}
