//! What generating and checking run a model through: a `Runner` feeds it
//! tokens and hands back, for each, the last hidden state and the logits of
//! the token after it. The tiny models run whole in one process; a stored
//! model runs as a pipeline of its ends and stages, each stage here or on
//! another node.

use candle_core::Tensor;

use crate::decoder::{self, Ends, Stage, StageCache};
use crate::model::{Session, Step};

pub trait Runner {
    fn vocab(&self) -> usize;
    /// The longest sequence the model takes.
    fn positions(&self) -> usize;
    /// Feeds the next tokens of the sequence and returns a step for each.
    fn feed(&mut self, tokens: &[u32]) -> Result<Vec<Step>, String>;
}

impl Runner for Session<'_> {
    fn vocab(&self) -> usize {
        self.model().config.vocab
    }

    fn positions(&self) -> usize {
        self.model().config.positions
    }

    fn feed(&mut self, tokens: &[u32]) -> Result<Vec<Step>, String> {
        Ok(tokens.iter().map(|&t| Session::feed(self, t)).collect())
    }
}

/// One stage of a pipeline: run here, or by another node. It keeps the
/// sequence's keys and values itself.
pub trait StageLink {
    /// Runs the stage over the next tokens' activations, `tokens` rows of
    /// `hidden` values, and returns its own, the same shape.
    fn forward(&mut self, activations: Vec<f32>, tokens: usize) -> Result<Vec<f32>, String>;
}

/// A stage this node holds.
pub struct LocalStage<'a> {
    stage: &'a Stage,
    cache: StageCache,
}

impl<'a> LocalStage<'a> {
    pub fn new(stage: &'a Stage) -> Self {
        LocalStage {
            stage,
            cache: stage.cache(),
        }
    }
}

fn failed(error: candle_core::Error) -> String {
    error.to_string()
}

/// Runs a stage over activations as plain floats: what a node serving a
/// stage to another node does.
pub fn run_stage(
    stage: &Stage,
    cache: &mut StageCache,
    activations: Vec<f32>,
    tokens: usize,
    hidden: usize,
) -> Result<Vec<f32>, String> {
    if tokens == 0 || activations.len() != tokens * hidden {
        return Err("the activations are not whole rows".into());
    }
    let device = decoder::device();
    let x = Tensor::from_vec(activations, (tokens, hidden), &device).map_err(failed)?;
    let y = stage.forward(&x, cache).map_err(failed)?;
    y.flatten_all()
        .and_then(|y| y.to_vec1::<f32>())
        .map_err(failed)
}

impl StageLink for LocalStage<'_> {
    fn forward(&mut self, activations: Vec<f32>, tokens: usize) -> Result<Vec<f32>, String> {
        let hidden = activations.len() / tokens.max(1);
        run_stage(self.stage, &mut self.cache, activations, tokens, hidden)
    }
}

/// A stored model run as its ends and its stages in order.
pub struct Pipeline<'a> {
    ends: &'a Ends,
    stages: Vec<Box<dyn StageLink + 'a>>,
    vocab: usize,
    hidden: usize,
    positions: usize,
}

impl<'a> Pipeline<'a> {
    pub fn new(
        ends: &'a Ends,
        config: &decoder::DecoderConfig,
        stages: Vec<Box<dyn StageLink + 'a>>,
    ) -> Self {
        Pipeline {
            ends,
            stages,
            vocab: config.vocab,
            hidden: config.hidden,
            positions: config.positions,
        }
    }
}

impl Runner for Pipeline<'_> {
    fn vocab(&self) -> usize {
        self.vocab
    }

    fn positions(&self) -> usize {
        self.positions
    }

    fn feed(&mut self, tokens: &[u32]) -> Result<Vec<Step>, String> {
        let embedded = self.ends.embed(tokens).map_err(failed)?;
        let mut activations = embedded
            .flatten_all()
            .and_then(|x| x.to_vec1::<f32>())
            .map_err(failed)?;
        for stage in &mut self.stages {
            activations = stage.forward(activations, tokens.len())?;
            if activations.len() != tokens.len() * self.hidden {
                return Err("a stage answered with activations of the wrong shape".into());
            }
        }
        let device = decoder::device();
        let x =
            Tensor::from_vec(activations, (tokens.len(), self.hidden), &device).map_err(failed)?;
        let (hidden, logits) = self.ends.finish(&x).map_err(failed)?;
        let hidden = decoder::rows(&hidden).map_err(failed)?;
        let logits = decoder::rows(&logits).map_err(failed)?;
        Ok(hidden
            .into_iter()
            .zip(logits)
            .map(|(hidden, logits)| Step { hidden, logits })
            .collect())
    }
}
