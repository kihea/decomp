//! The approved models this node holds, and how it answers a job's model
//! calls (see `decomp_core::inference`): by generating and keeping a
//! transcript, when it runs a job first, or by replaying another node's
//! transcript and checking each call, when it runs a job to check it.
//!
//! A tiny model runs whole here. A stored model (`decomp_inference::card`)
//! runs as a pipeline: this node runs the ends and the stages it holds, and
//! the nodes the job board drew in the lease run the rest (`stages`).

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::{Mutex, OnceLock};

use decomp_core::inference::{RecordedAnswer, RecordedCall, Transcript, approved};
use decomp_core::toploc::Proof;
use decomp_core::{
    APPROVED_MODELS, Blob, ContentHash, JobId, Lease, PipelineStage, SigningKey, StageRef, merkle,
};
use decomp_inference::card::Card;
use decomp_inference::decoder::{self, Ends, Stage};
use decomp_inference::{
    Generated, LocalStage, Model, Pipeline, Rejection, Runner, StageLink, tiny,
};
use decomp_sandbox::{ModelAnswer, ModelCall, ModelError, ModelRequest, Models};

use crate::client::Client;
use crate::stages::RemoteStage;
use crate::store;

/// A stored model, as much of it as this node holds.
pub struct Stored {
    pub card: Card,
    tokenizer: Option<decomp_inference::tokenizers::Tokenizer>,
    ends: Option<Ends>,
    pub stages: HashMap<u32, Stage>,
}

/// The approved models this node holds, by ID.
#[derive(Default)]
pub struct Held {
    tiny: HashMap<ContentHash, Model>,
    stored: HashMap<ContentHash, Stored>,
}

static HELD: OnceLock<Held> = OnceLock::new();

fn tiny_models() -> HashMap<ContentHash, Model> {
    APPROVED_MODELS
        .iter()
        .filter_map(|approved| Some((approved.id, decomp_inference::load(approved)?)))
        .collect()
}

/// The models this node holds: the tiny ones, which it builds, and whatever
/// parts of stored models `hold` fetched.
pub fn held() -> &'static Held {
    HELD.get_or_init(|| Held {
        tiny: tiny_models(),
        stored: HashMap::new(),
    })
}

/// What to hold of a stored model: `NAME` for all of it, `NAME:ends` for its
/// tokenizer and ends, `NAME:stage-N` for one stage.
fn parse_wanted(text: &str) -> Result<(ContentHash, Option<String>), String> {
    let (name, part) = match text.split_once(':') {
        Some((name, part)) => (name, Some(part.to_string())),
        None => (text, None),
    };
    let model = APPROVED_MODELS
        .iter()
        .find(|model| model.name == name && model.stages > 0)
        .ok_or_else(|| format!("{name} is not an approved model cut into stages"))?;
    Ok((model.id, part))
}

