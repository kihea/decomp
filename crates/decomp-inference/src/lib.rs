//! Runs DeComp's approved models on a node (spec section 2, "Inference"; section
//! 4, "TOPLOC proof"): generates text with TOPLOC proofs, and checks the text
//! and proofs another node handed back by running the model once over them.
//!
//! The proof format and the rules of the check are protocol rules and live in
//! `decomp_core::toploc` and `decomp_core::inference`; this crate holds the
//! models and the arithmetic: the tiny approved models (`tiny`), and stored
//! models of the Llama family (`decoder`), cut into files (`card`) and run as
//! a pipeline whose stages may be on other nodes (`runner`).

pub mod card;
pub mod decoder;
mod generate;
mod model;
pub mod runner;
pub mod tiny;

pub use generate::{
    Generated, Rejection, embed, fits, generate, proof_count, validate, validate_embedding,
};
pub use model::{Config, Model, ModelError, Session, Step};
pub use runner::{LocalStage, Pipeline, Runner, StageLink};
/// The tokenizer library stored models' tokenizers are read with.
pub use tokenizers;

use decomp_core::{ContentHash, inference::ApprovedModel, merkle};

/// The model's ID: the root of its weights file.
pub fn id_of(model: &Model) -> ContentHash {
    merkle::root(&model.to_bytes())
}

/// Builds the approved model `approved`, if this crate can, and checks that it
/// is the model the approved list names.
pub fn load(approved: &ApprovedModel) -> Option<Model> {
    let model = tiny::build(approved.name)?;
    (id_of(&model) == approved.id).then_some(model)
}
