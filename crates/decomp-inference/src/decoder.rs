//! The decoder of the Llama family, which Qwen2, Qwen3, Llama, and Mistral
//! share: token embeddings; layers of grouped-query self-attention with
//! rotary positions and a gated feed-forward network, each after RMS
//! normalization; a final normalization, whose output is the last hidden
//! state TOPLOC proves; and an output layer. Family variants are switches in
//! `DecoderConfig`: Qwen3 normalizes queries and keys per head, Qwen2 adds
//! biases to them.
//!
//! The model is cut into the ends (`Ends`: the embeddings, the final
//! normalization, and the output layer) and stages (`Stage`: a range of
//! layers), so that a model too large for one device runs across several.
//! Each stage keeps the attention keys and values of the tokens it has seen
//! (`StageCache`), so a pipeline passes only each new token's activations
//! from stage to stage.

use std::collections::HashMap;
use std::sync::OnceLock;

use candle_core::{D, DType, Device, Tensor};
use serde::{Deserialize, Serialize};

pub type Result<T> = std::result::Result<T, candle_core::Error>;

/// The shape and switches of a Llama-family decoder.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DecoderConfig {
    pub vocab: usize,
    pub hidden: usize,
    pub layers: usize,
    pub heads: usize,
    pub kv_heads: usize,
    pub head_dim: usize,
    pub intermediate: usize,
    pub rms_eps: f64,
    pub rope_theta: f64,
    /// The longest sequence, prompt and generated tokens together.
    pub positions: usize,
    /// The output layer is the embedding table (Qwen3's small models).
    pub tied_embeddings: bool,
    /// Queries and keys are RMS-normalized per head before positions are
    /// applied (Qwen3).
    pub qk_norm: bool,
    /// The query, key, and value projections have biases (Qwen2).
    pub qkv_bias: bool,
    /// The precision every node computes the model in, whatever its device.
    /// It is part of the card, and so of the model's ID, because TOPLOC
    /// tells a change of precision apart from the drift between devices.
    #[serde(default, skip_serializing_if = "Precision::is_f32")]
    pub precision: Precision,
}

/// The number format a model is computed in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Precision {
    /// 32-bit floats: the default, and the fastest on a CPU.
    #[default]
    F32,
    /// bfloat16: half the memory, the format large models are trained and
    /// served in on GPUs.
    Bf16,
}

impl Precision {
    fn is_f32(&self) -> bool {
        *self == Precision::F32
    }

    pub fn dtype(self) -> DType {
        match self {
            Precision::F32 => DType::F32,
            Precision::Bf16 => DType::BF16,
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "f32" => Some(Precision::F32),
            "bf16" => Some(Precision::Bf16),
            _ => None,
        }
    }
}

impl DecoderConfig {
    /// Reads a Hugging Face `config.json` of a Llama-family model.
    pub fn from_hugging_face(json: &serde_json::Value) -> Option<Self> {
        let number = |key: &str| json.get(key)?.as_u64().map(|n| n as usize);
        let float = |key: &str| json.get(key)?.as_f64();
        let kind = json.get("model_type")?.as_str()?;
        if !matches!(kind, "llama" | "mistral" | "qwen2" | "qwen3") {
            return None;
        }
        let hidden = number("hidden_size")?;
        let heads = number("num_attention_heads")?;
        Some(DecoderConfig {
            vocab: number("vocab_size")?,
            hidden,
            layers: number("num_hidden_layers")?,
            heads,
            kv_heads: number("num_key_value_heads").unwrap_or(heads),
            head_dim: number("head_dim").unwrap_or(hidden / heads),
            intermediate: number("intermediate_size")?,
            rms_eps: float("rms_norm_eps")?,
            rope_theta: float("rope_theta").unwrap_or(10_000.0),
            positions: number("max_position_embeddings")?,
            tied_embeddings: json
                .get("tie_word_embeddings")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            qk_norm: kind == "qwen3",
            qkv_bias: kind == "qwen2"
                || json
                    .get("attention_bias")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
            precision: Precision::F32,
        })
    }
}

/// The device models run on: the first GPU, in a build with GPU support
/// (the `cuda` or `metal` feature) on a machine that has one, and otherwise
/// the CPU. A machine with several GPUs runs one node for each, each shown
/// only its own (`CUDA_VISIBLE_DEVICES`).
pub fn device() -> Device {
    static DEVICE: OnceLock<Device> = OnceLock::new();
    DEVICE
        .get_or_init(|| {
            #[cfg(feature = "cuda")]
            if let Ok(device) = Device::new_cuda(0) {
                return device;
            }
            #[cfg(feature = "metal")]
            if let Ok(device) = Device::new_metal(0) {
                return device;
            }
            Device::Cpu
        })
        .clone()
}

