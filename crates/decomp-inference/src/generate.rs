//! Generating text with TOPLOC proofs, and checking another node's text and
//! proofs by running the model once over it (see `decomp_core::toploc` and
//! `decomp_core::inference`).

use decomp_core::inference::{MAX_GENERATED_TOKENS, Sampling, embedding_close, sample, sampled};
use decomp_core::toploc::{self, CHUNK_TOKENS, Comparison, Proof};

use crate::model::Step;
use crate::runner::Runner;

/// What a model generated, and the proofs of it: one of the prompt, then one
/// per `CHUNK_TOKENS` tokens.
#[derive(Clone, Debug)]
pub struct Generated {
    pub tokens: Vec<u32>,
    pub proofs: Vec<Proof>,
}

/// Why a call cannot run, or why a validator rejects another node's call.
#[derive(Debug, PartialEq, thiserror::Error)]
pub enum Rejection {
    #[error("the prompt is empty")]
    EmptyPrompt,
    #[error("the prompt and the tokens to generate are more than the model takes")]
    TooLong,
    #[error("a token is outside the model's vocabulary")]
    UnknownToken,
    #[error("the call asks for more than {MAX_GENERATED_TOKENS} tokens")]
    TooManyTokens,
    #[error("no proof could be made of a chunk")]
    Unprovable,
    #[error("the model could not be run: {0}")]
    Unavailable(String),
    #[error("the output is not as long as was asked for")]
    WrongLength,
    #[error("the proofs do not cover the prompt and every chunk of the output")]
    WrongProofCount,
    #[error("the proof of chunk {chunk} does not hold: {comparison:?}")]
    ProofFails {
        chunk: usize,
        comparison: Comparison,
    },
    #[error("token {step} is not one the model could have sampled")]
    NotSampled { step: usize },
    #[error("the embedding is not the model's")]
    WrongEmbedding,
}

/// Whether a call fits the model: a prompt, within its vocabulary, and with
/// the tokens to generate, within its length.
pub fn fits(runner: &dyn Runner, prompt: &[u32], max_tokens: u32) -> Result<(), Rejection> {
    if prompt.is_empty() {
        return Err(Rejection::EmptyPrompt);
    }
    if max_tokens > MAX_GENERATED_TOKENS {
        return Err(Rejection::TooManyTokens);
    }
    if prompt.len() + max_tokens as usize > runner.positions() {
        return Err(Rejection::TooLong);
    }
    if prompt.iter().any(|&t| t as usize >= runner.vocab()) {
        return Err(Rejection::UnknownToken);
    }
    Ok(())
}

fn feed(runner: &mut dyn Runner, tokens: &[u32]) -> Result<Vec<Step>, Rejection> {
    runner.feed(tokens).map_err(Rejection::Unavailable)
}

/// Feeds the prompt in one pass, and returns the hidden states of its tokens,
/// one after another, and the logits of the first token to generate.
fn read_prompt(runner: &mut dyn Runner, prompt: &[u32]) -> Result<(Vec<f32>, Vec<f32>), Rejection> {
    let steps = feed(runner, prompt)?;
    let logits = steps
        .last()
        .map(|step| step.logits.clone())
        .unwrap_or_default();
    Ok((
        steps.into_iter().flat_map(|step| step.hidden).collect(),
        logits,
    ))
}

/// Generates `max_tokens` tokens after `prompt`, and proves them.
pub fn generate(
    runner: &mut dyn Runner,
    prompt: &[u32],
    max_tokens: u32,
    sampling: &Sampling,
) -> Result<Generated, Rejection> {
    fits(runner, prompt, max_tokens)?;
    let (prompt_hidden, mut logits) = read_prompt(runner, prompt)?;
    let mut proofs = vec![toploc::prove(&prompt_hidden).ok_or(Rejection::Unprovable)?];
    let mut tokens = Vec::with_capacity(max_tokens as usize);
    let mut chunk = Vec::new();
    for step in 0..max_tokens {
        let token = sample(&logits, sampling, step);
        tokens.push(token);
        // Each generated token is fed too, so that it has a hidden state to
        // prove, and the logits of the token after it.
        let fed = feed(runner, &[token])?
            .pop()
            .ok_or_else(|| Rejection::Unavailable("no step came back".into()))?;
        chunk.extend(fed.hidden);
        logits = fed.logits;
        if tokens.len() % CHUNK_TOKENS == 0 || tokens.len() == max_tokens as usize {
            proofs.push(toploc::prove(&chunk).ok_or(Rejection::Unprovable)?);
            chunk.clear();
        }
    }
    Ok(Generated { tokens, proofs })
}