/// Fetches the parts of stored models this node is to hold, checks each
/// against its root, and loads them. A part this node already keeps as a
/// storage node (`local`) is read from its own copy; the rest come from
/// storage nodes. Call once, before `held`; a node that calls it later keeps
/// what it had.
pub fn hold(
    client: &Client,
    wanted: &[String],
    local: &dyn Fn(&ContentHash) -> Option<Vec<u8>>,
) -> Result<(), String> {
    let fetch = |root: ContentHash| -> Result<Vec<u8>, String> {
        match local(&root).filter(|bytes| merkle::root(bytes) == root) {
            Some(bytes) => Ok(bytes),
            None => store::fetch(client, root),
        }
    };
    let mut stored: HashMap<ContentHash, Stored> = HashMap::new();
    let device = decoder::device();
    for text in wanted {
        let (id, part) = parse_wanted(text)?;
        let model = match stored.entry(id) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let card: Card = serde_json::from_slice(&fetch(id)?)
                    .map_err(|e| format!("the card of {text} cannot be read: {e}"))?;
                if card.id() != id {
                    return Err(format!("the card fetched for {text} is not its card"));
                }
                entry.insert(Stored {
                    card,
                    tokenizer: None,
                    ends: None,
                    stages: HashMap::new(),
                })
            }
        };
        let card = model.card.clone();
        let wants_ends = part.as_deref().is_none_or(|p| p == "ends");
        if wants_ends && model.ends.is_none() {
            let bytes = fetch(card.tokenizer.root)?;
            let tokenizer = decomp_inference::tokenizers::Tokenizer::from_bytes(&bytes)
                .map_err(|e| e.to_string())?;
            let bytes = fetch(card.ends.root)?;
            let ends = Ends::load(&card.config, &bytes, &device).map_err(|e| e.to_string())?;
            model.tokenizer = Some(tokenizer);
            model.ends = Some(ends);
        }
        for (index, stage) in card.stages.iter().enumerate() {
            let name = format!("stage-{index}");
            let wants = part.as_deref().is_none_or(|p| p == name);
            if !wants || model.stages.contains_key(&(index as u32)) {
                continue;
            }
            let bytes = fetch(stage.part.root)?;
            let loaded = Stage::load(
                &card.config,
                stage.first_layer,
                stage.layers,
                &bytes,
                &device,
            )
            .map_err(|e| e.to_string())?;
            model.stages.insert(index as u32, loaded);
        }
        println!("holding {text} of {}", card.name);
    }
    let _ = HELD.set(Held {
        tiny: tiny_models(),
        stored,
    });
    Ok(())
}

impl Held {
    /// The models this node can run a job with: tiny ones whole, stored ones
    /// whose tokenizer and ends it holds.
    pub fn ids(&self) -> Vec<ContentHash> {
        let stored = self
            .stored
            .iter()
            .filter(|(_, model)| model.ends.is_some())
            .map(|(id, _)| *id);
        let mut ids: Vec<ContentHash> = self.tiny.keys().copied().chain(stored).collect();
        ids.sort();
        ids
    }

    /// The stages this node holds, which it runs for other nodes too.
    pub fn stages(&self) -> Vec<StageRef> {
        let mut stages: Vec<StageRef> = self
            .stored
            .iter()
            .flat_map(|(id, model)| {
                model.stages.keys().map(|stage| StageRef {
                    model: *id,
                    stage: *stage,
                })
            })
            .collect();
        stages.sort();
        stages
    }

    pub fn stage(&self, model: &ContentHash, stage: u32) -> Option<&Stage> {
        self.stored.get(model)?.stages.get(&stage)
    }

    pub fn hidden_width(&self, model: &ContentHash) -> Option<usize> {
        Some(self.stored.get(model)?.card.config.hidden)
    }
}

/// What a run of a job knows that its model calls need: the job, the nodes
/// drawn to run stages, and the key requests to them are signed with.
pub struct CallContext {
    job_id: JobId,
    pipeline: Vec<PipelineStage>,
    key: SigningKey,
}

impl CallContext {
    pub fn new(client: &Client, lease: &Lease) -> Self {
        CallContext {
            job_id: lease.job_id,
            pipeline: lease.pipeline.clone(),
            key: SigningKey::from_seed(client.key().seed()),
        }
    }

    /// Off the network: no job, no stages run elsewhere.
    pub fn local(key: &SigningKey) -> Self {
        CallContext {
            job_id: JobId::from_bytes([0; 32]),
            pipeline: Vec::new(),
            key: SigningKey::from_seed(key.seed()),
        }
    }
}

fn unrunnable(error: impl ToString) -> ModelError {
    ModelError::Unrunnable(error.to_string())
}

/// This node cannot run the call now, through no fault of the job's.
fn unavailable(error: impl ToString) -> ModelError {
    ModelError::Unavailable(error.to_string())
}

/// A failed generation: this node's trouble, or the call's.
fn ran(rejection: Rejection) -> ModelError {
    match rejection {
        Rejection::Unavailable(reason) => ModelError::Unavailable(reason),
        other => unrunnable(other),
    }
}

/// A model's tokens from bytes, and bytes from its tokens.
enum Tokens<'a> {
    Bytes,
    Tokenizer(&'a decomp_inference::tokenizers::Tokenizer),
}