/// A weights file's tensors, in the format the model is computed in.
pub fn load_tensors(
    bytes: &[u8],
    device: &Device,
    dtype: DType,
) -> Result<HashMap<String, Tensor>> {
    candle_core::safetensors::load_buffer(bytes, device)?
        .into_iter()
        .map(|(name, tensor)| Ok((name, tensor.to_dtype(dtype)?)))
        .collect()
}

/// Activations as they travel between stages, plain 32-bit floats, as a
/// `[tokens, hidden]` tensor in the format the model is computed in.
pub fn from_floats(floats: Vec<f32>, tokens: usize, hidden: usize, dtype: DType) -> Result<Tensor> {
    Tensor::from_vec(floats, (tokens, hidden), &device())?.to_dtype(dtype)
}

/// A tensor as plain 32-bit floats, row after row.
pub fn to_floats(tensor: &Tensor) -> Result<Vec<f32>> {
    tensor.to_dtype(DType::F32)?.flatten_all()?.to_vec1::<f32>()
}

fn take(tensors: &mut HashMap<String, Tensor>, name: &str) -> Result<Tensor> {
    tensors
        .remove(name)
        .ok_or_else(|| candle_core::Error::Msg(format!("the weights have no tensor {name}")))
}

/// A projection's weight, `[out, in]` as stored, kept transposed for `x · W`.
fn projection(tensors: &mut HashMap<String, Tensor>, name: &str) -> Result<Tensor> {
    take(tensors, name)?.t()?.contiguous()
}

/// The embeddings, the final normalization, and the output layer.
pub struct Ends {
    config: DecoderConfig,
    embeddings: Tensor,
    norm: Tensor,
    /// `[hidden, vocab]`.
    output: Tensor,
}

impl Ends {
    pub fn load(config: &DecoderConfig, bytes: &[u8], device: &Device) -> Result<Self> {
        let mut tensors = load_tensors(bytes, device, config.precision.dtype())?;
        let embeddings = take(&mut tensors, "model.embed_tokens.weight")?;
        let output = if config.tied_embeddings {
            embeddings.t()?.contiguous()?
        } else {
            projection(&mut tensors, "lm_head.weight")?
        };
        Ok(Ends {
            config: config.clone(),
            norm: take(&mut tensors, "model.norm.weight")?,
            embeddings,
            output,
        })
    }

    pub fn dtype(&self) -> DType {
        self.config.precision.dtype()
    }

    /// The tokens' embeddings, `[tokens, hidden]`.
    pub fn embed(&self, tokens: &[u32]) -> Result<Tensor> {
        let ids = Tensor::new(tokens, self.embeddings.device())?;
        self.embeddings.index_select(&ids, 0)
    }

    /// The last hidden states, `[tokens, hidden]`, and the logits of the token
    /// after each, `[tokens, vocab]`, from the last stage's activations.
    pub fn finish(&self, activations: &Tensor) -> Result<(Tensor, Tensor)> {
        let hidden = candle_nn::ops::rms_norm(activations, &self.norm, self.config.rms_eps as f32)?;
        let logits = hidden.matmul(&self.output)?;
        Ok((hidden, logits))
    }
}

struct Layer {
    attention_norm: Tensor,
    query: Tensor,
    key: Tensor,
    value: Tensor,
    query_bias: Option<Tensor>,
    key_bias: Option<Tensor>,
    value_bias: Option<Tensor>,
    query_norm: Option<Tensor>,
    key_norm: Option<Tensor>,
    out: Tensor,
    feed_norm: Tensor,
    gate: Tensor,
    up: Tensor,
    down: Tensor,
}

/// A range of the decoder's layers, and the rotary tables they share.
pub struct Stage {
    config: DecoderConfig,
    pub first_layer: usize,
    layers: Vec<Layer>,
    cos: Tensor,
    sin: Tensor,
}

/// What a stage remembers of a sequence: each layer's keys and values,
/// `[kv_heads, tokens, head_dim]`.
#[derive(Default)]
pub struct StageCache {
    entries: Vec<Option<(Tensor, Tensor)>>,
    /// Tokens seen so far.
    pub length: usize,
}