/// How many proofs a call that generated `tokens` tokens has.
pub fn proof_count(tokens: usize) -> usize {
    1 + tokens.div_ceil(CHUNK_TOKENS)
}

/// Checks another node's call: that its proofs hold of this model's own
/// hidden states over the prompt and the output, and that each token is one
/// the model could have sampled. The prompt and the output are each read in
/// one pass, which is what makes checking cheaper than generating.
pub fn validate(
    runner: &mut dyn Runner,
    prompt: &[u32],
    max_tokens: u32,
    sampling: &Sampling,
    claimed: &Generated,
) -> Result<(), Rejection> {
    fits(runner, prompt, max_tokens)?;
    if claimed.tokens.len() != max_tokens as usize {
        return Err(Rejection::WrongLength);
    }
    if claimed.proofs.len() != proof_count(claimed.tokens.len()) {
        return Err(Rejection::WrongProofCount);
    }
    if claimed.tokens.iter().any(|&t| t as usize >= runner.vocab()) {
        return Err(Rejection::UnknownToken);
    }
    let check = |chunk: usize, hidden: &[f32]| {
        let comparison = toploc::compare(&claimed.proofs[chunk], hidden);
        if comparison.holds() {
            Ok(())
        } else {
            Err(Rejection::ProofFails { chunk, comparison })
        }
    };
    let (prompt_hidden, first_logits) = read_prompt(runner, prompt)?;
    check(0, &prompt_hidden)?;
    if claimed.tokens.is_empty() {
        return Ok(());
    }
    let steps = feed(runner, &claimed.tokens)?;
    let mut logits = &first_logits;
    let mut chunk = Vec::new();
    for (step, (&token, fed)) in claimed.tokens.iter().zip(&steps).enumerate() {
        if !sampled(logits, sampling, step as u32, token) {
            return Err(Rejection::NotSampled { step });
        }
        chunk.extend_from_slice(&fed.hidden);
        logits = &fed.logits;
        let done = step + 1;
        if done % CHUNK_TOKENS == 0 || done == claimed.tokens.len() {
            check(done.div_ceil(CHUNK_TOKENS), &chunk)?;
            chunk.clear();
        }
    }
    Ok(())
}

/// The mean of the last hidden states of `text`, and the proof of them.
pub fn embed(runner: &mut dyn Runner, text: &[u32]) -> Result<(Vec<f32>, Proof), Rejection> {
    fits(runner, text, 0)?;
    let steps = feed(runner, text)?;
    let hidden: Vec<f32> = steps
        .iter()
        .flat_map(|s| s.hidden.iter().copied())
        .collect();
    let proof = toploc::prove(&hidden).ok_or(Rejection::Unprovable)?;
    Ok((mean(&steps), proof))
}

fn mean(steps: &[Step]) -> Vec<f32> {
    let width = steps.first().map_or(0, |s| s.hidden.len());
    let mut sum = vec![0.0; width];
    for step in steps {
        for (s, v) in sum.iter_mut().zip(&step.hidden) {
            *s += v;
        }
    }
    sum.iter().map(|s| s / steps.len() as f32).collect()
}

/// Checks another node's embedding of `text`, and its proof.
pub fn validate_embedding(
    runner: &mut dyn Runner,
    text: &[u32],
    embedding: &[f32],
    proof: &Proof,
) -> Result<(), Rejection> {
    fits(runner, text, 0)?;
    let steps = feed(runner, text)?;
    let hidden: Vec<f32> = steps
        .iter()
        .flat_map(|s| s.hidden.iter().copied())
        .collect();
    let comparison = toploc::compare(proof, &hidden);
    if !comparison.holds() {
        return Err(Rejection::ProofFails {
            chunk: 0,
            comparison,
        });
    }
    if !embedding_close(embedding, &mean(&steps)) {
        return Err(Rejection::WrongEmbedding);
    }
    Ok(())
}
