//! Runs DeComp job modules: deterministic, metered WebAssembly (spec sections 2,
//! 4 "Determinism" and "Checkpoint disputes", 5 "Price of a job", and 7 "Standard
//! instruction set").
//!
//! Two honest nodes that run the same job get the same output, the same compute
//! bill, and the same checkpoints, on any hardware. That holds because:
//!
//! - a module can reach nothing outside itself except the host functions below, so
//!   it has no clock, network, or file system, and its only randomness is a
//!   stream fixed by the job;
//! - threads are unavailable, NaN values are canonicalized, and relaxed SIMD
//!   instructions take their deterministic form;
//! - compute is counted in WebAssembly instructions, not time, by code added to
//!   the module itself (see `meter`), so the count does not depend on the engine.
//!
//! # Module interface
//!
//! A job module must export its linear memory as `memory` and a function `run`
//! that takes and returns nothing. A module built with the SDK also exports
//! `signature`, which writes a description of the job's types as its output.
//! A module may import these functions from the module named `decomp`, and
//! nothing else. Pointers and lengths are 32-bit integers.
//!
//! | Function | Signature | Effect |
//! | --- | --- | --- |
//! | `input.len` | `() -> len` | Length of the job input in bytes |
//! | `input.read` | `(dest)` | Copies the whole input into memory at `dest` |
//! | `output.write` | `(src, len)` | Appends `len` bytes at `src` to the job output |
//! | `hash` | `(src, len, dest)` | Writes the SHA-256 hash of `len` bytes at `src` to the 32 bytes at `dest` |
//! | `sig.verify` | `(key, message, len, signature) -> valid` | 1 if the 64 bytes at `signature` are the Ed25519 signature, by the 32-byte public key at `key`, of the `len` bytes at `message`; otherwise 0 |
//! | `seal` | `(key, src, len, dest) -> sealed` | Seals the `len` bytes at `src` to the 32-byte sealing key at `key`, writes the `len` + 48 sealed bytes at `dest`, and returns 1. Returns 0, and writes nothing, if `key` is not a usable key |
//! | `open` | `(src, len) -> len` | Opens the `len` sealed bytes at `src` with the key of the device the job runs on, and returns the message's length as a 64-bit integer, or -1 if that key does not open them. `result.read` then copies the message |
//! | `rand.seeded` | `(dest, len)` | Fills `len` bytes at `dest` from a stream fixed by the job's seed, so every node draws the same bytes |
//! | `blob.get` | `(hash) -> len` | Looks up the blob whose SHA-256 hash is the 32 bytes at `hash` among those the job was given to read, and returns its length as a 64-bit integer, or -1 if it is not one of them. `result.read` then copies it |
//! | `blob.put` | `(src, len, dest)` | Stores the `len` bytes at `src` as a blob and writes its hash to the 32 bytes at `dest`. The blob is kept only if the job completes |
//! | `kv.get` | `(key, len) -> len` | Looks up the record whose key is the `len` bytes at `key`, in the shard of state the job was handed, and returns its value's length as a 64-bit integer, or -1 if there is no such record. `result.read` then copies the value |
//! | `kv.put` | `(key, key_len, value, value_len)` | Sets the record with that key to that value. The change is kept only if the job completes |
//! | `kv.scan` | `(prefix, len) -> len` | Gathers the records whose keys start with the `len` bytes at `prefix`, in the byte form `decomp_schema::records` defines, and returns its length as a 64-bit integer. `result.read` then copies it |
//! | `result.read` | `(dest)` | Copies what the last `open`, `blob.get`, `kv.get`, or `kv.scan` found into memory at `dest` |
//!
//! The three `kv` functions end a job that was handed no shard.
//!
//! `seal` must give every node the same bytes, so it draws nothing at random:
//! what it does is fixed by the job's seed, the key, and the message (see
//! `decomp_core::seal::seal_fixed`, which also says what that gives up). `open`
//! opens only on a device that was given a key to open with, which is a device
//! running its own user's private job. A node has no such key, so on a node
//! `open` returns -1 whatever it is given, and every node agrees.
//!
//! | Function | Signature | Effect |
//! | --- | --- | --- |
//! | `job.spawn` | `(module, input, len, compute_cap)` | Asks for a follow-up job: the module whose hash is the 32 bytes at `module`, which must be one of the blobs the job was given to read, run on the `len` bytes at `input` with the 64-bit `compute_cap`. It is started only if this job completes and is verified |

