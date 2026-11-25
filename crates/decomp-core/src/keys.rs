//! Key, signature, and identifier types. On the wire each is a lowercase hex string.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};

/// Defines a fixed-size byte newtype that reads and writes as hex.
macro_rules! hex_newtype {
    ($(#[$meta:meta])* $name:ident, $len:expr) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name([u8; $len]);

        impl $name {
            pub fn from_bytes(bytes: [u8; $len]) -> Self {
                Self(bytes)
            }

            pub fn as_bytes(&self) -> &[u8; $len] {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&hex::encode(self.0))
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self)
            }
        }

        impl FromStr for $name {
            type Err = hex::FromHexError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                let mut bytes = [0u8; $len];
                hex::decode_to_slice(s, &mut bytes)?;
                Ok(Self(bytes))
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let text = String::deserialize(deserializer)?;
                text.parse().map_err(serde::de::Error::custom)
            }
        }
    };
}

hex_newtype!(
    /// An Ed25519 public key. Account keys and device keys are both this type.
    PublicKey,
    32
);

hex_newtype!(
    /// An Ed25519 signature.
    Signature,
    64
);

hex_newtype!(
    /// An account's identity: the SHA-256 hash of its account public key.
    AccountId,
    32
);

impl PublicKey {
    /// Whether `signature` is this key's signature over `message`.
    pub fn verifies(&self, message: &[u8], signature: &Signature) -> bool {
        let Ok(key) = ed25519_dalek::VerifyingKey::from_bytes(&self.0) else {
            return false;
        };
        let signature = ed25519_dalek::Signature::from_bytes(&signature.0);
        key.verify_strict(message, &signature).is_ok()
    }
}

hex_newtype!(
    /// The SHA-256 hash of some bytes. Modules, inputs, and outputs are named and
    /// compared by this.
    ContentHash,
    32
);

hex_newtype!(
    /// A job: the hash of the signed request that submitted it, so the same request
    /// sent twice names the same job.
    ///
    /// It is a one-time ID (spec section 7: jobs reach nodes "under one-time IDs,
    /// so a node cannot tell whose job it is running"). A job has its own, and
    /// the hash takes in the request's signature, which only the submitter and
    /// the coordinator have. So a node that is handed the ID, and knows what the
    /// job is and which device it suspects, still cannot check the guess.
    JobId,
    32
);

/// An Ed25519 secret key. It has no text form and no `Debug` output, so it cannot
/// end up in a log by accident.
pub struct SigningKey(ed25519_dalek::SigningKey);

impl SigningKey {
    pub fn generate() -> Self {
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).expect("the operating system provides randomness");
        Self::from_seed(seed)
    }

    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self(ed25519_dalek::SigningKey::from_bytes(&seed))
    }

    /// The 32 secret bytes this key can be rebuilt from.
    pub fn seed(&self) -> [u8; 32] {
        self.0.to_bytes()
    }

    pub fn public_key(&self) -> PublicKey {
        PublicKey(self.0.verifying_key().to_bytes())
    }

    pub fn sign(&self, message: &[u8]) -> Signature {
        Signature(ed25519_dalek::Signer::sign(&self.0, message).to_bytes())
    }
}

hex_newtype!(
    /// The name a device gives the shares it cuts one private job into (see
    /// `shares`): 32 bytes drawn at random, so that no two jobs' shares carry
    /// the same name by accident.
    ShareSet,
    32
);

impl ShareSet {
    pub fn generate() -> Self {
        let mut name = [0u8; 32];
        getrandom::fill(&mut name).expect("the operating system provides randomness");
        Self(name)
    }
}

impl JobId {
    /// The ID of the job submitted by the request with this signer, payload,
    /// and signature.
    pub fn of(submitter: &PublicKey, payload: &str, signature: &Signature) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(submitter.0);
        hasher.update(payload);
        hasher.update(signature.0);
        Self(hasher.finalize().into())
    }

    /// The ID of piece number `index`, counting from zero, of the split job
    /// `whole`.
    pub fn piece(whole: &JobId, index: u64) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(whole.0);
        hasher.update(b"piece");
        hasher.update(index.to_le_bytes());
        Self(hasher.finalize().into())
    }

    /// The ID of the follow-up job that `parent` asked for at place `index` in
    /// its result, counting from zero.
    pub fn follow_up(parent: &JobId, index: u64) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(parent.0);
        hasher.update(b"follow-up");
        hasher.update(index.to_le_bytes());
        Self(hasher.finalize().into())
    }
}

