//! Encrypted compute: how a private job's values reach nodes that compute on
//! them and cannot read them (spec section 7: "Encrypted compute uses
//! homomorphic encryption, so a node computes on values it cannot read").
//!
//! The device that wants a job run makes a key for that one job. The key has
//! two parts. One never leaves the device, and is what encrypts a value and
//! reads one. The other lets whoever holds it compute on encrypted values
//! without reading them. It is handed to the nodes as one of the blobs the job
//! may read.
//!
//! The device encrypts every private number and true-or-false value in the
//! job's input and submits the job on that input. The module computes on the
//! encrypted values and returns encrypted values, which the device reads with
//! the part of the key it kept. To a node, and to the job rules, the values are
//! ordinary bytes: the job is assigned, run, checked, and paid for like any
//! other, and two honest nodes return the same bytes.
//!
//! The encryption is TFHE-rs (the `tfhe` crate), with the library's default
//! parameters for encrypting single bits. A number is encrypted as its bits.
//! Nothing here adds to the library's cryptography: this module calls it to
//! make a key, to encrypt a bit, and to read one.
//!
//! # How an encrypted value travels
//!
//! In a job's JSON input or output, an encrypted value is base64 text of:
//!
//! - the name of the key it is under, `ENCRYPTION_KEY_NAME_BYTES` bytes: the
//!   SHA-256 hash of the blob holding the part of the key nodes compute with,
//!   which is how a module finds that blob;
//! - then the value, in the byte form the library's types have under `bincode`.
//!   A true-or-false value is one encrypted bit, and a number is a list of
//!   `ENCRYPTED_NUMBER_BITS` of them, lowest bit first. A device sends bits in
//!   the library's short form (`CompressedCiphertext`), and a module returns
//!   them in the full form (`Ciphertext`).
//!
//! A module may also return a private value as a plain number or `true` or
//! `false`. It does when the value did not depend on anything encrypted.
//!
//! What the nodes do see: every value that is not private, how many private
//! values there are and which are numbers, and the part of the key that
//! computes.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use decomp_schema::{
    ENCRYPTED_NUMBER_BITS, ENCRYPTION_KEY_NAME_BYTES, Mismatch, Schema, Signature,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use tfhe::boolean::ciphertext::{Ciphertext, CompressedCiphertext};
use tfhe::boolean::client_key::ClientKey;
use tfhe::boolean::parameters::DEFAULT_PARAMETERS;
use tfhe::boolean::server_key::CompressedServerKey;
use tfhe::core_crypto::prelude::LweSize;

use crate::keys::ContentHash;

// A key is named by a content hash.
const _: () = assert!(ENCRYPTION_KEY_NAME_BYTES == size_of::<ContentHash>());

/// The part of a job's key that stays on the device that made it.
pub struct EncryptionKey {
    key: ClientKey,
    /// The name every value under this key starts with.
    name: ContentHash,
    /// How long an encrypted bit under this key is, which a bit that comes back
    /// must be before it is read.
    bit_size: LweSize,
}

/// A job's input, encrypted for the nodes.
pub struct Encrypted {
    pub key: EncryptionKey,
    /// The part of the key nodes compute with. The job must be given it as a
    /// blob to read.
    pub computing_key: Vec<u8>,
    /// The input with every private value in it encrypted.
    pub input: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum EncryptError {
    #[error("{0}")]
    Input(Mismatch),
    #[error(
        "the job's private values are not all numbers and true-or-false values that can be computed on while encrypted"
    )]
    NotEncryptable,
    #[error("the job's result is not what a job on this input's encrypted values returns")]
    NotItsResult,
}

/// Makes a key for one job and encrypts every private value in the job's input
/// under it.
pub fn encrypt(signature: &Signature, input: &[u8]) -> Result<Encrypted, EncryptError> {
    if !signature.is_encrypted() {
        return Err(EncryptError::NotEncryptable);
    }
    signature.check_input(input).map_err(EncryptError::Input)?;
    let input: Value = serde_json::from_slice(input).expect("the input was just read as JSON");

    let key = ClientKey::new(&DEFAULT_PARAMETERS);
    let computing_key = encode(&CompressedServerKey::new(&key));
    let name = ContentHash::of(&computing_key);
    let bit_size = match key.encrypt(false) {
        Ciphertext::Encrypted(bit) => bit.lwe_size(),
        Ciphertext::Trivial(_) => unreachable!("a key encrypts what it is given"),
    };
    let key = EncryptionKey {
        key,
        name,
        bit_size,
    };

    let input = each_private(
        &Schema::Tuple(signature.input.clone()),
        &input,
        &|kind, value| {
            Some(Value::String(match (kind, value) {
                (Schema::EncryptedNumber, Value::Number(number)) => {
                    let number = number.as_u64().expect("a private number is a whole number");
                    let bits = (0..ENCRYPTED_NUMBER_BITS).map(|place| number >> place & 1 == 1);
                    let bits: Vec<CompressedCiphertext> =
                        bits.map(|bit| key.key.encrypt_compressed(bit)).collect();
                    key.travelling(&bits)
                }
                (Schema::EncryptedBool, Value::Bool(value)) => {
                    key.travelling(&key.key.encrypt_compressed(*value))
                }
                _ => unreachable!("the input was checked against the job's types"),
            }))
        },
    );
    Ok(Encrypted {
        input: to_json(&input.expect("every private value was encrypted")),
        computing_key,
        key,
    })
}

