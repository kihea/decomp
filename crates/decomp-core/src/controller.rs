//! An account's controllers, and how one approves a device (spec section 1,
//! "Sign-in and recovery": "DeComp knows only controller keys. How a person
//! proves they should control one is handled ... outside the account record").
//!
//! The account record holds, for each sign-in or recovery method the user
//! added, what the spec's table says DeComp stores:
//!
//! | Method | What DeComp stores | Here |
//! | --- | --- | --- |
//! | Passkey | The passkey's public key | `Controller::Passkey` |
//! | Recovery key or phrase | A public key | `Controller::Key` |
//! | Guardian accounts | The guardians' account IDs and how many must agree | `Controller::Guardians` |
//! | Email reset | The public key of a recovery service | `Controller::Key` |
//! | Crypto wallet | The wallet's public key | `Controller::Wallet` |
//!
//! A controller approves a device by signing one line of text that names the
//! account and the device (`statement`). Each kind signs it in the way its key
//! can: see `Approval`.

use std::collections::HashSet;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64_URL;
use k256::ecdsa::signature::hazmat::PrehashVerifier;
use p256::ecdsa::signature::Verifier;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sha3::Keccak256;

use crate::keys::{AccountId, PasskeyKey, PublicKey, Signature, WalletKey, WalletSignature};
use crate::request::Blob;

/// The most controllers an account may have. It bounds the size of an account
/// record and the signatures checked for one recovery.
pub const MAX_CONTROLLERS: usize = 8;

/// The most guardian accounts one controller may name. It bounds the same.
pub const MAX_GUARDIANS: usize = 16;

/// The most controllers an account can require to agree (spec section 1: "an
/// account can require two methods to agree").
pub const MOST_METHODS_REQUIRED: u32 = 2;

/// How long a recovery waits before it takes effect (spec section 1: "A
/// recovery takes effect after 48 hours (starting value), and any approved
/// device can cancel it in that time").
pub const RECOVERY_WAIT_SECS: u64 = 48 * 60 * 60;

/// The most approvals one request to start a recovery may carry: one for each
/// controller, and one for each guardian of one of them. It bounds the
/// signatures checked for one request.
pub const MAX_APPROVALS: usize = MAX_CONTROLLERS + MAX_GUARDIANS;

/// The most recoveries an account may have waiting at once. It bounds the
/// size of an account record.
pub const MAX_WAITING_RECOVERIES: usize = 4;

/// Starts the text a controller signs to approve a device, so that the
/// signature can never be taken for one over anything else.
const APPROVAL_DOMAIN: &str = "decomp/v1/approve-device\n";

/// What an Ethereum wallet puts in front of a message before signing it
/// (`personal_sign`, EIP-191), followed by the message's length in bytes.
const WALLET_PREFIX: &str = "\x19Ethereum Signed Message:\n";

/// What a passkey's signed client data says it was asked for, when it was
/// asked to sign in and not to make a new passkey.
const PASSKEY_ASSERTION: &str = "webauthn.get";

/// Where the flags are in a passkey's authenticator data: after the 32-byte
/// hash of the site's name. The data ends with a 4-byte counter.
const PASSKEY_FLAGS_AT: usize = 32;
const PASSKEY_DATA_MIN_BYTES: usize = 37;
/// The flag that says a person was present at the passkey.
const PASSKEY_USER_PRESENT: u8 = 0x01;

/// One way to prove control of an account, as the account record holds it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Controller {
    /// A key that signs as account and device keys do: a recovery key, kept by
    /// the user offline, or the key of a recovery service. DeComp does not
    /// tell the two apart.
    Key {
        key: PublicKey,
    },
    Passkey {
        key: PasskeyKey,
    },
    Wallet {
        key: WalletKey,
    },
    /// Other accounts, `required` of which must approve.
    Guardians {
        accounts: Vec<AccountId>,
        required: u32,
    },
}

impl Controller {
    /// What is wrong with this controller, if anything is.
    pub fn problem(&self) -> Option<&'static str> {
        let Self::Guardians { accounts, required } = self else {
            return None;
        };
        let distinct: HashSet<&AccountId> = accounts.iter().collect();
        if accounts.len() > MAX_GUARDIANS {
            Some("a controller names too many guardian accounts")
        } else if distinct.len() != accounts.len() {
            Some("a guardian account is named twice")
        } else if *required == 0 || *required as usize > accounts.len() {
            Some("the number of guardians that must agree is not between one and all of them")
        } else {
            None
        }
    }
}

/// What is wrong with `controllers` as the controllers of `account`, with
/// `methods_required` of them having to agree, if anything is.
pub fn problem(
    controllers: &[Controller],
    methods_required: u32,
    account: &AccountId,
) -> Option<&'static str> {
    let twice =
        |(place, controller): (usize, &Controller)| controllers[..place].contains(controller);
    let own_guardian = |controller: &Controller| matches!(controller, Controller::Guardians { accounts, .. } if accounts.contains(account));
    // An account with no controllers still says that one must agree.
    let most = (controllers.len() as u32).clamp(1, MOST_METHODS_REQUIRED);
    if controllers.len() > MAX_CONTROLLERS {
        Some("an account may not have that many controllers")
    } else if controllers.iter().enumerate().any(twice) {
        Some("a controller is listed twice")
    } else if controllers.iter().any(own_guardian) {
        Some("an account cannot be its own guardian")
    } else if methods_required == 0 || methods_required > most {
        Some("the number of controllers that must agree is not one, or two of at least two")
    } else {
        controllers.iter().find_map(Controller::problem)
    }
}