mod meter;

use std::collections::HashMap;

use decomp_core::{
    ContentHash, MAX_SPAWNED, OpeningKey, PublicKey, SEAL_OVERHEAD_BYTES, SealingKey, Signature,
    seal,
};
use decomp_schema::records::{self, Records, record_bytes};
use sha2::{Digest, Sha256};
use wasmtime::{
    Caller, Config, Engine, Extern, ExternType, Linker, Module, Store, StoreLimits,
    StoreLimitsBuilder, Trap, Val,
};

use crate::meter::{COUNTER_EXPORT, HOST_MODULE, METER_IMPORT, add_metering};

/// WebAssembly instructions in one compute unit, the unit jobs are priced in
/// (spec section 5: "one unit is 1 billion WebAssembly instructions"). At the
/// base rate one credit buys 4,000 units, about one hour of one slot on a
/// reference core.
pub const INSTRUCTIONS_PER_UNIT: u64 = 1_000_000_000;

/// How often a running job's state is hashed into a checkpoint. The spec's
/// starting value is 10 million instructions; each checkpoint hashes the
/// module's whole memory, and at that interval the hashing costs more than the
/// job, so a checkpoint is taken once per compute unit instead.
pub const INSTRUCTIONS_PER_CHECKPOINT: u64 = INSTRUCTIONS_PER_UNIT;

/// Memory available to one job: a slot's 512 MB (spec section 1, "Slots").
pub const SLOT_MEMORY_BYTES: usize = 512 * 1024 * 1024;

/// Instructions charged per byte a host function reads or writes for a module, so
/// that work done through the host is metered like the module's own.
const HOST_INSTRUCTIONS_PER_BYTE: u64 = 1;

/// Instructions charged for checking one signature, on top of the bytes read.
/// Checking takes the host roughly as long as this many instructions take a module.
const SIGNATURE_CHECK_INSTRUCTIONS: u64 = 500_000;

/// Instructions charged for sealing or opening one message, on top of the bytes
/// read. Each is one or two operations on a curve, as checking a signature is, so
/// it is charged the same; it has not been measured on its own.
const SEAL_INSTRUCTIONS: u64 = SIGNATURE_CHECK_INSTRUCTIONS;

/// The most a job may hand back in all: its output and the blobs it stores. A
/// job holds no more than a slot's memory, so it has no use for more, and without
/// a bound a job could fill its host's memory for the price of one unit.
pub const MAX_PRODUCED_BYTES: usize = SLOT_MEMORY_BYTES;

/// The longest key a record of state may have.
pub const MAX_KEY_BYTES: usize = 1024;

/// The most one shard of state may hold, in its byte form. A job reads its whole
/// shard and writes it whole, so shards are kept small; an app with more state
/// than this cuts it into more shards.
pub const MAX_SHARD_BYTES: usize = 8 * 1024 * 1024;

/// The most a module may be charged for describing its signature.
const SIGNATURE_COMPUTE_CAP: u64 = 1;

const ENTRY_POINT: &str = "run";
const SIGNATURE_EXPORT: &str = "signature";
const MEMORY_EXPORT: &str = "memory";

/// The hash of a job's state at one point in its run: its memory, and how many
/// instructions it had executed.
pub type Checkpoint = [u8; 32];

/// A job to run.
#[derive(Clone, Copy)]
pub struct Job<'a> {
    pub module: &'a [u8],
    pub input: &'a [u8],
    /// Compute units the job may use before it is stopped.
    pub compute_cap: u64,
    /// What the job's randomness is drawn from. Runs with the same seed draw the
    /// same bytes.
    pub seed: [u8; 32],
    /// The blobs the job may read with `blob.get`: those its manifest names.
    pub blobs: &'a Blobs,
    /// The records of the shard of state the job is handed, in their byte form.
    /// `None` if it is handed no shard.
    pub state: Option<&'a [u8]>,
    /// The key the job's `open` opens with. A device gives its own when it runs
    /// a private job of its own user. A node is given no user's key, and neither
    /// is anyone who re-runs a job to check one, so they give `None`.
    pub opening_key: Option<&'a OpeningKey>,
}

/// Blobs by the hash of their contents.
pub type Blobs = HashMap<ContentHash, Vec<u8>>;

