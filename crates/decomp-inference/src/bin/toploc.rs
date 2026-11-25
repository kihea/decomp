//! `toploc`: shows TOPLOC at work on the tiny approved models. It prints each
//! model's ID, then generates text with proofs and has the proofs checked by
//! prefill against the right model and against models a dishonest node might
//! use instead, and against changed text, and reports what each check found
//! and how long generating, proving, and checking took.
//!
//! ```text
//! toploc [model] [tokens]
//! ```

use std::env;
use std::time::Instant;

use decomp_core::Sampling;
use decomp_core::toploc;
use decomp_inference::{Generated, Model, Rejection, generate, id_of, tiny, validate};

/// The prompt every check uses.
const PROMPT: &[u8] = b"The quick brown fox jumps over the lazy dog.";

fn report(what: &str, outcome: Result<(), Rejection>) {
    match outcome {
        Ok(()) => println!("  {what:<52} accepted"),
        Err(rejection) => println!("  {what:<52} rejected: {rejection}"),
    }
}

fn main() {
    let mut args = env::args().skip(1);
    let name = args.next().unwrap_or_else(|| "tiny-1".into());
    let tokens: u32 = args.next().and_then(|n| n.parse().ok()).unwrap_or(128);

    for (known, _) in tiny::MODELS {
        let model = tiny::build(known).expect("listed");
        println!("{known}  {}", id_of(&model));
    }
    let Some(model) = tiny::build(&name) else {
        eprintln!("no tiny model is named {name}");
        return;
    };
    let prompt = tiny::encode(PROMPT);
    let sampling = Sampling {
        temperature_milli: 800,
        seed: [7; 32],
    };

    // Generating without proofs, to see what proving costs.
    let started = Instant::now();
    let mut session = model.session();
    let mut logits = Vec::new();
    for &t in &prompt {
        logits = session.feed(t).logits;
    }
    for step in 0..tokens {
        let token = decomp_core::inference::sample(&logits, &sampling, step);
        logits = session.feed(token).logits;
    }
    let plain = started.elapsed();

    let started = Instant::now();
    let generated =
        generate(&mut model.session(), &prompt, tokens, &sampling).expect("the call fits");
    let proved = started.elapsed();
    let proof_bytes = generated.proofs.len() * toploc::PROOF_BYTES;
    println!(
        "\n{name}: {tokens} tokens in {:.1} ms without proofs, {:.1} ms with {} proofs ({proof_bytes} bytes)",
        plain.as_secs_f64() * 1e3,
        proved.as_secs_f64() * 1e3,
        generated.proofs.len()
    );

    let started = Instant::now();
    let honest = validate(&mut model.session(), &prompt, tokens, &sampling, &generated);
    println!(
        "checked by prefill in {:.1} ms\n",
        started.elapsed().as_secs_f64() * 1e3
    );
    report("the same model", honest);

    let check = |what: &str, other: &Model| {
        report(
            what,
            validate(&mut other.session(), &prompt, tokens, &sampling, &generated),
        );
    };
    let other_seed = tiny::seeded(model.config, "a different seed");
    check("a model of the same shape, other weights", &other_seed);
    // Weights rounded to bfloat16 and to coarse steps: smaller, cheaper copies.
    check(
        "the same weights rounded to bfloat16",
        &model.map(|w| f32::from_bits(u32::from(toploc::bf16_bits(w)) << 16)),
    );
    check(
        "the same weights rounded to steps of 1/64",
        &model.map(|w| (w * 64.0).round() / 64.0),
    );
    check("every weight scaled by 1.01", &model.map(|w| w * 1.01));

    let mut changed = generated.clone();
    changed.tokens[tokens as usize / 2] ^= 1;
    report(
        "the right model, one output token changed",
        validate(&mut model.session(), &prompt, tokens, &sampling, &changed),
    );
    let mut other_prompt = prompt.clone();
    other_prompt[0] ^= 1;
    report(
        "the right model, the prompt changed",
        validate(
            &mut model.session(),
            &other_prompt,
            tokens,
            &sampling,
            &generated,
        ),
    );
    // A node that generates with a cheap model and proves with the right one.
    let cheap =
        generate(&mut other_seed.session(), &prompt, tokens, &sampling).expect("the call fits");
    let proofs = generate_proofs(&model, &prompt, &cheap.tokens);
    report(
        "text from another model, proved by the right one",
        validate(
            &mut model.session(),
            &prompt,
            tokens,
            &sampling,
            &Generated {
                tokens: cheap.tokens,
                proofs,
            },
        ),
    );
}

/// Proofs of `tokens` made by running `model` once over them, as a node that
/// did not generate them with `model` would make them.
fn generate_proofs(model: &Model, prompt: &[u32], tokens: &[u32]) -> Vec<toploc::Proof> {
    let mut session = model.session();
    let mut prompt_hidden = Vec::new();
    for &t in prompt {
        prompt_hidden.extend(session.feed(t).hidden);
    }
    let mut proofs = vec![toploc::prove(&prompt_hidden).expect("provable")];
    for chunk in tokens.chunks(toploc::CHUNK_TOKENS) {
        let mut hidden = Vec::new();
        for &t in chunk {
            hidden.extend(session.feed(t).hidden);
        }
        proofs.push(toploc::prove(&hidden).expect("provable"));
    }
    proofs
}
