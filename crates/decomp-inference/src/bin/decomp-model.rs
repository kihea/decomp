//! `decomp-model`: prepares a model to be stored on the network and tried
//! out.
//!
//! ```text
//! decomp-model pack <hugging-face-folder> --name qwen3-0.6b --stages 2 --out <folder> [--precision bf16]
//! decomp-model try <packed-folder> --prompt "The capital of France is" --tokens 16
//! ```
//!
//! `pack` cuts a Hugging Face model (`config.json`, `model.safetensors`,
//! `tokenizer.json`) into the files a stored model is published as, to be
//! computed in 32-bit floats (the default) or in bfloat16, writes
//! them and the card to the output folder, and prints the card's root: the
//! model's ID. Each file is then stored with `decomp-node store`, and the ID
//! added to the approved list.
//!
//! `try` runs a packed model here, every stage in this process, generates
//! with proofs, checks the proofs by reading the text in one pass, and shows
//! the text and the times.

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use decomp_core::Sampling;
use decomp_inference::card::{Card, pack};
use decomp_inference::decoder::{self, Ends, Precision, Stage};
use decomp_inference::{LocalStage, Pipeline, StageLink, generate, validate};

fn usage() -> Box<dyn Error> {
    "usage: decomp-model pack <folder> --name NAME --stages N --out FOLDER [--precision f32|bf16]\n       decomp-model try <packed-folder> [--prompt TEXT] [--tokens N]".into()
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

fn file_name(index: usize) -> String {
    match index {
        0 => "tokenizer.json".into(),
        1 => "ends.safetensors".into(),
        n => format!("stage-{}.safetensors", n - 2),
    }
}

fn pack_folder(args: &[String]) -> Result<(), Box<dyn Error>> {
    let folder = PathBuf::from(args.first().ok_or_else(usage)?);
    let name = flag(args, "--name").ok_or_else(usage)?;
    let stages: usize = flag(args, "--stages").ok_or_else(usage)?.parse()?;
    let out = PathBuf::from(flag(args, "--out").ok_or_else(usage)?);
    let precision = match flag(args, "--precision") {
        Some(name) => Precision::parse(&name).ok_or_else(usage)?,
        None => Precision::F32,
    };
    let config: serde_json::Value = serde_json::from_slice(&fs::read(folder.join("config.json"))?)?;
    let weights = fs::read(folder.join("model.safetensors"))?;
    let tokenizer = fs::read(folder.join("tokenizer.json"))?;
    let (card, files) = pack(&name, &config, &weights, tokenizer, stages, precision)?;
    fs::create_dir_all(&out)?;
    for (index, bytes) in files.iter().enumerate() {
        fs::write(out.join(file_name(index)), bytes)?;
    }
    fs::write(out.join("card.json"), card.to_bytes())?;
    println!("model {}  {}", card.name, card.id());
    println!(
        "tokenizer  {}  {} bytes",
        card.tokenizer.root, card.tokenizer.bytes
    );
    println!("ends       {}  {} bytes", card.ends.root, card.ends.bytes);
    for (index, stage) in card.stages.iter().enumerate() {
        println!(
            "stage {index}    {}  {} bytes, layers {}..{}",
            stage.part.root,
            stage.part.bytes,
            stage.first_layer,
            stage.first_layer + stage.layers
        );
    }
    Ok(())
}

fn read_card(folder: &Path) -> Result<Card, Box<dyn Error>> {
    Ok(serde_json::from_slice(&fs::read(
        folder.join("card.json"),
    )?)?)
}

fn try_folder(args: &[String]) -> Result<(), Box<dyn Error>> {
    let folder = PathBuf::from(args.first().ok_or_else(usage)?);
    let prompt = flag(args, "--prompt").unwrap_or_else(|| "The capital of France is".into());
    let max_tokens: u32 = flag(args, "--tokens").map_or(Ok(16), |n| n.parse())?;
    let card = read_card(&folder)?;
    let device = decoder::device();
    let started = Instant::now();
    let tokenizer = tokenizers::Tokenizer::from_file(folder.join("tokenizer.json"))
        .map_err(|e| e.to_string())?;
    let ends = Ends::load(
        &card.config,
        &fs::read(folder.join("ends.safetensors"))?,
        &device,
    )?;
    let stages = card
        .stages
        .iter()
        .enumerate()
        .map(|(index, part)| {
            let bytes = fs::read(folder.join(format!("stage-{index}.safetensors")))?;
            Ok(Stage::load(
                &card.config,
                part.first_layer,
                part.layers,
                &bytes,
                &device,
            )?)
        })
        .collect::<Result<Vec<Stage>, Box<dyn Error>>>()?;
    println!(
        "loaded {} in {:.1} s",
        card.name,
        started.elapsed().as_secs_f64()
    );

    let pipeline = || {
        let links: Vec<Box<dyn StageLink + '_>> = stages
            .iter()
            .map(|stage| Box::new(LocalStage::new(stage)) as Box<dyn StageLink>)
            .collect();
        Pipeline::new(&ends, &card.config, links)
    };
    let encoded = tokenizer
        .encode(prompt.as_str(), false)
        .map_err(|e| e.to_string())?;
    let tokens: Vec<u32> = encoded.get_ids().to_vec();
    let sampling = Sampling {
        temperature_milli: 0,
        seed: [0; 32],
    };
    let started = Instant::now();
    let generated = generate(&mut pipeline(), &tokens, max_tokens, &sampling)?;
    let took = started.elapsed();
    let text = tokenizer
        .decode(&generated.tokens, false)
        .map_err(|e| e.to_string())?;
    println!("{prompt}|{text}");
    println!(
        "generated {max_tokens} tokens in {:.2} s ({:.0} ms a token), {} proofs",
        took.as_secs_f64(),
        took.as_secs_f64() * 1e3 / f64::from(max_tokens.max(1)),
        generated.proofs.len()
    );
    let started = Instant::now();
    let checked = validate(&mut pipeline(), &tokens, max_tokens, &sampling, &generated);
    println!(
        "checked in one pass in {:.2} s: {}",
        started.elapsed().as_secs_f64(),
        match &checked {
            Ok(()) => "accepted".to_string(),
            Err(rejection) => format!("rejected: {rejection}"),
        }
    );
    let mut changed = generated.clone();
    if let Some(token) = changed.tokens.get_mut(max_tokens as usize / 2) {
        *token = (*token + 1) % card.config.vocab as u32;
    }
    let tampered = validate(&mut pipeline(), &tokens, max_tokens, &sampling, &changed);
    println!(
        "one token changed: {}",
        match tampered {
            Ok(()) => "accepted".to_string(),
            Err(rejection) => format!("rejected: {rejection}"),
        }
    );
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("pack") => pack_folder(&args[1..]),
        Some("try") => try_folder(&args[1..]),
        _ => Err(usage()),
    }
}
