//! Sealing: encrypting data so that only one device can read it (spec section 1,
//! "Device-to-device communication").
//!
//! Each device has a sealing key, which it publishes, and the opening key that
//! goes with it, which it keeps. Anyone can seal to a sealing key; nobody else,
//! and no node that carries or holds the sealed data, can open it.
//!
//! Sealing makes a new key pair for each message, used once. That pair and the
//! receiver's sealing key give a secret only the two sides can work out (X25519).
//! The secret is turned into a cipher key (HKDF with SHA-256), and the message
//! is encrypted with it (AES-256-GCM), which also lets the receiver tell if a
//! single bit was changed. A sealed message is the one-time public key followed
//! by the encrypted message. These are the choices a browser can make natively,
//! so every kind of device seals the same way.
//!
//! Sealing hides what is sent, not who sent it. A sender signs what it sends.

use std::collections::HashMap;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use hkdf::Hkdf;
use sha2::Sha256;

use crate::envelope::{Envelope, EnvelopeError};
use crate::keys::{OpeningKey, PublicKey, SealingKey};
use crate::request::{Call, Request};

/// The sealing key a device published, read from its own signed request to
/// publish it. `None` if `proof` is not that device's request to publish a key.
/// A sender checks the key this way, so it need not take anyone's word for which
/// key is a device's.
pub fn published_key(device: &PublicKey, proof: &Envelope) -> Option<SealingKey> {
    let request: Request = proof.open().ok()?;
    match request.call {
        Call::PublishSealingKey { key } if proof.signer == *device => Some(key),
        _ => None,
    }
}

/// The sealing key each device last published, kept as the signed requests that
/// published them. A log of those requests is a complete copy.
#[derive(Default)]
pub struct SealingKeys {
    /// By device: when its request was made, and the request.
    published: HashMap<PublicKey, (u64, Envelope)>,
}

impl SealingKeys {
    /// Takes in a device's request to publish a key. Returns whether it is the
    /// newest from that device, which is when it should be logged.
    pub fn apply(&mut self, proof: &Envelope) -> Result<bool, EnvelopeError> {
        let request: Request = proof.open()?;
        let newest = self
            .published
            .get(&proof.signer)
            .is_none_or(|(at, _)| *at < request.issued_at);
        if newest {
            let kept = (request.issued_at, proof.clone());
            self.published.insert(proof.signer, kept);
        }
        Ok(newest)
    }

    /// The request in which `device` published its current key, if it has.
    pub fn proof(&self, device: &PublicKey) -> Option<&Envelope> {
        self.published.get(device).map(|(_, proof)| proof)
    }
}

/// Mixed into every cipher key, so a key made here is no use for anything else.
const KEY_PURPOSE: &[u8] = b"decomp/v1 seal";

/// Bytes a sealed message is longer than the message: the one-time public key,
/// and the tag that shows the message was not changed.
pub const SEAL_OVERHEAD_BYTES: usize = 32 + 16;

/// The cipher's nonce. It may be fixed because each cipher key seals one message.
const NONCE: [u8; 12] = [0; 12];

/// Seals `message` so that only the holder of `to`'s opening key can read it.
/// `None` if `to` is not a usable key.
pub fn seal(to: &SealingKey, message: &[u8]) -> Option<Vec<u8>> {
    let once = OpeningKey::generate();
    let cipher = cipher(&once.shared_with(to)?, &once.sealing_key(), to);
    let encrypted = cipher
        .encrypt(&Nonce::from(NONCE), message)
        .expect("a message in memory is within what the cipher can encrypt");
    Some([once.sealing_key().as_bytes().as_slice(), &encrypted].concat())
}

/// Opens a message sealed to `key`'s sealing key. `None` if it was sealed to
/// another key, or was changed or cut short on the way.
pub fn open(key: &OpeningKey, sealed: &[u8]) -> Option<Vec<u8>> {
    let (once, encrypted) = sealed.split_at_checked(32)?;
    let once = SealingKey::from_bytes(once.try_into().ok()?);
    let cipher = cipher(&key.shared_with(&once)?, &once, &key.sealing_key());
    cipher.decrypt(&Nonce::from(NONCE), encrypted).ok()
}

/// The cipher for one message, from what its two sides share. Both public keys
/// are mixed in, so the key is tied to this one-time key and this receiver.
fn cipher(shared: &[u8; 32], once: &SealingKey, to: &SealingKey) -> Aes256Gcm {
    let salt = [once.as_bytes().as_slice(), to.as_bytes()].concat();
    let mut key = [0u8; 32];
    Hkdf::<Sha256>::new(Some(&salt), shared)
        .expand(KEY_PURPOSE, &mut key)
        .expect("32 bytes is a length HKDF can produce");
    Aes256Gcm::new(&key.into())
}
