//! A stored model: its card, and the files it is cut into.
//!
//! A model too large for one device is published as files on storage nodes:
//! the tokenizer, the ends (embeddings, final normalization, output layer),
//! and stages (ranges of layers). The card names each file by its root and
//! holds the decoder's configuration, and the card's own root is the model's
//! ID in the approved list (`decomp_core::APPROVED_MODELS`). A node holds the
//! files it chose to; the node running a job holds the tokenizer and the ends,
//! and nodes drawn by the job board run the stages it lacks.

use std::collections::BTreeMap;

use decomp_core::{ContentHash, merkle};
use safetensors::SafeTensors;
use safetensors::tensor::TensorView;
use serde::{Deserialize, Serialize};

use crate::decoder::{DecoderConfig, Precision};

/// One file of a stored model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Part {
    pub root: ContentHash,
    pub bytes: u64,
}

/// One stage: layers `first_layer..first_layer + layers`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagePart {
    pub first_layer: usize,
    pub layers: usize,
    #[serde(flatten)]
    pub part: Part,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Card {
    pub name: String,
    /// The architecture: `decoder`, the Llama family (see `decoder`).
    pub family: String,
    pub config: DecoderConfig,
    pub tokenizer: Part,
    pub ends: Part,
    pub stages: Vec<StagePart>,
}

/// The family name of `decoder` models.
pub const DECODER_FAMILY: &str = "decoder";

impl Card {
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("a card serializes")
    }

    /// The model's ID: the root of the card.
    pub fn id(&self) -> ContentHash {
        merkle::root(&self.to_bytes())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PackError {
    #[error("the configuration is not a Llama-family decoder's")]
    UnknownFamily,
    #[error("the weights cannot be read: {0}")]
    Weights(String),
    #[error("a model of {layers} layers cannot be cut into {stages} stages")]
    BadStages { layers: usize, stages: usize },
}

/// Cuts a Hugging Face model into a card and its files: the tokenizer, the
/// ends, and `stages` stages of layers as even as the layer count allows.
/// Tensors keep their names and types, so a file is a slice of the original.
/// Returns the card and each file's bytes, in the card's order: tokenizer,
/// ends, then the stages. Every node computes the model in `precision`.
pub fn pack(
    name: &str,
    config: &serde_json::Value,
    weights: &[u8],
    tokenizer: Vec<u8>,
    stages: usize,
    precision: Precision,
) -> Result<(Card, Vec<Vec<u8>>), PackError> {
    let mut config = DecoderConfig::from_hugging_face(config).ok_or(PackError::UnknownFamily)?;
    config.precision = precision;
    if stages == 0 || stages > config.layers {
        return Err(PackError::BadStages {
            layers: config.layers,
            stages,
        });
    }
    let tensors =
        SafeTensors::deserialize(weights).map_err(|e| PackError::Weights(e.to_string()))?;
    // Which file each tensor goes in: 0 for the ends, 1 + n for stage n.
    let per_stage = config.layers.div_ceil(stages);
    let file_of = |name: &str| -> usize {
        let layer = name
            .strip_prefix("model.layers.")
            .and_then(|rest| rest.split('.').next())
            .and_then(|n| n.parse::<usize>().ok());
        layer.map_or(0, |layer| 1 + layer / per_stage)
    };
    let mut files: Vec<BTreeMap<String, TensorView<'_>>> = vec![BTreeMap::new(); stages + 1];
    for (tensor_name, view) in tensors.tensors() {
        files[file_of(&tensor_name)].insert(tensor_name, view);
    }
    let mut parts = vec![tokenizer];
    for file in &files {
        let bytes = safetensors::serialize(file.iter().map(|(n, v)| (n.clone(), v.clone())), None)
            .map_err(|e| PackError::Weights(e.to_string()))?;
        parts.push(bytes);
    }
    let part = |bytes: &Vec<u8>| Part {
        root: merkle::root(bytes),
        bytes: bytes.len() as u64,
    };
    let stage_parts = (0..stages)
        .map(|stage| {
            let first_layer = stage * per_stage;
            StagePart {
                first_layer,
                layers: per_stage.min(config.layers - first_layer),
                part: part(&parts[2 + stage]),
            }
        })
        .collect();
    let card = Card {
        name: name.to_string(),
        family: DECODER_FAMILY.to_string(),
        tokenizer: part(&parts[0]),
        ends: part(&parts[1]),
        stages: stage_parts,
        config,
    };
    Ok((card, parts))
}
