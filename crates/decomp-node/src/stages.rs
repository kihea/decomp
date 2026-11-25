//! Running stages of stored models for other nodes, and asking other nodes
//! to run them (see `decomp_inference::card`).
//!
//! A node holding a lease on a job that calls a model it holds only part of
//! is told, in its lease, which nodes the job board drew to run the other
//! stages. It sends each the activations of the tokens it feeds the model,
//! signed with its device key, and takes back the stage's own (`RemoteStage`).
//! A node drawn to run a stage is told so when it asks for work, and runs
//! the stage only for the node it was drawn for, until that node's lease ends
//! (`StageService`). It keeps the keys and values of each model call's tokens
//! so far, so only new tokens' activations travel.

use std::collections::HashMap;
use std::io::{self, Read};
use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::{Duration, Instant};

use decomp_core::{
    Blob, ContentHash, Envelope, JobId, PublicKey, STAGE_PATH, SigningKey, StageAnswer, StageLease,
    StageRequest,
};
use decomp_inference::StageLink;
use decomp_inference::decoder::StageCache;
use decomp_inference::runner::run_stage;
use tiny_http::{Method, Request, Response};

use crate::client::unix_now;
use crate::models::held;
use crate::peer::agent;
use crate::server::respond;

/// How long a node waits for a node drawn to run a stage to learn that it
/// was, and how often it asks meanwhile: about one round of asking for work.
const STAGE_LEASE_WAIT: Duration = Duration::from_secs(10);
const STAGE_LEASE_POLL: Duration = Duration::from_millis(500);

/// The status a stage node answers with when it was not drawn for the
/// request, or has not yet learned that it was.
const NOT_DRAWN_YET: u16 = 403;

/// The most one request to a stage may carry: the activations of a long
/// prompt of a wide model, as base64 in JSON.
const MAX_STAGE_REQUEST_BYTES: u64 = 256 * 1024 * 1024;

fn floats(bytes: &[u8]) -> Vec<f32> {
    let (words, _) = bytes.as_chunks::<4>();
    words.iter().map(|word| f32::from_le_bytes(*word)).collect()
}