impl Stage {
    /// Loads layers `first..first + count` from a weights file holding them,
    /// under their names in the whole model (`model.layers.N.…`).
    pub fn load(
        config: &DecoderConfig,
        first: usize,
        count: usize,
        bytes: &[u8],
        device: &Device,
    ) -> Result<Self> {
        let dtype = config.precision.dtype();
        let mut tensors = load_tensors(bytes, device, dtype)?;
        let mut layers = Vec::with_capacity(count);
        for index in first..first + count {
            let name = |part: &str| format!("model.layers.{index}.{part}");
            let mut optional = |part: &str, wanted: bool| -> Result<Option<Tensor>> {
                if wanted {
                    take(&mut tensors, &name(part)).map(Some)
                } else {
                    Ok(None)
                }
            };
            let query_bias = optional("self_attn.q_proj.bias", config.qkv_bias)?;
            let key_bias = optional("self_attn.k_proj.bias", config.qkv_bias)?;
            let value_bias = optional("self_attn.v_proj.bias", config.qkv_bias)?;
            let query_norm = optional("self_attn.q_norm.weight", config.qk_norm)?;
            let key_norm = optional("self_attn.k_norm.weight", config.qk_norm)?;
            layers.push(Layer {
                attention_norm: take(&mut tensors, &name("input_layernorm.weight"))?,
                query: projection(&mut tensors, &name("self_attn.q_proj.weight"))?,
                key: projection(&mut tensors, &name("self_attn.k_proj.weight"))?,
                value: projection(&mut tensors, &name("self_attn.v_proj.weight"))?,
                query_bias,
                key_bias,
                value_bias,
                query_norm,
                key_norm,
                out: projection(&mut tensors, &name("self_attn.o_proj.weight"))?,
                feed_norm: take(&mut tensors, &name("post_attention_layernorm.weight"))?,
                gate: projection(&mut tensors, &name("mlp.gate_proj.weight"))?,
                up: projection(&mut tensors, &name("mlp.up_proj.weight"))?,
                down: projection(&mut tensors, &name("mlp.down_proj.weight"))?,
            });
        }
        let half = config.head_dim / 2;
        let inverse: Vec<f32> = (0..half)
            .map(|i| {
                1.0 / config
                    .rope_theta
                    .powf(2.0 * i as f64 / config.head_dim as f64) as f32
            })
            .collect();
        let inverse = Tensor::new(inverse.as_slice(), device)?.reshape((1, half))?;
        let positions = Tensor::arange(0u32, config.positions as u32, device)?
            .to_dtype(DType::F32)?
            .reshape((config.positions, 1))?;
        let angles = positions.matmul(&inverse)?;
        Ok(Stage {
            config: config.clone(),
            first_layer: first,
            layers,
            cos: angles.cos()?.to_dtype(dtype)?,
            sin: angles.sin()?.to_dtype(dtype)?,
        })
    }

    pub fn dtype(&self) -> DType {
        self.config.precision.dtype()
    }

    pub fn layer_count(&self) -> usize {
        self.layers.len()
    }

    /// A fresh cache for a new sequence.
    pub fn cache(&self) -> StageCache {
        StageCache {
            entries: (0..self.layers.len()).map(|_| None).collect(),
            length: 0,
        }
    }

    /// Runs the stage's layers over the activations of the next tokens of a
    /// sequence, `[tokens, hidden]`, and returns its own, the same shape.
    pub fn forward(&self, activations: &Tensor, cache: &mut StageCache) -> Result<Tensor> {
        let (tokens, _) = activations.dims2()?;
        let start = cache.length;
        if start + tokens > self.config.positions {
            candle_core::bail!("the sequence is longer than the model takes");
        }
        let cos = self.cos.narrow(0, start, tokens)?.contiguous()?;
        let sin = self.sin.narrow(0, start, tokens)?.contiguous()?;
        let mask = causal_mask(tokens, start, activations.device(), activations.dtype())?;
        let mut x = activations.clone();
        for (layer, entry) in self.layers.iter().zip(cache.entries.iter_mut()) {
            x = self.layer(layer, &x, entry, &cos, &sin, mask.as_ref())?;
        }
        cache.length += tokens;
        Ok(x)
    }

