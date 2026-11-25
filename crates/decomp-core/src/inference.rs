//! Inference jobs: the approved models a job may call, how tokens are
//! sampled so that a validator can check them, and the transcript a node
//! keeps of a job's model calls (spec section 2, "Inference: Model generation,
//! embeddings, ranking with a model"; section 4, "TOPLOC proof"; section 7,
//! "Models: `model.embed`, `model.generate`. Approved models only, identified
//! by hash; inference class").
//!
//! A job calls a model through the host. The first node to run it generates,
//! and keeps a transcript: for each call, the model's output and the TOPLOC
//! proofs of it (`toploc`). Any node that runs the job after it, to confirm
//! it, to settle a dispute, or as a known-answer check, is handed the
//! transcript and replays the job against it: each call answers with the
//! recorded output once the node has checked, by running the model once over
//! the prompt and the output, that the proofs hold and each token is one the
//! model could have sampled. A check that fails ends the run with
//! `Failure::ProofRejected`, which differs from the first node's result, so
//! the job is disputed and settled by referees who replay it too.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::keys::ContentHash;
use crate::request::Blob;

/// A model jobs may call: identified by the root (`merkle::root`) of its
/// weights file, which a node checks before it runs it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ApprovedModel {
    pub id: ContentHash,
    pub name: &'static str,
    /// What one token the model processes, of the prompt or generated, is
    /// charged, in WebAssembly instructions, so that inference is billed in
    /// compute units like everything else (spec section 5: inference is
    /// metered by "Tokens processed, by model").
    pub instructions_per_token: u64,
    /// Whether the model needs a GPU to run in reasonable time. A job that
    /// calls one is offered only to nodes that declare a GPU.
    pub needs_gpu: bool,
}

/// The approved models.
///
/// The `tiny` models are small transformers whose weights are made from a
/// published seed (`decomp_inference::tiny`), so anyone can rebuild them and
/// check their roots. They write nothing worth reading; they exist so that
/// inference jobs, TOPLOC proofs, and their checks run on any machine.
pub const APPROVED_MODELS: &[ApprovedModel] = &[
    ApprovedModel {
        id: hash_const(TINY_1_ID),
        name: "tiny-1",
        instructions_per_token: 2_000_000,
        needs_gpu: false,
    },
    ApprovedModel {
        id: hash_const(TINY_2_ID),
        name: "tiny-2",
        instructions_per_token: 10_000_000,
        needs_gpu: false,
    },
];

const TINY_1_ID: &str = "db50495579d7cda34584c112f1d2954e37dd492aa5628b31f35c052a63718ba8";
const TINY_2_ID: &str = "067fa04bc2289b203b55c173407f7a2419c65375a4c31e0a4a9a501292147c18";

const fn hex_digit(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        _ => panic!("not a lowercase hex digit"),
    }
}

const fn hash_const(hex: &str) -> ContentHash {
    let text = hex.as_bytes();
    let mut bytes = [0u8; 32];
    let mut i = 0;
    while i < 32 {
        bytes[i] = hex_digit(text[2 * i]) << 4 | hex_digit(text[2 * i + 1]);
        i += 1;
    }
    ContentHash::from_bytes(bytes)
}

/// The approved model with this ID, if there is one.
pub fn approved(id: &ContentHash) -> Option<&'static ApprovedModel> {
    APPROVED_MODELS.iter().find(|model| model.id == *id)
}

/// The most tokens one call may ask a model to generate.
pub const MAX_GENERATED_TOKENS: u32 = 4096;

/// How far a sampled token's score may fall short of the best, for a
/// validator to accept it: the room left for arithmetic that differs between
/// machines. Scores are logits divided by the temperature, plus noise.
pub const SAMPLING_TOLERANCE: f32 = 0.05;

/// How a call samples: `temperature` in thousandths, zero for always taking
/// the most likely token.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sampling {
    pub temperature_milli: u32,
    /// What the call's noise is drawn from: the job's seed and the call's
    /// place among the job's calls (`call_seed`), so every node draws the same.
    pub seed: [u8; 32],
}