impl AccountId {
    pub fn of(account_key: &PublicKey) -> Self {
        Self(Sha256::digest(account_key.0).into())
    }
}

impl ContentHash {
    pub fn of(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }
}

hex_newtype!(
    /// Where a balance sits on the ledger (spec section 6: "No public balance
    /// under a user's identity. Apps see the account ID. On a public ledger, a
    /// balance sits under a separate ledger address that only the account's
    /// controllers can link to that ID").
    ///
    /// An account's address is a hash that takes in the signature on the
    /// request that created the account. Only the account key can make that
    /// signature, and it is seen by nobody but the account's controller and the
    /// coordinator operators, who hold the account's record. So nobody else can
    /// work the address out from the account's ID or key, and the controller
    /// can show the link by showing the request.
    LedgerAddress,
    32
);

hex_newtype!(
    /// A subaccount on an ICRC ledger: 32 bytes that, with an owner's
    /// principal, make an ICRC account.
    Subaccount,
    32
);

impl LedgerAddress {
    /// The address of the account created by the request that carries
    /// `created`, the account key's signature.
    pub fn of(created: &Signature) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(b"decomp/ledger-address/v1\n");
        hasher.update(created.0);
        Self(hasher.finalize().into())
    }

    /// The address of one of the protocol's own pools, which belong to no
    /// account: the hash of the pool's name, which anyone can work out.
    pub fn of_pool(name: &str) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(b"decomp/ledger-pool/v1\n");
        hasher.update(name);
        Self(hasher.finalize().into())
    }
}

hex_newtype!(
    /// A device's public key for sealing: data sealed to it can be opened only
    /// with the device's opening key. An X25519 key, separate from the device's
    /// Ed25519 key because the two do different work, and because some places a
    /// device keeps its signing key cannot use it for anything else.
    SealingKey,
    32
);

/// The secret half of a sealing key. Like `SigningKey`, it has no text form and
/// no `Debug` output.
pub struct OpeningKey(x25519_dalek::StaticSecret);

impl OpeningKey {
    pub fn generate() -> Self {
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).expect("the operating system provides randomness");
        Self::from_seed(seed)
    }

    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self(x25519_dalek::StaticSecret::from(seed))
    }

    /// The 32 secret bytes this key can be rebuilt from.
    pub fn seed(&self) -> [u8; 32] {
        self.0.to_bytes()
    }

    pub fn sealing_key(&self) -> SealingKey {
        SealingKey(x25519_dalek::PublicKey::from(&self.0).to_bytes())
    }

    /// What this key and `other` share, which the holder of `other`'s secret
    /// half can also work out. `None` if `other` is not a usable key.
    pub(crate) fn shared_with(&self, other: &SealingKey) -> Option<[u8; 32]> {
        let shared = self
            .0
            .diffie_hellman(&x25519_dalek::PublicKey::from(other.0));
        shared.was_contributory().then(|| shared.to_bytes())
    }
}

hex_newtype!(
    /// A passkey's public key (spec section 1, "Passkey: The passkey's public
    /// key"): a point on the P-256 curve, as the 65 bytes that start with 04.
    /// That is the form a browser gives a new passkey's key in.
    PasskeyKey,
    65
);

hex_newtype!(
    /// A crypto wallet's public key (spec section 1, "Crypto wallet: The
    /// wallet's public key"): a point on the secp256k1 curve, which Ethereum
    /// wallets sign with, as the 65 bytes that start with 04.
    WalletKey,
    65
);

hex_newtype!(
    /// A wallet's signature as an Ethereum wallet hands it over: `r`, `s`, and
    /// one more byte that says which of two keys made it.
    WalletSignature,
    65
);
