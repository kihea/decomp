//! How the types of a job's inputs and outputs are described and checked (spec
//! section 7, "Typing").
//!
//! A job module built with the SDK can describe its own signature. The
//! coordinator reads that description when a job is submitted and refuses an
//! input that does not fit it, before any node runs anything. The SDK and the
//! coordinator both use this crate, so they cannot disagree about what a
//! description means.
//!
//! Typed inputs and outputs travel as JSON, so a schema describes JSON values.
//!
//! A description also says which of the two privacy modes the job was written
//! for, and which of its values are private (spec section 7, "Privacy modes").
//! The kinds of private value a job takes and returns decide which routes can
//! run it away from its user's device: see `Signature::is_totals` and
//! `Signature::is_encrypted`.

pub mod records;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The shape of one value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Schema {
    Bool,
    /// A whole number.
    Integer,
    /// Any number.
    Float,
    String,
    /// Raw bytes, carried as base64 text.
    Bytes,
    List(Box<Schema>),
    /// The value, or null.
    Option(Box<Schema>),
    /// A fixed number of values of given types, carried as an array.
    Tuple(Vec<Schema>),
    /// Named fields. Every field must be present; fields not named here are an error.
    Struct(BTreeMap<String, Schema>),
    /// One of a fixed set of names, carried as a string.
    Enum(Vec<String>),
    /// A value marked private: no node may see it as it is. It is carried as the
    /// value inside is.
    Private(Box<Schema>),
    /// A private whole number, from 0 up to but not including
    /// `PRIVATE_NUMBER_MODULUS`. Unlike other private values it can be cut into
    /// shares: numbers that each say nothing about it and that add up to it. A
    /// node may be handed one share.
    PrivateNumber,
    /// A private whole number, from 0 up to but not including 2 to the power
    /// `ENCRYPTED_NUMBER_BITS`, that a job can compute on while it is encrypted
    /// (spec section 7: "Encrypted compute uses homomorphic encryption, so a
    /// node computes on values it cannot read"). It is carried as a number
    /// where its user's device holds it, and as base64 text where it is
    /// encrypted, which is the only form a node may be handed.
    EncryptedNumber,
    /// A private true-or-false value that a job can compute on while it is
    /// encrypted. It is carried as `true` or `false` where its user's device
    /// holds it, and as base64 text where it is encrypted.
    EncryptedBool,
}

/// One more than the largest private number. Sums of private numbers wrap around
/// at it. It is 2^53 because a share is as likely to be large as small, and
/// every whole number below 2^53 comes through any JSON reader unchanged, also
/// one that keeps all numbers as 64-bit floating point.
pub const PRIVATE_NUMBER_MODULUS: u64 = 1 << 53;

/// How many bits an encrypted private number has. Arithmetic on one wraps
/// around at 2 to this power. What an operation on an encrypted number costs
/// grows with its bits, so this also bounds what a job run on encrypted values
/// costs for each operation.
pub const ENCRYPTED_NUMBER_BITS: u32 = 32;

/// How many bytes name the key an encrypted value is under. Each encrypted
/// value starts with them, so that the job computing on it can find the key
/// among the blobs it was given to read: the name is the hash of the blob that
/// holds the key.
pub const ENCRYPTION_KEY_NAME_BYTES: usize = 32;

/// The mode a job runs in (spec section 2, the manifest's `privacy` field: "open
/// or private"). An open job's data can be read by the nodes that run it. Of a
/// private job the spec promises that "no node ever sees the data unencrypted".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Privacy {
    #[default]
    Open,
    Private,
}

impl Privacy {
    pub fn is_open(&self) -> bool {
        *self == Self::Open
    }
}

/// The inputs a job takes, in order, and the output it produces. A job's input is
/// a JSON array holding one value per entry of `input`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signature {
    pub input: Vec<Schema>,
    pub output: Schema,
    /// The mode the job was written for. A module that says nothing is open.
    #[serde(default, skip_serializing_if = "Privacy::is_open")]
    pub privacy: Privacy,
}

/// An input that does not fit a job's signature.
#[derive(Debug, PartialEq, Eq)]
pub enum Mismatch {
    /// The input is not JSON at all.
    NotJson,
    /// The value at this path has the wrong shape. The path is written the way
    /// the value would be reached, such as `[1].title`.
    WrongShape(String),
}

impl std::fmt::Display for Mismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotJson => f.write_str("the input is not valid JSON"),
            Self::WrongShape(path) => {
                write!(
                    f,
                    "the input does not fit the job's signature at `input{path}`"
                )
            }
        }
    }
}

impl Signature {
    /// Checks a job's input against the types the job takes, as the input is
    /// on its user's device, and as nodes are handed it unless the job is run
    /// by encrypted compute.
    pub fn check_input(&self, input: &[u8]) -> Result<(), Mismatch> {
        self.check(input, Form::Clear)
    }

    /// Checks a job's input as nodes are handed it by encrypted compute: it
    /// must fit the types the job takes, with every private value in it in its
    /// encrypted form.
    pub fn check_encrypted_input(&self, input: &[u8]) -> Result<(), Mismatch> {
        self.check(input, Form::Encrypted)
    }

    fn check(&self, input: &[u8], form: Form) -> Result<(), Mismatch> {
        let value: Value = serde_json::from_slice(input).map_err(|_| Mismatch::NotJson)?;
        Schema::Tuple(self.input.clone()).check(&value, form, &mut String::new())
    }