/// What a job that ran to its end handed back.
#[derive(Debug, Default)]
pub struct Produced {
    pub output: Vec<u8>,
    /// The blobs the job stored, in the order it stored them.
    pub blobs: Vec<Vec<u8>>,
    /// The records the job left its shard with, in their byte form, if it
    /// changed them.
    pub state: Option<Vec<u8>>,
    /// The follow-up jobs the job asked for, in order.
    pub spawned: Vec<Spawned>,
}

/// A follow-up job a job asked for.
#[derive(Debug)]
pub struct Spawned {
    /// The module to run: one of the blobs the asking job was given to read.
    pub module: ContentHash,
    pub input: Vec<u8>,
    pub compute_cap: u64,
}

/// What running a job produced and what it cost.
#[derive(Debug)]
pub struct Run {
    /// Compute units to bill: instructions executed, rounded up to whole units.
    /// A failed job is billed for the work it did before failing.
    pub units: u64,
    pub result: Result<Produced, Failure>,
    /// The job's state every `INSTRUCTIONS_PER_CHECKPOINT` instructions. When two
    /// nodes disagree, the first checkpoint where their lists differ shows how
    /// far the job has to be re-run to find out which is right.
    pub checkpoints: Vec<Checkpoint>,
}

/// What a node reports about a job, before anything is uploaded: the form in
/// which two runs of a job are compared.
pub struct Finding {
    pub units: u64,
    /// What the job handed back, or why it handed back nothing.
    pub produced: Result<Produced, decomp_core::Failure>,
    /// The job's checkpoints, one 32-byte hash after another.
    pub checkpoints: Vec<u8>,
}

/// A job that ran but did not finish. Every honest node sees the same failure.
#[derive(Debug, thiserror::Error)]
pub enum Failure {
    #[error("the job used its whole compute cap")]
    ComputeCapExceeded,
    /// The module hit a trap, or a host function refused one of its calls.
    #[error("{0}")]
    Trapped(String),
}

/// A job that could not be started.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("not a valid job module: {0}")]
    InvalidModule(String),
    #[error("the input is larger than a slot's memory")]
    InputTooLarge,
    #[error("the state the job was handed is not a shard's records")]
    InvalidState,
}

/// Why a host function stopped the job.
#[derive(Debug, thiserror::Error)]
enum Halt {
    #[error("the job used its whole compute cap")]
    CapReached,
    #[error("the module passed a range that is outside its memory")]
    OutOfBounds,
    /// The caller asked for one checkpoint only, and it has been taken.
    #[error("the job was stopped at the checkpoint it was run for")]
    CheckpointReached,
    #[error("the job handed back more than a slot's memory holds")]
    TooMuchProduced,
    #[error("the module asked to read a result when it had not just looked one up")]
    NothingToRead,
    #[error("the job was handed no state")]
    NoState,
    #[error("a record's key is longer than {MAX_KEY_BYTES} bytes")]
    KeyTooLong,
    #[error("the job left its shard holding more than a shard may")]
    TooMuchState,
    #[error(
        "a follow-up job runs one of the blobs the job was given to read, on a compute cap of at least 1, and a job may ask for at most {MAX_SPAWNED}"
    )]
    BadFollowUp,
}

/// Per-job state the host functions work on.
struct State {
    input: Vec<u8>,
    output: Vec<u8>,
    /// The blobs the job may read.
    blobs: Blobs,
    /// The blobs the job has stored.
    stored: Vec<Vec<u8>>,
    /// The records of the job's shard, as the job has left them so far, and how
    /// many bytes their byte form takes. `None` if it was handed no shard.
    records: Option<Records>,
    records_bytes: usize,
    /// What the module last looked up, until it reads it.
    found: Option<Found>,
    /// The follow-up jobs the job has asked for.
    spawned: Vec<Spawned>,
    limits: StoreLimits,
    /// The most instructions the job may execute.
    cap: u64,
    /// Instructions executed so far, as of the last call to the meter.
    used: u64,
    /// What the module's counter was last set to.
    granted: u64,
    checkpoints: Vec<Checkpoint>,
    /// Stop once this many checkpoints have been taken.
    stop_after: Option<usize>,
    seed: [u8; 32],
    /// How many blocks of the job's random stream have been drawn.
    random_blocks_drawn: u64,
    /// The key `open` opens with, if the job was given one.
    opening_key: Option<OpeningKey>,
}

