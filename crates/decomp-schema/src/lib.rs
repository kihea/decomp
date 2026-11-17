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
}

/// One more than the largest private number. Sums of private numbers wrap around
/// at it. It is 2^53 because a share is as likely to be large as small, and
/// every whole number below 2^53 comes through any JSON reader unchanged, also
/// one that keeps all numbers as 64-bit floating point.
pub const PRIVATE_NUMBER_MODULUS: u64 = 1 << 53;

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
    /// Checks a job's input against the types the job takes.
    pub fn check_input(&self, input: &[u8]) -> Result<(), Mismatch> {
        let value: Value = serde_json::from_slice(input).map_err(|_| Mismatch::NotJson)?;
        Schema::Tuple(self.input.clone()).check(&value, &mut String::new())
    }

    /// Whether the job takes or returns private numbers and no private value of
    /// any other kind. All that can be done with a private number is to add it
    /// to another and to multiply it by a number that is not private. So such a
    /// job "only adds, counts, averages, or applies fixed weights" (spec section
    /// 7), and run on shares of its input it gives shares of its output.
    pub fn is_totals(&self) -> bool {
        let values = || self.input.iter().chain([&self.output]);
        values().any(|value| value.holds(&|part| *part == Schema::PrivateNumber))
            && !values().any(|value| value.holds(&|part| matches!(part, Schema::Private(_))))
    }
}

impl Schema {
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
    fn check(&self, value: &Value, path: &mut String) -> Result<(), Mismatch> {
        let fits = match (self, value) {
            (Self::Bool, Value::Bool(_)) => true,
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
                return inner.check(value, path);
            }
            (Self::List(item), Value::Array(values)) => {
                return each(values.iter().map(|value| (item.as_ref(), value)), path);
            }
            (Self::Tuple(items), Value::Array(values)) if items.len() == values.len() => {
                return each(items.iter().zip(values), path);
            }
            (Self::Struct(fields), Value::Object(object)) if fields.len() == object.len() => {
                for (name, field) in fields {
                    let Some(value) = object.get(name) else {
                        break;
                    };
                    let length = path.len();
                    path.push('.');
                    path.push_str(name);
                    field.check(value, path)?;
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
    path: &mut String,
) -> Result<(), Mismatch> {
    for (index, (schema, value)) in pairs.enumerate() {
        let length = path.len();
        path.push_str(&format!("[{index}]"));
        schema.check(value, path)?;
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