impl Tokens<'_> {
    fn encode(&self, bytes: &[u8]) -> Result<Vec<u32>, ModelError> {
        match self {
            Tokens::Bytes => Ok(tiny::encode(bytes)),
            Tokens::Tokenizer(tokenizer) => tokenizer
                .encode(String::from_utf8_lossy(bytes).as_ref(), false)
                .map(|encoding| encoding.get_ids().to_vec())
                .map_err(unrunnable),
        }
    }

    fn decode(&self, tokens: &[u32]) -> Result<Vec<u8>, ModelError> {
        match self {
            Tokens::Bytes => Ok(tiny::decode(tokens)),
            Tokens::Tokenizer(tokenizer) => tokenizer
                .decode(tokens, false)
                .map(String::into_bytes)
                .map_err(unrunnable),
        }
    }
}

/// Runs `f` with a runner for call number `call` of `model`, and the model's
/// tokens.
fn with_runner<R>(
    context: &CallContext,
    model: &ContentHash,
    call: u32,
    f: impl FnOnce(&mut dyn Runner, &Tokens<'_>) -> Result<R, ModelError>,
) -> Result<R, ModelError> {
    let held = held();
    if let Some(tiny_model) = held.tiny.get(model) {
        return f(&mut tiny_model.session(), &Tokens::Bytes);
    }
    let stored = held
        .stored
        .get(model)
        .ok_or_else(|| unavailable(format!("this node does not hold model {model}")))?;
    let (Some(ends), Some(tokenizer)) = (&stored.ends, &stored.tokenizer) else {
        return Err(unavailable("this node does not hold the model's ends"));
    };
    let mut links: Vec<Box<dyn StageLink + '_>> = Vec::new();
    for index in 0..stored.card.stages.len() as u32 {
        if let Some(stage) = stored.stages.get(&index) {
            links.push(Box::new(LocalStage::new(stage)));
            continue;
        }
        let drawn = context
            .pipeline
            .iter()
            .find(|p| p.model == *model && p.stage == index)
            .ok_or_else(|| unavailable(format!("no node was drawn to run stage {index}")))?;
        let address = drawn.address.clone().ok_or_else(|| {
            unavailable(format!("the node drawn for stage {index} has no address"))
        })?;
        links.push(Box::new(RemoteStage::new(
            address,
            &context.key,
            context.job_id,
            *model,
            index,
            call,
        )));
    }
    let mut pipeline = Pipeline::new(ends, &stored.card.config, links);
    f(&mut pipeline, &Tokens::Tokenizer(tokenizer))
}

fn proof_blobs(proofs: &[Proof]) -> Vec<Blob> {
    proofs.iter().map(|proof| Blob(proof.to_bytes())).collect()
}

fn as_bytes(embedding: &[f32]) -> Vec<u8> {
    embedding.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// Answers model calls by running the models, and keeps the transcript.
pub struct Generator {
    context: CallContext,
    transcript: Mutex<Transcript>,
}

impl Generator {
    pub fn new(context: CallContext) -> Self {
        Generator {
            context,
            transcript: Mutex::default(),
        }
    }

    pub fn transcript(&self) -> Transcript {
        self.transcript.lock().expect("transcript lock").clone()
    }
}

impl Models for Generator {
    fn call(&self, call: ModelCall<'_>) -> Result<ModelAnswer, ModelError> {
        let model = call.model;
        let (answer, recorded) = with_runner(
            &self.context,
            &model,
            call.index,
            |runner, tokens| match call.request {
                ModelRequest::Generate {
                    prompt,
                    max_tokens,
                    sampling,
                } => {
                    let prompt = tokens.encode(prompt)?;
                    let generated =
                        decomp_inference::generate(runner, &prompt, max_tokens, &sampling)
                            .map_err(ran)?;
                    let answer = ModelAnswer {
                        output: tokens.decode(&generated.tokens)?,
                        tokens: (prompt.len() + generated.tokens.len()) as u64,
                    };
                    let recorded = RecordedCall {
                        model,
                        answer: RecordedAnswer::Generate {
                            tokens: generated.tokens,
                        },
                        proofs: proof_blobs(&generated.proofs),
                    };
                    Ok((answer, recorded))
                }
                ModelRequest::Embed { text } => {
                    let text = tokens.encode(text)?;
                    let (embedding, proof) = decomp_inference::embed(runner, &text).map_err(ran)?;
                    let answer = ModelAnswer {
                        output: as_bytes(&embedding),
                        tokens: text.len() as u64,
                    };
                    let recorded = RecordedCall {
                        model,
                        answer: RecordedAnswer::Embed { embedding },
                        proofs: proof_blobs(&[proof]),
                    };
                    Ok((answer, recorded))
                }
            },
        )?;
        self.transcript
            .lock()
            .expect("transcript lock")
            .calls
            .push(recorded);
        Ok(answer)
    }
}

/// Answers model calls with another node's transcript, once each call checks
/// out against this node's own run of the model.
pub struct Replayer {
    context: CallContext,
    transcript: Transcript,
    used: Mutex<usize>,
}

impl Replayer {
    pub fn new(transcript: Transcript, context: CallContext) -> Self {
        Self {
            context,
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

/// A rejection that is the transcript's fault, or one that is this node's
/// (it could not run the model), which is not a finding about the transcript.
fn judged(rejection: Rejection) -> ModelError {
    match rejection {
        Rejection::Unavailable(reason) => ModelError::Unavailable(reason),
        other => rejected(other),
    }
}

impl Models for Replayer {
    fn call(&self, call: ModelCall<'_>) -> Result<ModelAnswer, ModelError> {
        let Some(recorded) = self.transcript.calls.get(call.index as usize) else {
            // The first node keeps no call that could not run, and its job
            // ended there; this run must end the same way.
            return with_runner(&self.context, &call.model, call.index, |runner, tokens| {
                let (prompt, max_tokens) = match call.request {
                    ModelRequest::Generate {
                        prompt, max_tokens, ..
                    } => (prompt, max_tokens),
                    ModelRequest::Embed { text } => (text, 0),
                };
                decomp_inference::fits(runner, &tokens.encode(prompt)?, max_tokens)
                    .map_err(unrunnable)?;
                Err(rejected(
                    "the job made more model calls than the transcript holds",
                ))
            });
        };
        *self.used.lock().expect("replay lock") = call.index as usize + 1;
        if recorded.model != call.model {
            return Err(rejected("the transcript names another model for this call"));
        }
        if approved(&call.model).is_none() {
            return Err(rejected(
                "the transcript names a model that is not approved",
            ));
        }
        let proofs = recorded
            .proofs
            .iter()
            .map(|blob| Proof::from_bytes(&blob.0))
            .collect::<Option<Vec<Proof>>>()
            .ok_or_else(|| rejected("a proof is not a TOPLOC proof"))?;
        with_runner(
            &self.context,
            &call.model,
            call.index,
            |runner, tokens| match (call.request, &recorded.answer) {
                (
                    ModelRequest::Generate {
                        prompt,
                        max_tokens,
                        sampling,
                    },
                    RecordedAnswer::Generate { tokens: claimed },
                ) => {
                    let prompt = tokens.encode(prompt)?;
                    let generated = Generated {
                        tokens: claimed.clone(),
                        proofs,
                    };
                    decomp_inference::validate(runner, &prompt, max_tokens, &sampling, &generated)
                        .map_err(judged)?;
                    Ok(ModelAnswer {
                        output: tokens.decode(claimed)?,
                        tokens: (prompt.len() + claimed.len()) as u64,
                    })
                }
                (ModelRequest::Embed { text }, RecordedAnswer::Embed { embedding }) => {
                    let text = tokens.encode(text)?;
                    let [proof] = proofs.as_slice() else {
                        return Err(rejected("an embedding has one proof"));
                    };
                    decomp_inference::validate_embedding(runner, &text, embedding, proof)
                        .map_err(judged)?;
                    Ok(ModelAnswer {
                        output: as_bytes(embedding),
                        tokens: text.len() as u64,
                    })
                }
                _ => Err(rejected("the transcript records another kind of call")),
            },
        )
    }
}