    fn layer(
        &self,
        layer: &Layer,
        x: &Tensor,
        entry: &mut Option<(Tensor, Tensor)>,
        cos: &Tensor,
        sin: &Tensor,
        mask: Option<&Tensor>,
    ) -> Result<Tensor> {
        let c = &self.config;
        let (tokens, _) = x.dims2()?;
        let eps = c.rms_eps as f32;
        let h = candle_nn::ops::rms_norm(x, &layer.attention_norm, eps)?;
        let project = |w: &Tensor, b: &Option<Tensor>| -> Result<Tensor> {
            let y = h.matmul(w)?;
            match b {
                Some(b) => y.broadcast_add(b),
                None => Ok(y),
            }
        };
        // [tokens, heads * head_dim] -> [1, heads, tokens, head_dim]
        let split = |y: Tensor, heads: usize, norm: &Option<Tensor>| -> Result<Tensor> {
            let y = y.reshape((tokens, heads, c.head_dim))?;
            let y = match norm {
                Some(norm) => candle_nn::ops::rms_norm(&y.contiguous()?, norm, eps)?,
                None => y,
            };
            y.transpose(0, 1)?.contiguous()?.unsqueeze(0)
        };
        let q = split(
            project(&layer.query, &layer.query_bias)?,
            c.heads,
            &layer.query_norm,
        )?;
        let k = split(
            project(&layer.key, &layer.key_bias)?,
            c.kv_heads,
            &layer.key_norm,
        )?;
        let v = split(project(&layer.value, &layer.value_bias)?, c.kv_heads, &None)?;
        let q = candle_nn::rotary_emb::rope(&q, cos, sin)?.squeeze(0)?;
        let k = candle_nn::rotary_emb::rope(&k, cos, sin)?.squeeze(0)?;
        let v = v.squeeze(0)?;
        let (k, v) = match entry.take() {
            Some((past_k, past_v)) => (
                Tensor::cat(&[&past_k, &k], 1)?,
                Tensor::cat(&[&past_v, &v], 1)?,
            ),
            None => (k, v),
        };
        *entry = Some((k.clone(), v.clone()));
        let repeat = c.heads / c.kv_heads;
        let widen = |t: &Tensor| -> Result<Tensor> {
            let (kv, total, dim) = t.dims3()?;
            t.unsqueeze(1)?
                .expand((kv, repeat, total, dim))?
                .reshape((kv * repeat, total, dim))
        };
        let (k, v) = (widen(&k)?, widen(&v)?);
        let scale = 1.0 / (c.head_dim as f64).sqrt();
        let mut scores = (q.matmul(&k.t()?)? * scale)?;
        if let Some(mask) = mask {
            scores = scores.broadcast_add(mask)?;
        }
        let weights = candle_nn::ops::softmax_last_dim(&scores)?;
        let attended = weights
            .matmul(&v)?
            .transpose(0, 1)?
            .contiguous()?
            .reshape((tokens, c.heads * c.head_dim))?;
        let x = (x + attended.matmul(&layer.out)?)?;
        let h = candle_nn::ops::rms_norm(&x, &layer.feed_norm, eps)?;
        let gated = (candle_nn::ops::silu(&h.matmul(&layer.gate)?)? * h.matmul(&layer.up)?)?;
        x + gated.matmul(&layer.down)?
    }
}

/// The mask that keeps each of `tokens` new tokens, after `start` earlier
/// ones, from attending to tokens after it: `[tokens, start + tokens]`.
/// `None` for one token, which may attend to all.
fn causal_mask(
    tokens: usize,
    start: usize,
    device: &Device,
    dtype: DType,
) -> Result<Option<Tensor>> {
    if tokens == 1 {
        return Ok(None);
    }
    let total = start + tokens;
    let values: Vec<f32> = (0..tokens)
        .flat_map(|i| {
            (0..total).map(move |j| {
                if j <= start + i {
                    0.0
                } else {
                    f32::NEG_INFINITY
                }
            })
        })
        .collect();
    Tensor::from_vec(values, (tokens, total), device)?
        .to_dtype(dtype)
        .map(Some)
}

/// The rows of a `[rows, width]` tensor as plain 32-bit floats.
pub fn rows(tensor: &Tensor) -> Result<Vec<Vec<f32>>> {
    tensor.to_dtype(DType::F32)?.to_vec2::<f32>()
}

/// The last dimension's index of the largest value, for each row.
pub fn argmax(logits: &Tensor) -> Result<Vec<u32>> {
    logits.argmax(D::Minus1)?.to_vec1::<u32>()
}
