//! The approved models this node holds, and how it answers a job's model
//! calls (see `decomp_core::inference`): by generating and keeping a
//! transcript, when it runs a job first, or by replaying another node's
//! transcript and checking each call, when it runs a job to check it.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use decomp_core::inference::{RecordedAnswer, RecordedCall, Transcript};
use decomp_core::toploc::Proof;
use decomp_core::{APPROVED_MODELS, Blob, ContentHash};
use decomp_inference::{Generated, Model, tiny};
use decomp_sandbox::{ModelAnswer, ModelCall, ModelError, ModelRequest, Models};

/// The approved models this node can run, by ID.
pub struct Held(HashMap<ContentHash, Model>);

static HELD: OnceLock<Held> = OnceLock::new();

/// The approved models this node holds: built once, and each checked against
/// the ID the approved list gives it.
pub fn held() -> &'static Held {
    HELD.get_or_init(|| {
        let models = APPROVED_MODELS
            .iter()
            .filter_map(|approved| Some((approved.id, decomp_inference::load(approved)?)))
            .collect();
        Held(models)
    })
}

impl Held {
    pub fn ids(&self) -> Vec<ContentHash> {
        let mut ids: Vec<ContentHash> = self.0.keys().copied().collect();
        ids.sort();
        ids
    }

    fn get(&self, id: &ContentHash) -> Result<&Model, ModelError> {
        self.0
            .get(id)
            .ok_or_else(|| ModelError::Unrunnable(format!("this node does not hold model {id}")))
    }
}

fn unrunnable(error: decomp_inference::Rejection) -> ModelError {
    ModelError::Unrunnable(error.to_string())
}

fn proof_blobs(proofs: &[Proof]) -> Vec<Blob> {
    proofs.iter().map(|proof| Blob(proof.to_bytes())).collect()
}

/// Answers model calls by running the models, and keeps the transcript.
#[derive(Default)]
pub struct Generator {
    transcript: Mutex<Transcript>,
}

impl Generator {
    pub fn transcript(&self) -> Transcript {
        self.transcript.lock().expect("transcript lock").clone()
    }
}

impl Models for Generator {
    fn call(&self, call: ModelCall<'_>) -> Result<ModelAnswer, ModelError> {
        let model = held().get(&call.model)?;
        let (answer, recorded) = match call.request {
            ModelRequest::Generate {
                prompt,
                max_tokens,
                sampling,
            } => {
                let prompt = tiny::encode(prompt);
                let generated = decomp_inference::generate(model, &prompt, max_tokens, &sampling)
                    .map_err(unrunnable)?;
                let answer = ModelAnswer {
                    output: tiny::decode(&generated.tokens),
                    tokens: (prompt.len() + generated.tokens.len()) as u64,
                };
                let recorded = RecordedCall {
                    model: call.model,
                    answer: RecordedAnswer::Generate {
                        tokens: generated.tokens,
                    },
                    proofs: proof_blobs(&generated.proofs),
                };
                (answer, recorded)
            }
            ModelRequest::Embed { text } => {
                let text = tiny::encode(text);
                let (embedding, proof) =
                    decomp_inference::embed(model, &text).map_err(unrunnable)?;
                let answer = ModelAnswer {
                    output: embedding.iter().flat_map(|v| v.to_le_bytes()).collect(),
                    tokens: text.len() as u64,
                };
                let recorded = RecordedCall {
                    model: call.model,
                    answer: RecordedAnswer::Embed { embedding },
                    proofs: proof_blobs(&[proof]),
                };
                (answer, recorded)
            }
        };
        self.transcript
            .lock()
            .expect("transcript lock")
            .calls
            .push(recorded);
        Ok(answer)
    }
}

/// Answers model calls with another node's transcript, once each call checks
/// out against this node's own copy of the model.
pub struct Replayer {
    transcript: Transcript,
    used: Mutex<usize>,
}

impl Replayer {
    pub fn new(transcript: Transcript) -> Self {
        Self {
            transcript,
            used: Mutex::new(0),
        }
    }

    /// Whether the job made every call the transcript holds. A transcript
    /// with calls the job never made is not the job's.
    pub fn used_all(&self) -> bool {
        *self.used.lock().expect("replay lock") == self.transcript.calls.len()
    }
}

fn rejected(reason: impl ToString) -> ModelError {
    ModelError::Rejected(reason.to_string())
}

impl Models for Replayer {
    fn call(&self, call: ModelCall<'_>) -> Result<ModelAnswer, ModelError> {
        let recorded = self
            .transcript
            .calls
            .get(call.index as usize)
            .ok_or_else(|| rejected("the job made more model calls than the transcript holds"))?;
        *self.used.lock().expect("replay lock") = call.index as usize + 1;
        if recorded.model != call.model {
            return Err(rejected("the transcript names another model for this call"));
        }
        let model = held().get(&call.model)?;
        let proofs = recorded
            .proofs
            .iter()
            .map(|blob| Proof::from_bytes(&blob.0))
            .collect::<Option<Vec<Proof>>>()
            .ok_or_else(|| rejected("a proof is not a TOPLOC proof"))?;
        match (call.request, &recorded.answer) {
            (
                ModelRequest::Generate {
                    prompt,
                    max_tokens,
                    sampling,
                },
                RecordedAnswer::Generate { tokens },
            ) => {
                let prompt = tiny::encode(prompt);
                let claimed = Generated {
                    tokens: tokens.clone(),
                    proofs,
                };
                decomp_inference::validate(model, &prompt, max_tokens, &sampling, &claimed)
                    .map_err(rejected)?;
                Ok(ModelAnswer {
                    output: tiny::decode(tokens),
                    tokens: (prompt.len() + tokens.len()) as u64,
                })
            }
            (ModelRequest::Embed { text }, RecordedAnswer::Embed { embedding }) => {
                let text = tiny::encode(text);
                let [proof] = proofs.as_slice() else {
                    return Err(rejected("an embedding has one proof"));
                };
                decomp_inference::validate_embedding(model, &text, embedding, proof)
                    .map_err(rejected)?;
                Ok(ModelAnswer {
                    output: embedding.iter().flat_map(|v| v.to_le_bytes()).collect(),
                    tokens: text.len() as u64,
                })
            }
            _ => Err(rejected("the transcript records another kind of call")),
        }
    }
}
