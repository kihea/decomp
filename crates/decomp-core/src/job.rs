//! What a job is on the wire: the manifest an application submits, the result a
//! node reports, and the receipt that proves the work (spec sections 2 and 6).

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::envelope::Envelope;
use crate::keys::{AccountId, ContentHash, JobId, PublicKey, ShareSet};
use crate::ledger::{Credits, FeeSplit, fee};
use crate::privacy::{Privacy, PrivateRoute};
use crate::state::ShardRef;

/// How many nodes run a job when the first node drawn for it is still on
/// probation and the submitter has not asked for a number.
pub const UNPROVEN_REDUNDANCY: u32 = 2;

/// The most blobs a job may be given to read, besides its input.
pub const MAX_READS: usize = 64;

/// The most follow-up jobs one job may ask for.
pub const MAX_SPAWNED: usize = 64;

/// The highest size tier. Jobs of 16 units or more all fall in it.
pub const MAX_TIER: u32 = 4;

/// The most keys a job may name whose accounts' nodes it is kept from.
pub const MAX_KEPT_FROM: usize = 64;

/// The most earlier jobs a job may name whose nodes' accounts it is kept from.
pub const MAX_APART_FROM: usize = 8;

/// What to run and within what limits (spec section 2, "Manifest fields"). Fields
/// the spec lists but that nothing reads yet are added with the features that
/// read them.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub module: ContentHash,
    pub input: ContentHash,
    /// Compute units the job may use before it is stopped.
    pub compute_cap: u64,
    /// Latest acceptable finish time, in seconds since 1970.
    pub deadline: u64,
    /// How many nodes, each from a different account, must run the job. Left out,
    /// the coordinator decides: one proven node, spot-checked now and then, or
    /// two nodes when the first one drawn is on probation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redundancy: Option<u32>,
    /// How to cut the job into pieces that run independently. Each piece is a job
    /// of its own, with this manifest's limits, on its own part of the input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub split: Option<Split>,
    /// Credits set aside for the job. It must cover the highest possible fee:
    /// every paid node using the whole compute cap. What is not spent is returned.
    pub max_fee: Credits,
    /// The app that pays for the job, by its ID. Left out, the account that
    /// submits the job pays. An app pays only for jobs that run its own modules,
    /// and only up to what its manifest allows (see `app`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sponsor: Option<AccountId>,
    /// Blobs the job may read with `blob.get`, besides its input. Each must be
    /// stored before the job is submitted. A job cannot read any other blob: if
    /// it could, a node that ran it before some blob was stored and a node that
    /// ran it after would honestly disagree.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reads: Vec<ContentHash>,
    /// The shard of an app's state the job is handed, to read and to change (see
    /// `state`). Only a job that runs one of the app's own modules may be handed
    /// its state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<ShardRef>,
    /// The mode the job runs in: "open or private" (spec section 2). Left out,
    /// open. It must be what the job's module declares (see `privacy`).
    #[serde(default, skip_serializing_if = "Privacy::is_open")]
    pub privacy: Privacy,
    /// Set when the job is one share of a private job (see `shares`). Only a
    /// private job may be a share.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub share: Option<Share>,
    /// Keys whose accounts' nodes are never handed the job. A key is an
    /// account's own or one of its devices'; a key that belongs to no account
    /// bars nobody. This is how a job is kept from the nodes of someone with a
    /// stake in its result. A node is not told which keys a job names.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keep_from: Vec<PublicKey>,
    /// Earlier jobs whose nodes this job is kept apart from: no node of an
    /// account that was handed one of them is handed this one. This is how a
    /// job that is run again and again goes to different nodes each time. A job
    /// that has been forgotten, or was never known, bars nobody. A node is not
    /// told which jobs a job names.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub apart_from: Vec<JobId>,
}

/// Which share of which private job a job is (see `shares`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Share {
    /// The name the submitter's device gave the private job's shares. Jobs from
    /// one account that carry the same name are never handed to nodes of the
    /// same account, so no account holds two shares of anything.
    pub set: ShareSet,
    /// The share's place among the job's shares, counting from zero. It tells
    /// apart two shares that are otherwise alike, as the shares of an input
    /// with no private number in it are.
    pub index: u32,
}

/// The rule for cutting a job into pieces (spec section 2, "Decomposition"). The
/// input is cut into runs of `chunk_bytes` bytes, the module runs on each, and the
/// job's output is the pieces' outputs joined in order. No node sees more of the
/// input than its own piece.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Split {
    pub chunk_bytes: u64,
}

/// The most pieces one job may be cut into.
pub const MAX_PIECES: u64 = 1024;

impl Split {
    /// How many pieces an input of `input_bytes` bytes is cut into. An empty input
    /// is still one piece.
    pub fn pieces(&self, input_bytes: u64) -> u64 {
        input_bytes.div_ceil(self.chunk_bytes.max(1)).max(1)
    }
}

impl Manifest {
    /// The most the job, or each of its pieces if it is split, can cost its
    /// submitter.
    pub fn highest_fee(&self) -> Credits {
        fee(
            self.compute_cap,
            self.redundancy.unwrap_or(UNPROVEN_REDUNDANCY),
        )
    }

