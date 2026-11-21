//! Signed envelopes: the one way a signed message travels in DeComp.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::keys::{PublicKey, Signature, SigningKey};

/// Prefixed to every payload before signing, so a DeComp signature can never be
/// replayed as a signature in another protocol or another version of this one.
const SIGNING_DOMAIN: &str = "decomp/v1\n";

/// A JSON payload and a signature over its exact text.
///
/// The payload travels as a string, not as nested JSON, so the verifier checks the
/// same bytes the signer saw. Nothing is re-serialized, which means the protocol
/// needs no canonical encoding and implementations in other languages stay small.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Envelope {
    pub payload: String,
    pub signer: PublicKey,
    pub signature: Signature,
}

#[derive(Debug, thiserror::Error)]
pub enum EnvelopeError {
    #[error("the signature does not match the payload and signer")]
    BadSignature,
    #[error("the payload is not a valid message: {0}")]
    BadPayload(#[from] serde_json::Error),
}

impl Envelope {
    /// Serializes `message` and signs it with `key`.
    pub fn seal<T: Serialize>(key: &SigningKey, message: &T) -> Self {
        let payload = serde_json::to_string(message).expect("protocol messages serialize");
        let signature = key.sign(signing_input(&payload).as_bytes());
        Self {
            payload,
            signer: key.public_key(),
            signature,
        }
    }

    /// The payload `message` travels as and the bytes a signer signs for it,
    /// for a key held elsewhere, such as a threshold key, which hands back a
    /// signature for `with_signature`.
    pub fn to_sign<T: Serialize>(message: &T) -> (String, Vec<u8>) {
        let payload = serde_json::to_string(message).expect("protocol messages serialize");
        let bytes = signing_input(&payload).into_bytes();
        (payload, bytes)
    }

    /// An envelope from a payload and a signature made elsewhere. It is not
    /// checked here; `open` checks it.
    pub fn with_signature(payload: String, signer: PublicKey, signature: Signature) -> Self {
        Self {
            payload,
            signer,
            signature,
        }
    }

    /// Checks the signature, then parses the payload. The payload is never parsed
    /// unless the signature is valid.
    pub fn open<T: DeserializeOwned>(&self) -> Result<T, EnvelopeError> {
        if !self
            .signer
            .verifies(signing_input(&self.payload).as_bytes(), &self.signature)
        {
            return Err(EnvelopeError::BadSignature);
        }
        Ok(serde_json::from_str(&self.payload)?)
    }
}

fn signing_input(payload: &str) -> String {
    [SIGNING_DOMAIN, payload].concat()
}
