//! A decoder-only transformer: token and position embeddings, layers of
//! causal self-attention and a feed-forward network, each after RMS
//! normalization, and a final normalization whose output is the last hidden
//! state that TOPLOC proofs are made from.
//!
//! A weights file is a header line, `decomp-model/v1`, a line of the
//! configuration as JSON, and the weights as little-endian `f32` in the order
//! `Weights` lists them. The model's ID is the file's root.

use serde::{Deserialize, Serialize};

const MAGIC: &[u8] = b"decomp-model/v1\n";

/// Keeps RMS normalization from dividing by zero.
const NORM_EPSILON: f32 = 1e-5;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// Tokens the model knows: bytes, for the tiny models.
    pub vocab: usize,
    /// Width of the hidden state.
    pub dim: usize,
    pub layers: usize,
    pub heads: usize,
    /// Width of the feed-forward network.
    pub hidden: usize,
    /// The longest sequence, prompt and generated tokens together.
    pub positions: usize,
}

struct Layer {
    attention_norm: Vec<f32>,
    query: Vec<f32>,
    key: Vec<f32>,
    value: Vec<f32>,
    out: Vec<f32>,
    feed_norm: Vec<f32>,
    up: Vec<f32>,
    down: Vec<f32>,
}

/// A model's weights. Matrices are stored by input row: `[inputs][outputs]`.
pub struct Model {
    pub config: Config,
    tokens: Vec<f32>,
    positions: Vec<f32>,
    layers: Vec<Layer>,
    final_norm: Vec<f32>,
    head: Vec<f32>,
}

#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("not a model file")]
    NotAModel,
    #[error("the configuration does not fit the weights")]
    BadConfig,
}

/// How many `f32` each tensor of a model holds, in file order.
fn shapes(config: &Config) -> Vec<usize> {
    let Config {
        vocab,
        dim,
        hidden,
        positions,
        ..
    } = *config;
    let mut shapes = vec![vocab * dim, positions * dim];
    for _ in 0..config.layers {
        shapes.extend([
            dim,
            dim * dim,
            dim * dim,
            dim * dim,
            dim * dim,
            dim,
            dim * hidden,
        ]);
        shapes.push(hidden * dim);
    }
    shapes.extend([dim, dim * vocab]);
    shapes
}

impl Model {
    /// A model from its tensors, in file order.
    pub fn from_tensors(config: Config, tensors: Vec<Vec<f32>>) -> Result<Self, ModelError> {
        let expected = shapes(&config);
        if config.heads == 0
            || !config.dim.is_multiple_of(config.heads)
            || tensors.len() != expected.len()
            || tensors.iter().zip(&expected).any(|(t, n)| t.len() != *n)
        {
            return Err(ModelError::BadConfig);
        }
        let mut tensors = tensors.into_iter();
        let mut next = || tensors.next().expect("counted above");
        let tokens = next();
        let positions = next();
        let layers = (0..config.layers)
            .map(|_| Layer {
                attention_norm: next(),
                query: next(),
                key: next(),
                value: next(),
                out: next(),
                feed_norm: next(),
                up: next(),
                down: next(),
            })
            .collect();
        let final_norm = next();
        let head = next();
        Ok(Model {
            config,
            tokens,
            positions,
            layers,
            final_norm,
            head,
        })
    }

    fn tensors(&self) -> Vec<&[f32]> {
        let mut all: Vec<&[f32]> = vec![&self.tokens, &self.positions];
        for layer in &self.layers {
            all.extend([
                layer.attention_norm.as_slice(),
                &layer.query,
                &layer.key,
                &layer.value,
                &layer.out,
                &layer.feed_norm,
                &layer.up,
                &layer.down,
            ]);
        }
        all.extend([self.final_norm.as_slice(), &self.head]);
        all
    }

    /// The model as a weights file.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = MAGIC.to_vec();
        bytes.extend(serde_json::to_vec(&self.config).expect("a configuration serializes"));
        bytes.push(b'\n');
        for tensor in self.tensors() {
            for value in tensor {
                bytes.extend_from_slice(&value.to_le_bytes());
            }
        }
        bytes
    }

    /// Reads a weights file.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ModelError> {
        let rest = bytes.strip_prefix(MAGIC).ok_or(ModelError::NotAModel)?;
        let end = rest
            .iter()
            .position(|&b| b == b'\n')
            .ok_or(ModelError::NotAModel)?;
        let config: Config =
            serde_json::from_slice(&rest[..end]).map_err(|_| ModelError::NotAModel)?;
        let (words, _) = rest[end + 1..].as_chunks::<4>();
        let mut floats = words.iter().map(|word| f32::from_le_bytes(*word));
        let tensors = shapes(&config)
            .into_iter()
            .map(|n| floats.by_ref().take(n).collect())
            .collect();
        if floats.next().is_some() || (rest.len() - end - 1) % 4 != 0 {
            return Err(ModelError::BadConfig);
        }
        Model::from_tensors(config, tensors)
    }

    /// Runs `f` over every weight, in file order, to make a changed copy.
    pub fn map(&self, f: impl Fn(f32) -> f32) -> Model {
        let tensors = self
            .tensors()
            .into_iter()
            .map(|t| t.iter().map(|&v| f(v)).collect())
            .collect();
        Model::from_tensors(self.config, tensors).expect("same shapes")
    }

    /// A session that feeds the model one token at a time, keeping the
    /// attention keys and values of the tokens before.
    pub fn session(&self) -> Session<'_> {
        Session {
            model: self,
            keys: vec![Vec::new(); self.config.layers],
            values: vec![Vec::new(); self.config.layers],
            length: 0,
        }
    }
}

