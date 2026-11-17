//! Shares: how a private job's numbers reach nodes without any node seeing one
//! (spec section 7: "Private totals splits each input into shares held by nodes
//! on different accounts").
//!
//! A private number is cut into shares: whole numbers below
//! `PRIVATE_NUMBER_MODULUS`, all but one drawn at random, that add up to the
//! number when the sum wraps around at the modulus. Any set of shares short of
//! all of them is just random numbers and says nothing about the number.
//!
//! The device that wants a job run cuts the job's input into as many inputs as
//! there are shares. Each holds one share of every private number, and
//! everything that is not private as it was. Each input is submitted as a job
//! of its own on the same module, all carrying one name (`Manifest::share`), and
//! the job rules never hand two jobs with one name to the same account.
//! Each job is assigned, run, checked, and paid for like any other: a share is
//! ordinary bytes to a job, so verification is unchanged.
//!
//! A job that only adds private numbers and multiplies them by numbers that are
//! not private gives, on shares of its input, shares of its output. The device
//! adds the outputs together and has the result. No node, and no account, saw
//! more than one share of anything.
//!
//! What the nodes do see: every value that is not marked private, and how many
//! private numbers there are.

use decomp_schema::{Mismatch, PRIVATE_NUMBER_MODULUS, Schema, Signature};
use serde_json::{Map, Value};

/// How many shares a device cuts each private number into. Each share's job is
/// checked like any job, so two shares cost twice what the job would cost open,
/// which is the spec's figure for this route: "About 2x". More shares would
/// keep a number private from more accounts working together, at that many
/// times the price.
pub const SHARES: usize = 2;

#[derive(Debug, thiserror::Error)]
pub enum ShareError {
    #[error("{0}")]
    Input(Mismatch),
    #[error("the job's private values are not all numbers that can be cut into shares")]
    NotTotals,
    #[error("the results of the job's shares do not fit together")]
    Mismatched,
}

/// Cuts a job's input into `SHARES` inputs. Each private number in it becomes
/// one share in each; everything else is the same in all of them.
pub fn cut(signature: &Signature, input: &[u8]) -> Result<Vec<Vec<u8>>, ShareError> {
    if !signature.is_totals() {
        return Err(ShareError::NotTotals);
    }
    signature.check_input(input).map_err(ShareError::Input)?;
    let input: Value = serde_json::from_slice(input).expect("the input was just read as JSON");
    let inputs = cut_value(&Schema::Tuple(signature.input.clone()), &input);
    Ok(inputs.iter().map(encode).collect())
}

/// Adds together the outputs of a job's shares: private numbers are summed, and
/// everything else must be the same in every output, as it is when each share
/// was run by the same module.
pub fn join(signature: &Signature, outputs: &[Vec<u8>]) -> Result<Vec<u8>, ShareError> {
    let outputs: Option<Vec<Value>> = outputs
        .iter()
        .map(|output| serde_json::from_slice(output).ok())
        .collect();
    let outputs = outputs.filter(|outputs| !outputs.is_empty());
    let outputs = outputs.ok_or(ShareError::Mismatched)?;
    let joined = join_values(&signature.output, &outputs.iter().collect::<Vec<_>>());
    joined.as_ref().map(encode).ok_or(ShareError::Mismatched)
}

fn encode(value: &Value) -> Vec<u8> {
    serde_json::to_vec(value).expect("a JSON value can be written out")
}