/// Reads the result of a job that ran on input encrypted under `key`: every
/// private value in it is put back as the plain value it holds.
pub fn decrypt(
    signature: &Signature,
    key: &EncryptionKey,
    output: &[u8],
) -> Result<Vec<u8>, EncryptError> {
    let output: Value = serde_json::from_slice(output).map_err(|_| EncryptError::NotItsResult)?;
    let output = each_private(&signature.output, &output, &|kind, value| {
        match (kind, value) {
            // A module returns a value as it is when nothing encrypted went
            // into it.
            (Schema::EncryptedNumber, Value::Number(_))
            | (Schema::EncryptedBool, Value::Bool(_)) => Some(value.clone()),
            (Schema::EncryptedNumber, Value::String(text)) => {
                let bits: Vec<Ciphertext> = key.arrived(text)?;
                key.number(&bits).map(Value::from)
            }
            (Schema::EncryptedBool, Value::String(text)) => {
                Some(Value::Bool(key.read(&key.arrived(text)?)?))
            }
            _ => None,
        }
    });
    output
        .as_ref()
        .map(to_json)
        .ok_or(EncryptError::NotItsResult)
}

impl EncryptionKey {
    /// An encrypted value as it travels: the key's name, then the value.
    fn travelling<Bits: Serialize>(&self, bits: &Bits) -> String {
        let mut bytes = self.name.as_bytes().to_vec();
        bytes.extend(encode(bits));
        BASE64.encode(bytes)
    }

    /// An encrypted value that came back from a job, if it is under this key
    /// and in the form a value has.
    fn arrived<Bits: DeserializeOwned>(&self, text: &str) -> Option<Bits> {
        let bytes = BASE64.decode(text).ok()?;
        let bits = bytes.strip_prefix(self.name.as_bytes())?;
        bincode::deserialize(bits).ok()
    }

    /// The number whose encrypted bits these are, lowest bit first.
    fn number(&self, bits: &[Ciphertext]) -> Option<u64> {
        if bits.len() != ENCRYPTED_NUMBER_BITS as usize {
            return None;
        }
        let mut highest_first = bits.iter().rev();
        highest_first.try_fold(0, |number, bit| {
            Some(number << 1 | u64::from(self.read(bit)?))
        })
    }

    /// The bit an encrypted bit holds. `None` for one that is not the length
    /// this key's bits are, which the library would stop the program over.
    fn read(&self, bit: &Ciphertext) -> Option<bool> {
        match bit {
            Ciphertext::Encrypted(encrypted) if encrypted.lwe_size() != self.bit_size => None,
            bit => Some(self.key.decrypt(bit)),
        }
    }
}

/// The byte form the library's types travel in.
fn encode<Value: Serialize>(value: &Value) -> Vec<u8> {
    bincode::serialize(value).expect("the library's types can be written out")
}

fn to_json(value: &Value) -> Vec<u8> {
    serde_json::to_vec(value).expect("a JSON value can be written out")
}

/// `value` with each private value in it that `schema` says can be computed on
/// while encrypted replaced by what `change` makes of it. `None` if `change`
/// refuses one, or if `value` holds a field that `schema` does not.
fn each_private(
    schema: &Schema,
    value: &Value,
    change: &dyn Fn(&Schema, &Value) -> Option<Value>,
) -> Option<Value> {
    match (schema, value) {
        (Schema::EncryptedNumber | Schema::EncryptedBool, value) => change(schema, value),
        (Schema::Option(inner), value) if !value.is_null() => each_private(inner, value, change),
        (Schema::List(item), Value::Array(values)) => {
            let changed = values.iter().map(|value| each_private(item, value, change));
            changed.collect::<Option<Vec<_>>>().map(Value::Array)
        }
        (Schema::Tuple(items), Value::Array(values)) if items.len() == values.len() => {
            let pairs = items.iter().zip(values);
            let changed = pairs.map(|(item, value)| each_private(item, value, change));
            changed.collect::<Option<Vec<_>>>().map(Value::Array)
        }
        (Schema::Struct(fields), Value::Object(object)) => {
            let mut changed = Map::new();
            for (name, value) in object {
                changed.insert(
                    name.clone(),
                    each_private(fields.get(name)?, value, change)?,
                );
            }
            Some(Value::Object(changed))
        }
        _ => Some(value.clone()),
    }
}