/// What `result.read` copies next.
enum Found {
    /// One of the blobs the job may read.
    Blob(ContentHash),
    /// A record's value, the byte form of several records, or an opened message.
    Bytes(Vec<u8>),
}

/// A configured engine. Create one and reuse it for every job.
pub struct Sandbox {
    engine: Engine,
    linker: Linker<State>,
}

impl Default for Sandbox {
    fn default() -> Self {
        Self::new()
    }
}

impl Sandbox {
    pub fn new() -> Self {
        let mut config = Config::new();
        config
            .cranelift_nan_canonicalization(true)
            .relaxed_simd_deterministic(true)
            // Features the meter does not account for are switched off, so a module
            // that uses them is rejected instead of being metered wrongly.
            .wasm_tail_call(false)
            .wasm_memory64(false)
            // Backtraces vary with how a module was compiled and are not part of a result.
            .wasm_backtrace_max_frames(None);
        // Threads and exceptions need no setting: this crate builds wasmtime without
        // the features behind them, so modules that use either are rejected.
        let engine = Engine::new(&config).expect("the sandbox configuration is valid");

        let mut linker = Linker::new(&engine);
        linker
            .func_wrap(HOST_MODULE, "input.len", input_len)
            .and_then(|linker| linker.func_wrap(HOST_MODULE, "input.read", input_read))
            .and_then(|linker| linker.func_wrap(HOST_MODULE, "output.write", output_write))
            .and_then(|linker| linker.func_wrap(HOST_MODULE, "hash", hash))
            .and_then(|linker| linker.func_wrap(HOST_MODULE, "sig.verify", sig_verify))
            .and_then(|linker| linker.func_wrap(HOST_MODULE, "seal", seal_message))
            .and_then(|linker| linker.func_wrap(HOST_MODULE, "open", open_message))
            .and_then(|linker| linker.func_wrap(HOST_MODULE, "rand.seeded", rand_seeded))
            .and_then(|linker| linker.func_wrap(HOST_MODULE, "blob.get", blob_get))
            .and_then(|linker| linker.func_wrap(HOST_MODULE, "blob.put", blob_put))
            .and_then(|linker| linker.func_wrap(HOST_MODULE, "result.read", result_read))
            .and_then(|linker| linker.func_wrap(HOST_MODULE, "kv.get", kv_get))
            .and_then(|linker| linker.func_wrap(HOST_MODULE, "kv.put", kv_put))
            .and_then(|linker| linker.func_wrap(HOST_MODULE, "kv.scan", kv_scan))
            .and_then(|linker| linker.func_wrap(HOST_MODULE, "job.spawn", job_spawn))
            .and_then(|linker| linker.func_wrap(HOST_MODULE, METER_IMPORT, meter))
            .expect("host function names are unique");
        Self { engine, linker }
    }

    /// Runs a job to its end, or until it has used its compute cap.
    pub fn run(&self, job: &Job<'_>) -> Result<Run, RunError> {
        self.execute(job, ENTRY_POINT, None)
    }

    /// Runs a job and puts what happened in the terms results are reported in.
    /// A job that cannot be run at all is a finding too: every node finds the
    /// same, so it is that job's result.
    pub fn find(&self, job: &Job<'_>) -> Finding {
        match self.run(job) {
            Ok(run) => Finding {
                units: run.units,
                produced: run.result.map_err(|failure| match failure {
                    Failure::ComputeCapExceeded => decomp_core::Failure::ComputeCapExceeded,
                    Failure::Trapped(_) => decomp_core::Failure::Trapped,
                }),
                checkpoints: run.checkpoints.concat(),
            },
            Err(_) => Finding {
                units: 0,
                produced: Err(decomp_core::Failure::InvalidJob),
                checkpoints: Vec::new(),
            },
        }
    }

    /// Runs the job only as far as its checkpoint number `index` (counting from
    /// zero) and returns that checkpoint, or `None` if the job ends before
    /// reaching it. This is how a disputed job is refereed without running it all.
    pub fn checkpoint(&self, job: &Job<'_>, index: usize) -> Result<Option<Checkpoint>, RunError> {
        let run = self.execute(job, ENTRY_POINT, Some(index + 1))?;
        Ok(run.checkpoints.get(index).copied())
    }