/// One value for each share. `value` has already been checked against `schema`.
fn cut_value(schema: &Schema, value: &Value) -> Vec<Value> {
    match (schema, value) {
        (Schema::PrivateNumber, Value::Number(number)) => {
            let number = number.as_u64().expect("a private number is a whole number");
            shares_of(number).into_iter().map(Value::from).collect()
        }
        (Schema::Option(inner), value) if !value.is_null() => cut_value(inner, value),
        (Schema::List(item), Value::Array(values)) => {
            lists(values.iter().map(|value| cut_value(item, value)))
        }
        (Schema::Tuple(items), Value::Array(values)) => lists(
            items
                .iter()
                .zip(values)
                .map(|(item, value)| cut_value(item, value)),
        ),
        (Schema::Struct(fields), Value::Object(object)) => {
            let mut objects = vec![Map::new(); SHARES];
            for (name, value) in object {
                let parts = cut_value(&fields[name], value);
                for (object, part) in objects.iter_mut().zip(parts) {
                    object.insert(name.clone(), part);
                }
            }
            objects.into_iter().map(Value::Object).collect()
        }
        _ => vec![value.clone(); SHARES],
    }
}

/// Given, for each value of a list, that value's part in each share: the list
/// each share gets.
fn lists(values: impl Iterator<Item = Vec<Value>>) -> Vec<Value> {
    let mut lists = vec![Vec::new(); SHARES];
    for parts in values {
        for (list, part) in lists.iter_mut().zip(parts) {
            list.push(part);
        }
    }
    lists.into_iter().map(Value::Array).collect()
}

/// Shares of one private number: all but the last drawn at random, and the last
/// what must be added to them to reach the number.
fn shares_of(number: u64) -> Vec<u64> {
    let mut shares: Vec<u64> = (1..SHARES).map(|_| random_number()).collect();
    let drawn = shares.iter().fold(0, |sum, share| add(sum, *share));
    // Taking away wraps around at 2^64, and the modulus divides that evenly.
    shares.push(number.wrapping_sub(drawn) % PRIVATE_NUMBER_MODULUS);
    shares
}

/// A whole number below the modulus, each as likely as any other.
fn random_number() -> u64 {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).expect("the operating system provides randomness");
    // The modulus is a power of two, so the remainder is as even as the bytes.
    u64::from_le_bytes(bytes) % PRIVATE_NUMBER_MODULUS
}

fn add(a: u64, b: u64) -> u64 {
    a.wrapping_add(b) % PRIVATE_NUMBER_MODULUS
}

/// One value from the value each share produced, or `None` if they do not fit
/// the schema or one another.
fn join_values(schema: &Schema, parts: &[&Value]) -> Option<Value> {
    match schema {
        Schema::PrivateNumber => {
            let shares: Option<Vec<u64>> = parts.iter().map(|part| part.as_u64()).collect();
            Some(Value::from(shares?.into_iter().fold(0, add)))
        }
        Schema::Option(inner) if parts.iter().all(|part| !part.is_null()) => {
            join_values(inner, parts)
        }
        Schema::List(item) => {
            let length = parts[0].as_array()?.len();
            join_arrays(&vec![item.as_ref(); length], parts)
        }
        Schema::Tuple(items) => join_arrays(&items.iter().collect::<Vec<_>>(), parts),
        Schema::Struct(fields) => {
            let mut object = Map::new();
            for (name, field) in fields {
                let column: Option<Vec<&Value>> = parts.iter().map(|part| part.get(name)).collect();
                object.insert(name.clone(), join_values(field, &column?)?);
            }
            Some(Value::Object(object))
        }
        _ => parts
            .iter()
            .all(|part| part == &parts[0])
            .then(|| parts[0].clone()),
    }
}

/// Joins the array each share produced, place by place. `items` is the schema
/// of each place.
fn join_arrays(items: &[&Schema], parts: &[&Value]) -> Option<Value> {
    let arrays: Option<Vec<&Vec<Value>>> = parts.iter().map(|part| part.as_array()).collect();
    let arrays = arrays.filter(|arrays| arrays.iter().all(|array| array.len() == items.len()))?;
    let joined: Option<Vec<Value>> = (items.iter().enumerate())
        .map(|(index, item)| {
            let column: Vec<&Value> = arrays.iter().map(|array| &array[index]).collect();
            join_values(item, &column)
        })
        .collect();
    joined.map(Value::Array)
}
