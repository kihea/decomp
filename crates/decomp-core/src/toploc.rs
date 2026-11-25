//! TOPLOC proofs of inference (spec section 4: "The node commits to hashes of
//! the model's internal states; a validator recomputes and compares them").
//!
//! The method is from Ong et al., "TOPLOC: A Locality Sensitive Hashing
//! Scheme for Trustless Verifiable Inference" (2025), which the spec's
//! INTELLECT-2 source uses. A node that generates text keeps, for the prompt
//! and for every `CHUNK_TOKENS` tokens it generates, the `TOP_K` largest
//! values of the model's last hidden state, rounded to bfloat16. It encodes
//! them as a polynomial over a prime field whose value at each value's
//! position (taken modulo a number that keeps the positions apart) is the
//! value's bits, and commits to the polynomial's coefficients: 258 bytes per
//! chunk.
//!
//! A validator runs the model once over the prompt and the generated text,
//! which is far cheaper than generating it, takes the largest values of its
//! own hidden states, and reads the claimed values at those positions off the
//! polynomial. GPU arithmetic differs a little between machines, so the
//! values are compared with tolerance: the exponents must mostly agree, and
//! the mantissas must be close on average. A different model, a smaller or
//! more coarsely quantized copy of it, or a changed prompt or output moves the
//! largest values or their sizes, and fails.
//!
//! The paper warns of one thing this does not catch: a node that generates
//! with a cheap model and computes the proof by running the right model once
//! over the cheap model's text. That costs the node the validator's work on
//! top of its own, and the text it hands back is still what the right model
//! scores as plausible. The sampling check (`inference::sampled`) narrows it
//! further: each token must be one the right model could have chosen.

/// Tokens of generated text one proof covers. The prompt is covered by one
/// proof of its own.
pub const CHUNK_TOKENS: usize = 32;

/// Values of the hidden state a proof keeps, per chunk.
pub const TOP_K: usize = 128;

/// The prime the polynomial's coefficients are taken modulo: the largest
/// below 2^16, so each coefficient fits two bytes. Finite bfloat16 values are
/// below it as bits (`0xff80` is negative infinity).
const PRIME: u64 = 65_521;

/// The positions of the kept values are taken modulo the largest number in
/// this range that keeps them apart, so that each is below `PRIME`.
const LARGEST_MODULUS: u32 = 65_521;
const SMALLEST_MODULUS: u32 = 32_769;

/// The most kept values whose sign and exponent may differ from the
/// validator's, out of `TOP_K`, and the most the mantissas of the rest may
/// differ on average and in the median, in units of the last place of a
/// bfloat16 mantissa. The paper's thresholds for bfloat16.
pub const MAX_EXPONENT_MISMATCHES: usize = 38;
pub const MAX_MEAN_MANTISSA_DIFFERENCE: f64 = 10.0;
pub const MAX_MEDIAN_MANTISSA_DIFFERENCE: f64 = 8.0;

/// The bytes of one proof: the modulus, then `TOP_K` coefficients, each two
/// bytes, little-endian.
pub const PROOF_BYTES: usize = 2 + 2 * TOP_K;

/// A proof of one chunk: the modulus and the polynomial's coefficients,
/// lowest power first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proof {
    pub modulus: u16,
    pub coefficients: Vec<u16>,
}

/// How a validator's hidden states compared with a proof.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Comparison {
    pub exponent_mismatches: usize,
    pub mean_mantissa_difference: f64,
    pub median_mantissa_difference: f64,
}

impl Comparison {
    /// Whether the proof holds within the tolerances.
    pub fn holds(&self) -> bool {
        self.exponent_mismatches <= MAX_EXPONENT_MISMATCHES
            && self.mean_mantissa_difference <= MAX_MEAN_MANTISSA_DIFFERENCE
            && self.median_mantissa_difference <= MAX_MEDIAN_MANTISSA_DIFFERENCE
    }
}

/// A value rounded to bfloat16, as its bits: the top 16 bits of the `f32`,
/// rounded to nearest, ties to even.
pub fn bf16_bits(value: f32) -> u16 {
    let bits = value.to_bits();
    if value.is_nan() {
        return 0x7fc0;
    }
    let round = 0x7fff + ((bits >> 16) & 1);
    (bits.wrapping_add(round) >> 16) as u16
}

/// The positions of the `TOP_K` values largest in magnitude, ties going to
/// the earlier position. Fewer if there are fewer values.
fn top_positions(values: &[f32]) -> Vec<usize> {
    let mut positions: Vec<usize> = (0..values.len()).collect();
    positions.sort_by(|&a, &b| {
        values[b]
            .abs()
            .total_cmp(&values[a].abs())
            .then_with(|| a.cmp(&b))
    });
    positions.truncate(TOP_K);
    positions
}

fn power(mut base: u64, mut exponent: u64) -> u64 {
    let mut result = 1;
    base %= PRIME;
    while exponent > 0 {
        if exponent & 1 == 1 {
            result = result * base % PRIME;
        }
        base = base * base % PRIME;
        exponent >>= 1;
    }
    result
}