fn bytes(floats: &[f32]) -> Vec<u8> {
    floats.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// A stage another node runs for this one.
pub struct RemoteStage {
    url: String,
    key: SigningKey,
    job_id: JobId,
    model: ContentHash,
    stage: u32,
    call: u32,
    sent: u64,
}

impl RemoteStage {
    pub fn new(
        address: String,
        key: &SigningKey,
        job_id: JobId,
        model: ContentHash,
        stage: u32,
        call: u32,
    ) -> Self {
        RemoteStage {
            url: format!("{}{STAGE_PATH}", address.trim_end_matches('/')),
            key: SigningKey::from_seed(key.seed()),
            job_id,
            model,
            stage,
            call,
            sent: 0,
        }
    }
}

impl StageLink for RemoteStage {
    fn forward(&mut self, activations: Vec<f32>, tokens: usize) -> Result<Vec<f32>, String> {
        let request = StageRequest {
            job_id: self.job_id,
            model: self.model,
            stage: self.stage,
            call: self.call,
            start: self.sent,
            tokens: tokens as u32,
            activations: Blob(bytes(&activations)),
        };
        let envelope = Envelope::seal(&self.key, &request);
        // A node drawn for a stage learns of it when it next asks for work,
        // which may be just after this node learned of its lease.
        let started = Instant::now();
        let mut answer = loop {
            let answer = agent()
                .post(&self.url)
                .send_json(&envelope)
                .map_err(|e| format!("stage {} did not answer: {e}", self.stage))?;
            if answer.status().as_u16() != NOT_DRAWN_YET || started.elapsed() >= STAGE_LEASE_WAIT {
                break answer;
            }
            sleep(STAGE_LEASE_POLL);
        };
        if !answer.status().is_success() {
            let why = answer.body_mut().read_to_string().unwrap_or_default();
            return Err(format!("stage {} refused: {why}", self.stage));
        }
        let answer: StageAnswer = answer
            .body_mut()
            .with_config()
            .limit(MAX_STAGE_REQUEST_BYTES)
            .read_json()
            .map_err(|e| format!("stage {} sent no activations: {e}", self.stage))?;
        self.sent += tokens as u64;
        Ok(floats(&answer.activations.0))
    }
}

/// Which sequence a stage's cache is for: whose, of which job, model, stage,
/// and call.
type SequenceKey = (PublicKey, JobId, ContentHash, u32, u32);

/// The stages this node was drawn to run, and the sequences it is in the
/// middle of.
#[derive(Default)]
pub struct StageService {
    leases: Mutex<Vec<StageLease>>,
    caches: Mutex<HashMap<SequenceKey, StageCache>>,
}

impl StageService {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    /// Takes the stages the job board says this node runs now. Sequences of
    /// stages no longer leased are forgotten.
    pub fn update(&self, leases: Vec<StageLease>) {
        let mut caches = self.caches.lock().expect("stage lock");
        caches.retain(|(head, job, model, stage, _), _| {
            leases.iter().any(|lease| {
                lease.head == *head
                    && lease.job_id == *job
                    && lease.model == *model
                    && lease.stage == *stage
            })
        });
        *self.leases.lock().expect("stage lock") = leases;
    }

    fn run(&self, envelope: &Envelope) -> Result<StageAnswer, (u16, String)> {
        let request: StageRequest = envelope
            .open()
            .map_err(|_| (400, "not a signed stage request".to_string()))?;
        let now = unix_now();
        let leased = self.leases.lock().expect("stage lock").iter().any(|lease| {
            lease.head == envelope.signer
                && lease.job_id == request.job_id
                && lease.model == request.model
                && lease.stage == request.stage
                && now <= lease.expires_at
        });
        if !leased {
            let why = "this node was not drawn to run that stage for you";
            return Err((NOT_DRAWN_YET, why.into()));
        }
        let held = held();
        let (Some(stage), Some(hidden)) = (
            held.stage(&request.model, request.stage),
            held.hidden_width(&request.model),
        ) else {
            return Err((404, "this node does not hold that stage".into()));
        };
        let key = (
            envelope.signer,
            request.job_id,
            request.model,
            request.stage,
            request.call,
        );
        // The cache is taken out while the stage runs, so other sequences go on.
        let mut cache = {
            let mut caches = self.caches.lock().expect("stage lock");
            if request.start == 0 {
                caches.remove(&key);
            }
            caches.remove(&key).unwrap_or_else(|| stage.cache())
        };
        if cache.length as u64 != request.start {
            return Err((409, "the tokens do not follow those sent before".into()));
        }
        let activations = floats(&request.activations.0);
        let out = run_stage(
            stage,
            &mut cache,
            activations,
            request.tokens as usize,
            hidden,
        )
        .map_err(|e| (400, e))?;
        self.caches.lock().expect("stage lock").insert(key, cache);
        Ok(StageAnswer {
            activations: Blob(bytes(&out)),
        })
    }
}

/// Answers `POST /v1/stage`.
pub(crate) fn answer(service: &StageService, mut request: Request) -> io::Result<()> {
    if *request.method() != Method::Post {
        return respond(
            request,
            Response::from_string("no such address").with_status_code(404),
        );
    }
    let body = request.as_reader().take(MAX_STAGE_REQUEST_BYTES);
    let outcome = serde_json::from_reader::<_, Envelope>(body)
        .map_err(|_| (400, "not a signed request".to_string()))
        .and_then(|envelope| service.run(&envelope));
    match outcome {
        Ok(answer) => respond(
            request,
            Response::from_data(serde_json::to_vec(&answer).expect("answers serialize")),
        ),
        Err((status, why)) => respond(request, Response::from_string(why).with_status_code(status)),
    }
}
