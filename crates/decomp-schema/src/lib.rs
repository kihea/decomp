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
}

/// The inputs a job takes, in order, and the output it produces. A job's input is
/// a JSON array holding one value per entry of `input`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signature {
    pub input: Vec<Schema>,
    pub output: Schema,
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
}

impl Schema {
    /// Checks `value` against this schema. `path` is where `value` sits in the
    /// input; on a mismatch it is extended to name the part that is wrong.
    fn check(&self, value: &Value, path: &mut String) -> Result<(), Mismatch> {
        let fits = match (self, value) {
            (Self::Bool, Value::Bool(_)) => true,
            (Self::Integer, Value::Number(number)) => number.is_i64() || number.is_u64(),
            (Self::Float, Value::Number(_)) => true,
            (Self::String, Value::String(_)) => true,
            (Self::Bytes, Value::String(text)) => is_base64(text),
            (Self::Enum(names), Value::String(name)) => names.contains(name),
            (Self::Option(_), Value::Null) => true,
            (Self::Option(inner), value) => return inner.check(value, path),
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