fn inverse(value: u64) -> u64 {
    power(value, PRIME - 2)
}

/// The coefficients, lowest power first, of the polynomial of lowest degree
/// through the points, whose x values are distinct and below `PRIME`.
fn interpolate(points: &[(u64, u64)]) -> Vec<u64> {
    let n = points.len();
    // Newton's divided differences.
    let mut differences: Vec<u64> = points.iter().map(|&(_, y)| y % PRIME).collect();
    for level in 1..n {
        for i in (level..n).rev() {
            let numerator = (differences[i] + PRIME - differences[i - 1]) % PRIME;
            let denominator = (points[i].0 + PRIME - points[i - level].0) % PRIME;
            differences[i] = numerator * inverse(denominator) % PRIME;
        }
    }
    // Expand the Newton form into powers of x, from the highest term down.
    let mut coefficients = vec![0u64; n];
    for i in (0..n).rev() {
        // coefficients = coefficients * (x - x_i) + differences[i]
        let x = points[i].0;
        let mut shifted = vec![0u64; n];
        for power in 0..n {
            let c = coefficients[power];
            if c == 0 {
                continue;
            }
            if power + 1 < n {
                shifted[power + 1] = (shifted[power + 1] + c) % PRIME;
            }
            shifted[power] = (shifted[power] + PRIME - c * x % PRIME) % PRIME;
        }
        shifted[0] = (shifted[0] + differences[i]) % PRIME;
        coefficients = shifted;
    }
    coefficients
}

fn evaluate(coefficients: &[u16], x: u64) -> u64 {
    coefficients
        .iter()
        .rev()
        .fold(0, |sum, &c| (sum * x + u64::from(c)) % PRIME)
}

/// The proof of one chunk of hidden states: the hidden state of each of its
/// tokens, one after another. `None` if no modulus keeps the kept positions
/// apart, which a chunk of fewer than `SMALLEST_MODULUS` values never needs.
pub fn prove(hidden: &[f32]) -> Option<Proof> {
    let positions = top_positions(hidden);
    let modulus = (SMALLEST_MODULUS..=LARGEST_MODULUS).rev().find(|&m| {
        let mut seen = std::collections::HashSet::new();
        positions.iter().all(|&p| seen.insert(p as u32 % m))
    })?;
    let points: Vec<(u64, u64)> = positions
        .iter()
        .map(|&p| {
            let x = u64::from(p as u32 % modulus);
            (x, u64::from(bf16_bits(hidden[p])))
        })
        .collect();
    let mut coefficients: Vec<u16> = interpolate(&points).into_iter().map(|c| c as u16).collect();
    coefficients.resize(TOP_K, 0);
    Some(Proof {
        modulus: modulus as u16,
        coefficients,
    })
}

/// Compares a validator's own hidden states for a chunk with the proof a
/// node gave for it.
pub fn compare(proof: &Proof, hidden: &[f32]) -> Comparison {
    let mut mismatches = 0;
    let mut differences = Vec::new();
    let modulus = u32::from(proof.modulus.max(1));
    for position in top_positions(hidden) {
        let x = u64::from(position as u32 % modulus);
        let claimed = evaluate(&proof.coefficients, x);
        let ours = u64::from(bf16_bits(hidden[position]));
        // The sign and exponent are the top nine bits; the mantissa the low seven.
        if claimed > u64::from(u16::MAX) || claimed >> 7 != ours >> 7 {
            mismatches += 1;
        } else {
            differences.push((claimed & 0x7f).abs_diff(ours & 0x7f));
        }
    }
    let (mean, median) = if differences.is_empty() {
        (f64::INFINITY, f64::INFINITY)
    } else {
        differences.sort_unstable();
        let mean = differences.iter().sum::<u64>() as f64 / differences.len() as f64;
        let middle = differences.len() / 2;
        let median = if differences.len() % 2 == 0 {
            (differences[middle - 1] + differences[middle]) as f64 / 2.0
        } else {
            differences[middle] as f64
        };
        (mean, median)
    };
    Comparison {
        exponent_mismatches: mismatches,
        mean_mantissa_difference: mean,
        median_mantissa_difference: median,
    }
}

impl Proof {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(PROOF_BYTES);
        bytes.extend_from_slice(&self.modulus.to_le_bytes());
        for c in &self.coefficients {
            bytes.extend_from_slice(&c.to_le_bytes());
        }
        bytes
    }

    /// Reads a proof from its bytes. `None` if they are not `PROOF_BYTES` long.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != PROOF_BYTES {
            return None;
        }
        let word = |i: usize| u16::from_le_bytes([bytes[i], bytes[i + 1]]);
        Some(Proof {
            modulus: word(0),
            coefficients: (0..TOP_K).map(|k| word(2 + 2 * k)).collect(),
        })
    }
}
