//! Keys for an account's private values, which no single party holds (spec
//! section 7, "Privacy": "Private values are stored encrypted, with keys that
//! no single party holds (ICP vetKeys)").
//!
//! A device asks the control plane for the key for a label, with a transport
//! key it made for the request. The control canister has the subnet's
//! threshold key (vetKD) derive the key for the account and the label and
//! encrypt it to the transport key, so only that device can read it, and the
//! nodes of the subnet never see it whole. Every device of the account gets the
//! same key for the same label, so private values written on one device open
//! on the others without a key passing between them.

use crate::keys::AccountId;

/// The context every value key is derived in. Keys from other contexts, or
/// from other canisters, are unrelated to these.
pub const VALUE_KEY_CONTEXT: &[u8] = b"decomp/value-keys/v1";

/// The longest label a value key may be asked for under.
pub const MAX_VALUE_KEY_LABEL_BYTES: usize = 256;

/// What the threshold key derives an account's key for `label` from: the
/// account's ID and then the label, so no account can ask for another's.
pub fn value_key_input(account: &AccountId, label: &str) -> Vec<u8> {
    [account.as_bytes().as_slice(), label.as_bytes()].concat()
}