/// The seed of a job's call number `index`, from the job's own seed.
pub fn call_seed(job_seed: &[u8; 32], index: u32) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"decomp/model-call/v1\n");
    hasher.update(job_seed);
    hasher.update(index.to_le_bytes());
    hasher.finalize().into()
}

/// A uniform number in (0, 1) for token `token` at step `step` of a call.
fn uniform(seed: &[u8; 32], step: u32, token: u32) -> f64 {
    let mut hasher = Sha256::new();
    hasher.update(seed);
    hasher.update(step.to_le_bytes());
    hasher.update((token / 4).to_le_bytes());
    let block: [u8; 32] = hasher.finalize().into();
    let at = (token % 4) as usize * 8;
    let bits = u64::from_le_bytes(block[at..at + 8].try_into().expect("8 bytes"));
    // 53 random bits, kept away from both ends.
    ((bits >> 11) as f64 + 0.5) / (1u64 << 53) as f64
}

/// Each token's score at step `step`: its logit over the temperature plus
/// Gumbel noise, so that taking the highest score samples from the model's
/// distribution. With no temperature, the logits themselves.
fn scores(logits: &[f32], sampling: &Sampling, step: u32) -> Vec<f32> {
    if sampling.temperature_milli == 0 {
        return logits.to_vec();
    }
    let temperature = sampling.temperature_milli as f32 / 1000.0;
    logits
        .iter()
        .enumerate()
        .map(|(token, logit)| {
            let noise = -(-uniform(&sampling.seed, step, token as u32).ln()).ln();
            logit / temperature + noise as f32
        })
        .collect()
}

/// The token sampled at step `step` (counting generated tokens from zero)
/// from the model's logits.
pub fn sample(logits: &[f32], sampling: &Sampling, step: u32) -> u32 {
    let scores = scores(logits, sampling, step);
    let mut best = 0;
    for (token, score) in scores.iter().enumerate() {
        if *score > scores[best] {
            best = token;
        }
    }
    best as u32
}

/// Whether `token` could have been sampled at step `step`: its score is
/// within `SAMPLING_TOLERANCE` of the best, by the validator's own logits.
pub fn sampled(logits: &[f32], sampling: &Sampling, step: u32, token: u32) -> bool {
    let scores = scores(logits, sampling, step);
    let Some(score) = scores.get(token as usize) else {
        return false;
    };
    let best = scores.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    *score >= best - SAMPLING_TOLERANCE
}

/// One model call of a job, as the node that ran it first recorded it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecordedCall {
    pub model: ContentHash,
    #[serde(flatten)]
    pub answer: RecordedAnswer,
    /// The TOPLOC proofs, each `toploc::PROOF_BYTES` long: of the prompt,
    /// then, for a generation, one per `CHUNK_TOKENS` tokens generated.
    pub proofs: Vec<Blob>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecordedAnswer {
    /// The tokens the model generated.
    Generate { tokens: Vec<u32> },
    /// The embedding: the mean of the text's last hidden states.
    Embed { embedding: Vec<f32> },
}

/// How far each value of an embedding may differ from a validator's, as a
/// share of the validator's largest value: room for arithmetic that differs
/// between machines.
pub const EMBEDDING_TOLERANCE: f32 = 0.01;

/// Whether a claimed embedding is close enough to the validator's own.
pub fn embedding_close(claimed: &[f32], ours: &[f32]) -> bool {
    let largest = ours.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    claimed.len() == ours.len()
        && claimed
            .iter()
            .zip(ours)
            .all(|(a, b)| (a - b).abs() <= EMBEDDING_TOLERANCE * largest)
}

/// A job's model calls, in the order it made them. It is stored as a blob,
/// and the result of the job names its root.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Transcript {
    pub calls: Vec<RecordedCall>,
}