/// `vector [n] x matrix [n][m]`.
fn times(vector: &[f32], matrix: &[f32], outputs: usize) -> Vec<f32> {
    let mut result = vec![0.0; outputs];
    for (row, &x) in matrix.chunks_exact(outputs).zip(vector) {
        for (r, &w) in result.iter_mut().zip(row) {
            *r += x * w;
        }
    }
    result
}

fn rms_norm(x: &[f32], gain: &[f32]) -> Vec<f32> {
    let mean_square = x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32;
    let scale = 1.0 / (mean_square + NORM_EPSILON).sqrt();
    x.iter().zip(gain).map(|(v, g)| v * scale * g).collect()
}

fn gelu(x: f32) -> f32 {
    const SQRT_2_OVER_PI: f32 = 0.797_884_6;
    0.5 * x * (1.0 + (SQRT_2_OVER_PI * (x + 0.044_715 * x * x * x)).tanh())
}

/// What the model computed for one token: its last hidden state, and the
/// logits of the token after it.
pub struct Step {
    pub hidden: Vec<f32>,
    pub logits: Vec<f32>,
}

pub struct Session<'a> {
    model: &'a Model,
    keys: Vec<Vec<f32>>,
    values: Vec<Vec<f32>>,
    length: usize,
}

impl Session<'_> {
    /// Tokens fed so far.
    pub fn len(&self) -> usize {
        self.length
    }

    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    /// Feeds the next token. The caller keeps within `Config::positions` and
    /// the vocabulary.
    pub fn feed(&mut self, token: u32) -> Step {
        let model = self.model;
        let Config {
            dim,
            heads,
            hidden,
            vocab,
            ..
        } = model.config;
        let head_dim = dim / heads;
        let position = self.length;
        let token = token as usize;
        let mut x: Vec<f32> = model.tokens[token * dim..(token + 1) * dim]
            .iter()
            .zip(&model.positions[position * dim..(position + 1) * dim])
            .map(|(t, p)| t + p)
            .collect();
        let scale = 1.0 / (head_dim as f32).sqrt();
        for (index, layer) in model.layers.iter().enumerate() {
            let h = rms_norm(&x, &layer.attention_norm);
            let query = times(&h, &layer.query, dim);
            self.keys[index].extend(times(&h, &layer.key, dim));
            self.values[index].extend(times(&h, &layer.value, dim));
            let keys = &self.keys[index];
            let values = &self.values[index];
            let mut attended = vec![0.0; dim];
            for head in 0..heads {
                let span = head * head_dim..(head + 1) * head_dim;
                let q = &query[span.clone()];
                let scores: Vec<f32> = (0..=position)
                    .map(|p| {
                        let k = &keys[p * dim..][span.clone()];
                        q.iter().zip(k).map(|(a, b)| a * b).sum::<f32>() * scale
                    })
                    .collect();
                let top = scores.iter().copied().fold(f32::NEG_INFINITY, f32::max);
                let weights: Vec<f32> = scores.iter().map(|s| (s - top).exp()).collect();
                let total: f32 = weights.iter().sum();
                for (p, w) in weights.iter().enumerate() {
                    let v = &values[p * dim..][span.clone()];
                    for (a, b) in attended[span.clone()].iter_mut().zip(v) {
                        *a += w / total * b;
                    }
                }
            }
            for (x, o) in x.iter_mut().zip(times(&attended, &layer.out, dim)) {
                *x += o;
            }
            let h = rms_norm(&x, &layer.feed_norm);
            let up: Vec<f32> = times(&h, &layer.up, hidden).into_iter().map(gelu).collect();
            for (x, d) in x.iter_mut().zip(times(&up, &layer.down, dim)) {
                *x += d;
            }
        }
        self.length += 1;
        let hidden_state = rms_norm(&x, &model.final_norm);
        let logits = times(&hidden_state, &model.head, vocab);
        Step {
            hidden: hidden_state,
            logits,
        }
    }
}