    /// The job's size tier: how many times its compute cap doubles the smallest
    /// job's (spec section 3). One unit is tier 0, two or three are tier 1, and
    /// so on.
    pub fn tier(&self) -> u32 {
        self.compute_cap.max(1).ilog2().min(MAX_TIER)
    }

    /// The manifest as a node is handed it: without the keys and jobs it is
    /// kept from, which would tell the node whose result the job matters to.
    pub fn as_handed(&self) -> Self {
        Self {
            keep_from: Vec::new(),
            apart_from: Vec::new(),
            ..self.clone()
        }
    }
}

/// What one node found when it ran a job. Honest nodes report identical results,
/// so results are compared whole: the units and checkpoints as well as the outcome.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobResult {
    pub job_id: JobId,
    pub units: u64,
    pub outcome: Outcome,
    /// The hash of the blob holding the job's checkpoints, one 32-byte hash after
    /// another. If nodes disagree, the first checkpoint where their lists differ
    /// shows how far the job must be re-run to find out which is right.
    pub checkpoints: ContentHash,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum Outcome {
    Completed {
        output: ContentHash,
        /// The blobs the job stored with `blob.put`, in the order it stored them.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        blobs: Vec<ContentHash>,
        /// The contents the job left its shard with, if it changed them.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state: Option<ContentHash>,
        /// The follow-up jobs the job asked for with `job.spawn`, in order.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        spawned: Vec<Spawn>,
    },
    Failed {
        failure: Failure,
    },
}

/// A follow-up job that a job asked for (spec section 7, `job.spawn`). It is
/// started once the job that asked is verified, with that job's deadline, node
/// count, sponsor, and blobs to read, and is paid for out of what that job left
/// of its fee cap. Follow-ups are started in order for as long as that covers
/// the most each could cost; the rest are not started.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spawn {
    /// The module to run: one of the blobs the asking job was given to read.
    pub module: ContentHash,
    pub input: ContentHash,
    pub compute_cap: u64,
}

/// Why a job did not complete. These are results, not errors: every honest node
/// sees the same failure, and the work done before it is still billed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Failure {
    ComputeCapExceeded,
    Trapped,
    /// The module or its input cannot be run at all.
    InvalidJob,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ComputeCapExceeded => "the job used its whole compute cap",
            Self::Trapped => "the job trapped",
            Self::InvalidJob => "the module or its input cannot be run",
        })
    }
}

/// A job handed to a node to run.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Assignment {
    pub job_id: JobId,
    pub manifest: Manifest,
    /// What the job draws its randomness from: the ID the job was first submitted
    /// under. It differs from `job_id` only when a verified job is being reissued
    /// to check a node, which must reproduce the original run.
    pub seed: JobId,
    /// The contents of the job's shard as they were when the job was first handed
    /// out. `None` if the job is handed no shard, or the shard was empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shard: Option<ContentHash>,
}

/// How a job's result came to stand. The first of these that is true of any
/// piece of a split job is what is said of the whole job.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verification {
    /// One node ran the job and no other was asked to. Nothing was compared.
    /// This is how a proven node runs most jobs (spec section 4, "Earning spot
    /// checks"): the result rests on the node's record, and on its not knowing
    /// which of its jobs are checked. It is also what a submitter gets who asks
    /// for one node.
    Unchecked,
    /// The nodes that ran the job disagreed, and the referee ran it to find
    /// which of them were right.
    Refereed,
    /// Every node that ran the job, two or more, reported the same result.
    Agreed,
}

/// Proof of a finished job (spec section 6, "Receipts"). `results` holds the signed
/// request of each node whose result stood, so anyone can recheck them.
///
/// With `fee`, which is what the job cost, the last four fields are what the
/// spec has every result report (section 7): "the privacy route used, how the
/// result was verified, what it cost, how long it took, and whether an enclave
/// was involved".
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Receipt {
    pub job_id: JobId,
    pub module: ContentHash,
    pub input: ContentHash,
    /// Units each node used.
    pub units: u64,
    pub fee: FeeSplit,
    pub outcome: Outcome,
    /// The shard the job was handed, if any, and the contents it had then
    /// (`None` for an empty shard). The contents the job left are in `outcome`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<ShardRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handed: Option<ContentHash>,
    pub results: Vec<Envelope>,
    /// For a job that was split: its pieces, in order. Each piece has a receipt of
    /// its own holding its nodes' reports; this one totals their units and fees.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pieces: Vec<JobId>,
    /// The private route the job was run by. Left out for an open job, which is
    /// run by none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<PrivateRoute>,
    pub verified: Verification,
    /// Seconds from when the job was submitted to when its result stood.
    pub seconds: u64,
    /// Whether the job ran inside a hardware enclave. No node can yet show that
    /// it runs in one, so no job is sent to one, and this is always false.
    pub enclave: bool,
}

/// Where a job stands, as its submitter sees it.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum JobStatus {
    /// Not yet handed to nodes.
    Waiting,
    /// Handed to nodes; not all have reported, or their results are being refereed.
    Running,
    /// A result stood: every node agreed, or a referee found which was right.
    Verified { receipt: Box<Receipt> },
    /// The nodes disagreed and the referee found none of them right.
    Disputed,
    /// The deadline passed before the job was verified.
    Expired,
}