    /// The description a module gives of its job's types, as the SDK wrote it, or
    /// `None` if the module does not describe itself.
    pub fn signature(&self, module: &[u8]) -> Result<Option<Vec<u8>>, RunError> {
        let job = Job {
            module,
            input: &[],
            compute_cap: SIGNATURE_COMPUTE_CAP,
            seed: [0; 32],
            blobs: &Blobs::new(),
            state: None,
            opening_key: None,
        };
        match self.execute(&job, SIGNATURE_EXPORT, None) {
            Ok(run) => Ok(run.result.ok().map(|produced| produced.output)),
            Err(RunError::InvalidModule(_)) if self.run_is_possible(module) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// What a module says of its job: its types and its privacy mode. `None` if
    /// the module does not describe itself, or cannot be run at all.
    pub fn declared(&self, module: &[u8]) -> Option<decomp_schema::Signature> {
        let described = self.signature(module).ok().flatten()?;
        serde_json::from_slice(&described).ok()
    }

    /// Whether `module` is a valid job module, leaving aside any signature.
    fn run_is_possible(&self, module: &[u8]) -> bool {
        add_metering(module)
            .ok()
            .and_then(|metered| Module::new(&self.engine, metered).ok())
            .is_some_and(|module| check_exports(&module, ENTRY_POINT).is_ok())
    }

    fn execute(
        &self,
        job: &Job<'_>,
        entry_point: &str,
        stop_after: Option<usize>,
    ) -> Result<Run, RunError> {
        if job.input.len() > SLOT_MEMORY_BYTES {
            return Err(RunError::InputTooLarge);
        }
        let handed = job.state.filter(|bytes| bytes.len() <= MAX_SHARD_BYTES);
        let records = handed.map(records::decode);
        if records.is_none() != job.state.is_none() || records == Some(None) {
            return Err(RunError::InvalidState);
        }
        let metered =
            add_metering(job.module).map_err(|e| RunError::InvalidModule(e.to_string()))?;
        let invalid = |error: wasmtime::Error| RunError::InvalidModule(error.to_string());
        let module = Module::new(&self.engine, metered).map_err(invalid)?;
        check_exports(&module, entry_point)?;
        // Resolving imports here means that from now on only the job itself can fail.
        let ready = self.linker.instantiate_pre(&module).map_err(invalid)?;

        let state = State {
            input: job.input.to_vec(),
            output: Vec::new(),
            blobs: job.blobs.clone(),
            stored: Vec::new(),
            records: records.flatten(),
            records_bytes: job.state.map_or(0, <[u8]>::len),
            found: None,
            spawned: Vec::new(),
            limits: StoreLimitsBuilder::new()
                .memory_size(SLOT_MEMORY_BYTES)
                .build(),
            cap: job.compute_cap.saturating_mul(INSTRUCTIONS_PER_UNIT),
            used: 0,
            granted: 0,
            checkpoints: Vec::new(),
            stop_after,
            seed: job.seed,
            random_blocks_drawn: 0,
            opening_key: job.opening_key.map(|key| OpeningKey::from_seed(key.seed())),
        };
        let mut store = Store::new(&self.engine, state);
        store.limiter(|state| &mut state.limits);

        let mut counter = None;
        let finished = ready.instantiate(&mut store).and_then(|instance| {
            counter = instance.get_global(&mut store, COUNTER_EXPORT);
            let entry = instance.get_typed_func::<(), ()>(&mut store, entry_point)?;
            entry.call(&mut store, ())
        });

        // The meter last counted when the module last called it; add what the
        // module has executed since, which is what its counter has gone down by.
        let left = match counter.map(|counter| counter.get(&mut store)) {
            Some(Val::I64(left)) => left,
            _ => 0,
        };
        let state = store.into_data();
        let since_meter = state.granted.saturating_add_signed(-left);
        let used = state.used.saturating_add(since_meter).min(state.cap);
        Ok(Run {
            units: used.div_ceil(INSTRUCTIONS_PER_UNIT).max(1),
            result: match finished {
                Ok(()) => Ok(Produced {
                    output: state.output,
                    blobs: state.stored,
                    state: state
                        .records
                        .map(|records| records::encode(&records))
                        .filter(|left| Some(left.as_slice()) != job.state),
                    spawned: state.spawned,
                }),
                Err(error) => Err(failure_of(&error)),
            },
            checkpoints: state.checkpoints,
        })
    }
}

fn check_exports(module: &Module, entry_point: &str) -> Result<(), RunError> {
    let entry_ok = matches!(
        module.get_export(entry_point),
        Some(ExternType::Func(f)) if f.params().len() == 0 && f.results().len() == 0
    );
    if !entry_ok {
        let message =
            format!("it must export a function `{entry_point}` that takes and returns nothing");
        return Err(RunError::InvalidModule(message));
    }
    if !matches!(
        module.get_export(MEMORY_EXPORT),
        Some(ExternType::Memory(_))
    ) {
        let message = format!("it must export its memory as `{MEMORY_EXPORT}`");
        return Err(RunError::InvalidModule(message));
    }
    Ok(())
}

fn failure_of(error: &wasmtime::Error) -> Failure {
    if let Some(Halt::CapReached | Halt::CheckpointReached) = error.downcast_ref::<Halt>() {
        // A run stopped at a checkpoint is reported like one that ran out of
        // budget; `Sandbox::checkpoint` only reads the checkpoint.
        return Failure::ComputeCapExceeded;
    }
    match error.downcast_ref::<Trap>() {
        Some(trap) => Failure::Trapped(trap.to_string()),
        // A host function refused a call, or the engine enforced a limit.
        None => Failure::Trapped(error.root_cause().to_string()),
    }
}

/// The module's memory, alongside the job state, for a host function to work on.
fn memory_and_state<'a>(caller: &'a mut Caller<'_, State>) -> (&'a mut [u8], &'a mut State) {
    let memory = caller
        .get_export(MEMORY_EXPORT)
        .and_then(Extern::into_memory)
        .expect("check_exports confirmed the memory export");
    memory.data_and_store_mut(caller)
}

/// The `len` bytes of a module's memory starting at `start`.
fn range(memory: &[u8], start: u32, len: usize) -> Result<&[u8], Halt> {
    let start = start as usize;
    start
        .checked_add(len)
        .and_then(|end| memory.get(start..end))
        .ok_or(Halt::OutOfBounds)
}

fn range_mut(memory: &mut [u8], start: u32, len: usize) -> Result<&mut [u8], Halt> {
    let start = start as usize;
    start
        .checked_add(len)
        .and_then(|end| memory.get_mut(start..end))
        .ok_or(Halt::OutOfBounds)
}

/// Bills the job for host work, ending it if that takes it past its compute cap.
fn charge(state: &mut State, instructions: u64) -> Result<(), Halt> {
    state.used = state.used.saturating_add(instructions);
    if state.used >= state.cap {
        Err(Halt::CapReached)
    } else {
        Ok(())
    }
}

fn charge_for_bytes(state: &mut State, bytes: usize) -> Result<(), Halt> {
    charge(
        state,
        (bytes as u64).saturating_mul(HOST_INSTRUCTIONS_PER_BYTE),
    )
}

/// Called by the module when its instruction counter runs out. Counts what was
/// executed, takes a checkpoint, and returns the counter's new value: the
/// instructions the job may execute before it must call again.
fn meter(mut caller: Caller<'_, State>, left: i64) -> wasmtime::Result<i64> {
    let (memory, state) = memory_and_state(&mut caller);
    // `left` is zero or negative: the module ran `granted - left` instructions.
    state.used = state
        .used
        .saturating_add(state.granted.saturating_add_signed(-left));
    if state.used >= state.cap {
        return Err(Halt::CapReached.into());
    }

    let mut hasher = Sha256::new();
    hasher.update(state.used.to_le_bytes());
    hasher.update(&*memory);
    state.checkpoints.push(hasher.finalize().into());
    if state.stop_after == Some(state.checkpoints.len()) {
        return Err(Halt::CheckpointReached.into());
    }

    state.granted = INSTRUCTIONS_PER_CHECKPOINT.min(state.cap - state.used);
    Ok(state.granted as i64)
}

fn input_len(caller: Caller<'_, State>) -> u32 {
    // Cannot truncate: `execute` rejects inputs larger than a slot's memory.
    caller.data().input.len() as u32
}

fn input_read(mut caller: Caller<'_, State>, dest: u32) -> wasmtime::Result<()> {
    let (memory, state) = memory_and_state(&mut caller);
    charge_for_bytes(state, state.input.len())?;
    range_mut(memory, dest, state.input.len())?.copy_from_slice(&state.input);
    Ok(())
}

fn output_write(mut caller: Caller<'_, State>, src: u32, len: u32) -> wasmtime::Result<()> {
    let (memory, state) = memory_and_state(&mut caller);
    charge_for_bytes(state, len as usize)?;
    make_room(state, len as usize)?;
    state
        .output
        .extend_from_slice(range(memory, src, len as usize)?);
    Ok(())
}

/// Ends the job if handing back `more` bytes would take it past what it may.
fn make_room(state: &State, more: usize) -> Result<(), Halt> {
    let stored: usize = state.stored.iter().map(Vec::len).sum();
    let inputs: usize = state.spawned.iter().map(|asked| asked.input.len()).sum();
    if state.output.len() + stored + inputs + more > MAX_PRODUCED_BYTES {
        Err(Halt::TooMuchProduced)
    } else {
        Ok(())
    }
}

fn blob_get(mut caller: Caller<'_, State>, hash: u32) -> wasmtime::Result<i64> {
    let (memory, state) = memory_and_state(&mut caller);
    let hash = ContentHash::from_bytes(range(memory, hash, 32)?.try_into().expect("32 bytes"));
    let len = state.blobs.get(&hash).map(Vec::len);
    state.found = len.map(|_| Found::Blob(hash));
    Ok(len.map_or(-1, |len| len as i64))
}

fn result_read(mut caller: Caller<'_, State>, dest: u32) -> wasmtime::Result<()> {
    let (memory, state) = memory_and_state(&mut caller);
    let found = state.found.take().ok_or(Halt::NothingToRead)?;
    let bytes = match &found {
        Found::Blob(hash) => &state.blobs[hash],
        Found::Bytes(bytes) => bytes,
    };
    let len = bytes.len();
    range_mut(memory, dest, len)?.copy_from_slice(bytes);
    charge_for_bytes(state, len)?;
    Ok(())
}

fn job_spawn(
    mut caller: Caller<'_, State>,
    module: u32,
    input: u32,
    len: u32,
    compute_cap: i64,
) -> wasmtime::Result<()> {
    let (memory, state) = memory_and_state(&mut caller);
    charge_for_bytes(state, len as usize)?;
    make_room(state, len as usize)?;
    let module = ContentHash::from_bytes(range(memory, module, 32)?.try_into().expect("32 bytes"));
    let allowed =
        state.blobs.contains_key(&module) && compute_cap >= 1 && state.spawned.len() < MAX_SPAWNED;
    if !allowed {
        return Err(Halt::BadFollowUp.into());
    }
    state.spawned.push(Spawned {
        module,
        input: range(memory, input, len as usize)?.to_vec(),
        compute_cap: compute_cap as u64,
    });
    Ok(())
}

/// The bytes a module names as a record's key or as the start of several.
fn key(memory: &[u8], start: u32, len: u32) -> Result<&[u8], Halt> {
    if len as usize > MAX_KEY_BYTES {
        return Err(Halt::KeyTooLong);
    }
    range(memory, start, len as usize)
}

fn kv_get(mut caller: Caller<'_, State>, at: u32, len: u32) -> wasmtime::Result<i64> {
    let (memory, state) = memory_and_state(&mut caller);
    let records = state.records.as_ref().ok_or(Halt::NoState)?;
    let value = records.get(key(memory, at, len)?).cloned();
    charge_for_bytes(state, value.as_ref().map_or(0, Vec::len))?;
    let found = value.as_ref().map_or(-1, |value| value.len() as i64);
    state.found = value.map(Found::Bytes);
    Ok(found)
}

fn kv_put(
    mut caller: Caller<'_, State>,
    at: u32,
    len: u32,
    value: u32,
    value_len: u32,
) -> wasmtime::Result<()> {
    let (memory, state) = memory_and_state(&mut caller);
    charge_for_bytes(state, len as usize + value_len as usize)?;
    let key = key(memory, at, len)?.to_vec();
    let value = range(memory, value, value_len as usize)?.to_vec();
    let records = state.records.as_mut().ok_or(Halt::NoState)?;

    let added = record_bytes(&key, &value);
    let removed = records.get(&key).map_or(0, |old| record_bytes(&key, old));
    let left = state.records_bytes + added - removed;
    if left > MAX_SHARD_BYTES {
        return Err(Halt::TooMuchState.into());
    }
    state.records_bytes = left;
    records.insert(key, value);
    Ok(())
}

fn kv_scan(mut caller: Caller<'_, State>, at: u32, len: u32) -> wasmtime::Result<i64> {
    let (memory, state) = memory_and_state(&mut caller);
    let records = state.records.as_ref().ok_or(Halt::NoState)?;
    let prefix = key(memory, at, len)?;
    let matching: Records = records
        .range(prefix.to_vec()..)
        .take_while(|(key, _)| key.starts_with(prefix))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let found = records::encode(&matching);
    charge_for_bytes(state, found.len())?;
    let len = found.len() as i64;
    state.found = Some(Found::Bytes(found));
    Ok(len)
}

fn blob_put(mut caller: Caller<'_, State>, src: u32, len: u32, dest: u32) -> wasmtime::Result<()> {
    let (memory, state) = memory_and_state(&mut caller);
    charge_for_bytes(state, len as usize)?;
    make_room(state, len as usize)?;
    let bytes = range(memory, src, len as usize)?.to_vec();
    let hash = ContentHash::of(&bytes);
    range_mut(memory, dest, 32)?.copy_from_slice(hash.as_bytes());
    state.stored.push(bytes);
    Ok(())
}

fn hash(mut caller: Caller<'_, State>, src: u32, len: u32, dest: u32) -> wasmtime::Result<()> {
    let (memory, state) = memory_and_state(&mut caller);
    charge_for_bytes(state, len as usize)?;
    let digest = Sha256::digest(range(memory, src, len as usize)?);
    range_mut(memory, dest, digest.len())?.copy_from_slice(&digest);
    Ok(())
}

fn sig_verify(
    mut caller: Caller<'_, State>,
    key: u32,
    message: u32,
    len: u32,
    signature: u32,
) -> wasmtime::Result<u32> {
    let (memory, state) = memory_and_state(&mut caller);
    charge(state, SIGNATURE_CHECK_INSTRUCTIONS)?;
    charge_for_bytes(state, len as usize)?;
    let key = PublicKey::from_bytes(range(memory, key, 32)?.try_into().expect("32 bytes"));
    let signature =
        Signature::from_bytes(range(memory, signature, 64)?.try_into().expect("64 bytes"));
    Ok(key.verifies(range(memory, message, len as usize)?, &signature) as u32)
}

/// Seals a message to a key. The job's seed stands in for the randomness
/// sealing would otherwise draw, so every node writes the same bytes.
fn seal_message(
    mut caller: Caller<'_, State>,
    key: u32,
    src: u32,
    len: u32,
    dest: u32,
) -> wasmtime::Result<u32> {
    let (memory, state) = memory_and_state(&mut caller);
    charge(state, SEAL_INSTRUCTIONS)?;
    charge_for_bytes(state, len as usize)?;
    let key = SealingKey::from_bytes(range(memory, key, 32)?.try_into().expect("32 bytes"));
    let message = range(memory, src, len as usize)?;
    let Some(sealed) = seal::seal_fixed(&state.seed, &key, message) else {
        return Ok(0);
    };
    range_mut(memory, dest, len as usize + SEAL_OVERHEAD_BYTES)?.copy_from_slice(&sealed);
    Ok(1)
}

/// Opens a sealed message with the key the job was given to open with, if it
/// was given one, for `result.read` to copy.
fn open_message(mut caller: Caller<'_, State>, src: u32, len: u32) -> wasmtime::Result<i64> {
    let (memory, state) = memory_and_state(&mut caller);
    charge(state, SEAL_INSTRUCTIONS)?;
    charge_for_bytes(state, len as usize)?;
    let sealed = range(memory, src, len as usize)?;
    let opened = state
        .opening_key
        .as_ref()
        .and_then(|key| seal::open(key, sealed));
    let found = opened.as_ref().map_or(-1, |message| message.len() as i64);
    state.found = opened.map(Found::Bytes);
    Ok(found)
}

/// Fills the module's buffer from the job's random stream: block `n` of the
/// stream is the SHA-256 hash of the seed followed by `n`.
fn rand_seeded(mut caller: Caller<'_, State>, dest: u32, len: u32) -> wasmtime::Result<()> {
    let (memory, state) = memory_and_state(&mut caller);
    charge_for_bytes(state, len as usize)?;
    for chunk in range_mut(memory, dest, len as usize)?.chunks_mut(32) {
        let mut hasher = Sha256::new();
        hasher.update(state.seed);
        hasher.update(state.random_blocks_drawn.to_le_bytes());
        chunk.copy_from_slice(&hasher.finalize()[..chunk.len()]);
        state.random_blocks_drawn += 1;
    }
    Ok(())
}
