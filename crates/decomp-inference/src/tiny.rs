//! The tiny approved models: small transformers whose weights are made from a
//! published seed, so anyone can rebuild them and check that their roots are
//! the IDs `decomp_core::APPROVED_MODELS` lists. Their tokens are bytes.
//!
//! Their weights are random, so what they write is noise. They exist so that
//! inference jobs, TOPLOC proofs, and the checks of both run on any machine,
//! with no download and no GPU.

use sha2::{Digest, Sha256};

use crate::model::{Config, Model};

/// Bytes are the tokens.
const BYTE_VOCAB: usize = 256;

/// The tiny models, by the name the approved list gives them, with their
/// shapes and the seed their weights are drawn from.
pub const MODELS: &[(&str, Config)] = &[
    (
        "tiny-1",
        Config {
            vocab: BYTE_VOCAB,
            dim: 64,
            layers: 2,
            heads: 4,
            hidden: 256,
            positions: 1024,
        },
    ),
    (
        "tiny-2",
        Config {
            vocab: BYTE_VOCAB,
            dim: 128,
            layers: 4,
            heads: 8,
            hidden: 512,
            positions: 1024,
        },
    ),
];

/// How large embeddings are drawn, against one for a matrix's usual bound.
/// Small embeddings leave the hidden state to what attention and the
/// feed-forward layers compute from the context, as in a trained model.
const EMBEDDING_GAIN: f32 = 0.1;

/// How much larger query and key weights are drawn than the usual bound. A
/// randomly drawn model otherwise attends to every token nearly evenly, and
/// its hidden states hardly depend on the context; trained models attend
/// sharply.
const ATTENTION_GAIN: f32 = 4.0;

/// A stream of numbers drawn from a seed (SplitMix64), the same everywhere.
struct Draws(u64);

impl Draws {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A number spread evenly over [-bound, bound].
    fn spread(&mut self, bound: f32) -> f32 {
        let unit = (self.next() >> 40) as f32 / (1u64 << 24) as f32;
        (unit * 2.0 - 1.0) * bound
    }
}

/// Builds a model of shape `config` from the seed `seed`: matrices drawn
/// evenly within one over the square root of their inputs (four times that
/// for queries and keys), embeddings within `EMBEDDING_GAIN`, and
/// normalization gains of one.
pub fn seeded(config: Config, seed: &str) -> Model {
    let digest: [u8; 32] = Sha256::digest(seed.as_bytes()).into();
    let mut draws = Draws(u64::from_le_bytes(digest[..8].try_into().expect("8 bytes")));
    let mut matrix = |inputs: usize, outputs: usize, gain: f32| -> Vec<f32> {
        let bound = gain / (inputs as f32).sqrt();
        (0..inputs * outputs).map(|_| draws.spread(bound)).collect()
    };
    let Config {
        vocab,
        dim,
        hidden,
        positions,
        ..
    } = config;
    let mut tensors = vec![
        matrix(1, vocab * dim, EMBEDDING_GAIN),
        matrix(1, positions * dim, EMBEDDING_GAIN),
    ];
    for _ in 0..config.layers {
        tensors.push(vec![1.0; dim]);
        tensors.push(matrix(dim, dim, ATTENTION_GAIN));
        tensors.push(matrix(dim, dim, ATTENTION_GAIN));
        tensors.push(matrix(dim, dim, 1.0));
        tensors.push(matrix(dim, dim, 1.0));
        tensors.push(vec![1.0; dim]);
        tensors.push(matrix(dim, hidden, 1.0));
        tensors.push(matrix(hidden, dim, 1.0));
    }
    tensors.push(vec![1.0; dim]);
    tensors.push(matrix(dim, vocab, 1.0));
    Model::from_tensors(config, tensors).expect("the shapes are the configuration's")
}

/// The tiny model named `name`, built from its published seed:
/// `decomp/tiny-model/<name>`.
pub fn build(name: &str) -> Option<Model> {
    let (_, config) = MODELS.iter().find(|(known, _)| *known == name)?;
    Some(seeded(*config, &format!("decomp/tiny-model/{name}")))
}

/// Bytes as tokens.
pub fn encode(bytes: &[u8]) -> Vec<u32> {
    bytes.iter().map(|&b| u32::from(b)).collect()
}

/// Tokens as bytes. Every token of a tiny model is a byte.
pub fn decode(tokens: &[u32]) -> Vec<u8> {
    tokens.iter().map(|&t| t as u8).collect()
}