/// One signature over the statement that approves a device. A passkey's and
/// a wallet's do not name their key: a device that is in no account cannot
/// read the account's record to learn it, so each is tried against the
/// account's passkeys or wallets.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Approval {
    /// By a recovery key, by a recovery service's key, or by the account key
    /// or a device of a guardian account. The signature is over the statement.
    Key {
        signer: PublicKey,
        signature: Signature,
    },
    /// By a passkey, as a WebAuthn assertion: the passkey signs its
    /// authenticator data followed by the SHA-256 hash of `client_data`, and
    /// `client_data` holds the challenge, which is the SHA-256 hash of the
    /// statement.
    Passkey {
        authenticator_data: Blob,
        /// The client data exactly as the browser made it, a JSON text.
        client_data: String,
        /// The signature in the form passkeys give it (ASN.1 DER).
        signature: Blob,
    },
    /// By a wallet, as Ethereum's `personal_sign` of the statement.
    Wallet { signature: WalletSignature },
}

/// The part of a passkey's client data that is checked. A browser adds other
/// fields, such as the site the passkey was used on. DeComp holds no site to
/// compare that with: which sites may use a passkey is the browser's rule.
#[derive(Deserialize)]
struct ClientData {
    #[serde(rename = "type")]
    kind: String,
    challenge: String,
}

impl Approval {
    /// The key whose true signature over `statement` this is, if it is one
    /// made as account and device keys sign.
    fn key_signer(&self, statement: &str) -> Option<PublicKey> {
        match self {
            Self::Key { signer, signature } => signer
                .verifies(statement.as_bytes(), signature)
                .then_some(*signer),
            _ => None,
        }
    }

    /// Whether this is a true assertion over `statement` by the passkey `key`.
    fn is_by_passkey(&self, key: &PasskeyKey, statement: &str) -> bool {
        let Self::Passkey {
            authenticator_data: Blob(authenticator_data),
            client_data,
            signature: Blob(signature),
        } = self
        else {
            return false;
        };
        let Ok(ClientData { kind, challenge }) = serde_json::from_str(client_data) else {
            return false;
        };
        let asked = BASE64_URL.encode(Sha256::digest(statement));
        let present = authenticator_data.len() >= PASSKEY_DATA_MIN_BYTES
            && authenticator_data[PASSKEY_FLAGS_AT] & PASSKEY_USER_PRESENT != 0;
        if kind != PASSKEY_ASSERTION || challenge != asked || !present {
            return false;
        }
        let Ok(key) = p256::ecdsa::VerifyingKey::from_sec1_bytes(key.as_bytes()) else {
            return false;
        };
        let Ok(signature) = p256::ecdsa::DerSignature::from_bytes(signature) else {
            return false;
        };
        let signed = [authenticator_data.as_slice(), &Sha256::digest(client_data)].concat();
        key.verify(&signed, &signature).is_ok()
    }

    /// Whether this is a true `personal_sign` of `statement` by the wallet
    /// `key`.
    fn is_by_wallet(&self, key: &WalletKey, statement: &str) -> bool {
        let Self::Wallet { signature } = self else {
            return false;
        };
        let Ok(key) = k256::ecdsa::VerifyingKey::from_sec1_bytes(key.as_bytes()) else {
            return false;
        };
        // The last byte only says which key made the signature, and the key
        // it is checked against is already known.
        let (r_and_s, _) = signature.as_bytes().split_at(64);
        let Ok(signature) = k256::ecdsa::Signature::from_slice(r_and_s) else {
            return false;
        };
        let mut hasher = Keccak256::new();
        hasher.update(WALLET_PREFIX);
        hasher.update(statement.len().to_string());
        hasher.update(statement);
        key.verify_prehash(&hasher.finalize(), &signature).is_ok()
    }
}

/// The text a controller signs to approve `device` into `account`. It names
/// no time and no place in the account's history, because guardians may take
/// days to sign and a device not yet in the account cannot read its record.
/// It can be used once all the same: a device key belongs to one account for
/// good, whether its recovery takes effect or is cancelled.
pub fn statement(account: &AccountId, device: &PublicKey) -> String {
    format!("{APPROVAL_DOMAIN}{account}\n{device}")
}

/// How many of `controllers` have approved, given `approvals` of `statement`.
/// `acts_for` gives the account a key may act for now, as its account key or
/// as an approved device: that is how a guardian account signs. More than
/// `MAX_APPROVALS` approvals count for nothing.
pub fn agreeing(
    controllers: &[Controller],
    approvals: &[Approval],
    statement: &str,
    acts_for: impl Fn(&PublicKey) -> Option<AccountId>,
) -> usize {
    if approvals.len() > MAX_APPROVALS {
        return 0;
    }
    let signers: Vec<PublicKey> = approvals
        .iter()
        .filter_map(|approval| approval.key_signer(statement))
        .collect();
    let agrees = |controller: &&Controller| match controller {
        Controller::Key { key } => signers.contains(key),
        Controller::Passkey { key } => approvals
            .iter()
            .any(|approval| approval.is_by_passkey(key, statement)),
        Controller::Wallet { key } => approvals
            .iter()
            .any(|approval| approval.is_by_wallet(key, statement)),
        Controller::Guardians { accounts, required } => {
            let guardians: HashSet<AccountId> = signers
                .iter()
                .filter_map(&acts_for)
                .filter(|account| accounts.contains(account))
                .collect();
            guardians.len() >= *required as usize
        }
    };
    controllers.iter().filter(agrees).count()
}