    /// Whether the job takes or returns private numbers and no private value of
    /// any other kind. All that can be done with a private number is to add it
    /// to another and to multiply it by a number that is not private. So such a
    /// job "only adds, counts, averages, or applies fixed weights" (spec section
    /// 7), and run on shares of its input it gives shares of its output.
    pub fn is_totals(&self) -> bool {
        self.holds(&|part| *part == Schema::PrivateNumber)
            && !self.holds(&|part| matches!(part, Schema::Private(_)) || part.is_encrypted())
    }

    /// Whether the job takes or returns private values that can be computed on
    /// while encrypted, and no private value of any other kind. Such a job can
    /// be run on encrypted values: "The code must use the SDK's private number
    /// and true-or-false types" (spec section 7, "Limits of 'general'").
    pub fn is_encrypted(&self) -> bool {
        self.holds(&Schema::is_encrypted)
            && !self.holds(&|part| matches!(part, Schema::Private(_) | Schema::PrivateNumber))
    }

    /// Whether anything the job takes or returns passes `test`.
    fn holds(&self, test: &dyn Fn(&Schema) -> bool) -> bool {
        let mut values = self.input.iter().chain([&self.output]);
        values.any(|value| value.holds(test))
    }
}

/// The form a job's private values are in when its input is checked.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Form {
    /// As the values are, which is how their user's device holds them.
    Clear,
    /// Encrypted, which is how nodes are handed a job run by encrypted compute.
    Encrypted,
}

impl Schema {
    /// Whether this is a private value that can be computed on while encrypted.
    fn is_encrypted(&self) -> bool {
        matches!(self, Self::EncryptedNumber | Self::EncryptedBool)
    }

    /// Whether this schema, or any schema it is built from, passes `test`.
    fn holds(&self, test: &dyn Fn(&Schema) -> bool) -> bool {
        test(self)
            || match self {
                Self::List(inner) | Self::Option(inner) | Self::Private(inner) => inner.holds(test),
                Self::Tuple(items) => items.iter().any(|item| item.holds(test)),
                Self::Struct(fields) => fields.values().any(|field| field.holds(test)),
                _ => false,
            }
    }

    /// Checks `value` against this schema. `path` is where `value` sits in the
    /// input; on a mismatch it is extended to name the part that is wrong.
    fn check(&self, value: &Value, form: Form, path: &mut String) -> Result<(), Mismatch> {
        let fits = match (self, value) {
            // An encrypted value is base64 text, and must be nothing else where
            // the input is on its way to nodes.
            (Self::EncryptedNumber | Self::EncryptedBool, Value::String(text)) => {
                form == Form::Encrypted && is_base64(text)
            }
            (Self::EncryptedNumber | Self::EncryptedBool, _) if form == Form::Encrypted => false,
            (Self::EncryptedNumber, Value::Number(number)) => number
                .as_u64()
                .is_some_and(|number| number >> ENCRYPTED_NUMBER_BITS == 0),
            (Self::Bool | Self::EncryptedBool, Value::Bool(_)) => true,
            (Self::Integer, Value::Number(number)) => number.is_i64() || number.is_u64(),
            (Self::PrivateNumber, Value::Number(number)) => number
                .as_u64()
                .is_some_and(|number| number < PRIVATE_NUMBER_MODULUS),
            (Self::Float, Value::Number(_)) => true,
            (Self::String, Value::String(_)) => true,
            (Self::Bytes, Value::String(text)) => is_base64(text),
            (Self::Enum(names), Value::String(name)) => names.contains(name),
            (Self::Option(_), Value::Null) => true,
            (Self::Option(inner) | Self::Private(inner), value) => {
                return inner.check(value, form, path);
            }
            (Self::List(item), Value::Array(values)) => {
                let pairs = values.iter().map(|value| (item.as_ref(), value));
                return each(pairs, form, path);
            }
            (Self::Tuple(items), Value::Array(values)) if items.len() == values.len() => {
                return each(items.iter().zip(values), form, path);
            }
            (Self::Struct(fields), Value::Object(object)) if fields.len() == object.len() => {
                for (name, field) in fields {
                    let Some(value) = object.get(name) else {
                        break;
                    };
                    let length = path.len();
                    path.push('.');
                    path.push_str(name);
                    field.check(value, form, path)?;
                    path.truncate(length);
                }
                fields.keys().all(|name| object.contains_key(name))
            }
            _ => false,
        };
        if fits {
            Ok(())
        } else {
            Err(Mismatch::WrongShape(path.clone()))
        }
    }
}

/// Checks each value of an array against its schema.
fn each<'a>(
    pairs: impl Iterator<Item = (&'a Schema, &'a Value)>,
    form: Form,
    path: &mut String,
) -> Result<(), Mismatch> {
    for (index, (schema, value)) in pairs.enumerate() {
        let length = path.len();
        path.push_str(&format!("[{index}]"));
        schema.check(value, form, path)?;
        path.truncate(length);
    }
    Ok(())
}

fn is_base64(text: &str) -> bool {
    let body = text.trim_end_matches('=');
    text.len().is_multiple_of(4)
        && text.len() - body.len() <= 2
        && body
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'+' || byte == b'/')
}
