# DeComp

DeComp, the Decentralized Compute Protocol, runs applications on compute that
their users' devices contribute, instead of on a central cloud. Devices run
jobs, store data, and relay traffic. A control plane of Internet Computer
canisters keeps the shared records: accounts, names, the job board, and
receipts. No server run by the team sits on the path of a job.

- **Jobs** are deterministic WebAssembly modules with a typed manifest, so two
  honest nodes produce the same output and results are checked by comparing
  hashes.
- **Verification follows supply.** A job picked up by a second node is
  confirmed by the match. A job picked up once is accepted on the node's
  reputation and audited at random, and every receipt states which level it got.
- **Nodes ask for work and never choose the job.** Leases are drawn from
  public randomness among the nodes asking, within each owner's work
  preferences. Accounts can also place standing bids in the larger tiers.
- **Pricing is metered**, in compute units of one billion instructions,
  never in wall-clock time. Rates are posted by formula and move with supply
  and demand between a floor and a ceiling.

The full design is in [docs/spec.md](docs/spec.md), the reasoning behind the
choices made while building it in [docs/design-notes.md](docs/design-notes.md),
and what is built today in
[docs/status.md](docs/status.md).

## Repository layout

| Path | What it is |
| --- | --- |
| `crates/decomp-core` | The protocol's rules: accounts, keys, jobs, the job board, the market, the ledger, the registry, records, storage, relays, and settlement. It does no I/O, so every host applies the same rules. |
| `canisters/control` | The control plane on the Internet Computer: every service, from `decomp-control`, answering the same calls over the HTTP gateway. |
| `crates/decomp-replica` | Runs the canisters on a local Internet Computer replica (PocketIC) behind an HTTP gateway, and upgrades them in place when rebuilt. |
| `crates/decomp-control` | The control plane's services and interface, for any host. The devnet and the canister both run it. |
| `crates/decomp-devnet` | A local network: `decomp-control` in one process, with files for storage, behind an HTTP server. |
| `crates/decomp-node` | The node agent and command-line client: create or join an account, contribute compute, submit jobs, store and send files, and check receipts and draws. Its library is shared with desktop apps. |
| `crates/decomp-sandbox` | The metered, deterministic WebAssembly sandbox jobs run in. |
| `crates/decomp-inference` | Runs the approved models on a node: generation with TOPLOC proofs, and checking another node's generation by prefill. |
| `crates/decomp-schema` | How a job's input and output types are described and checked. |
| `client-ts` | `@decomp/client`, the TypeScript client for browsers and desktop web views. |
| `docs` | The spec, status, and design notes. |

## Quick start

You need Rust 1.88 or newer with the `wasm32-unknown-unknown` target.

Start a local network:

```bash
cargo run --release -p decomp-devnet
```

It listens on `http://127.0.0.1:8787` and keeps its data in `./data`. Job
data lives on storage nodes, and a blob is kept on nodes of at least two
accounts, so start two contributing nodes, each with its own account and its
own address to hold data at. Keep each account key somewhere safe:

```bash
cargo run --release -p decomp-node -- --dir one create-account --account-key one.key
cargo run --release -p decomp-node -- --dir one start --listen 127.0.0.1:9101
```

and, in another terminal:

```bash
cargo run --release -p decomp-node -- --dir two create-account --account-key two.key
cargo run --release -p decomp-node -- --dir two start --listen 127.0.0.1:9102
```

From a second account, submit a job. The input is a JSON array of the job
function's arguments:

```bash
cargo run --release -p decomp-node -- --dir other create-account --account-key other.key
cargo run --release -p decomp-node -- --dir other submit module.wasm --input input.json --compute-cap 1
```

The submitter stores the module and input on the nodes, which fetch them, run
the job, and store its output there. The nodes print when they commit and
reveal; the submitter prints the result, how it was verified, and what it
cost. `decomp-node --help` lists every
command, including `preferences`, `rates`, `bid`, `register-name`, `log`,
`receipts`, and `draws`.

### On a local Internet Computer replica

The control canister runs on PocketIC, the Internet Computer's local
replica, which runs on Linux and macOS (on Windows, inside WSL). Start a
PocketIC server (it ships with `dfx`, in `~/.cache/dfinity/versions/<version>/`):

```bash
pocket-ic --port 47200 --ttl 86400
```

Build the canister and install it behind a gateway:

```bash
cargo build --release --target wasm32-unknown-unknown -p decomp-control-canister
cargo run -p decomp-replica
```

It installs the token's ICRC ledger beside it (`canisters/token`), with the
control canister as its only minter, and prints both canisters. Clients take
the control canister's address as the control plane's address; where `*.localhost` names do not resolve, use the gateway's form,
`http://localhost:4943?canisterId=<canister id>`. Rebuilding the canister
upgrades it in place, and its state is kept.

### Measuring throughput

`decomp-load` makes accounts, has them submit jobs at once to the control
canister on a local replica, and reports how many jobs a second it took and
settled, how long jobs took, and the cycles the canister spent on each:

```bash
DECOMP_CONTROL_URL="http://localhost:4943?canisterId=<canister id>"   cargo run --release -p decomp-replica --bin decomp-load -- module.wasm --jobs 200 --accounts 10
```

### Checking inference

`toploc` shows TOPLOC at work on the tiny approved models: it prints their IDs,
generates text with proofs, and has the proofs checked against the right model,
against models a dishonest node might use instead, and against changed text:

```bash
cargo run --release -p decomp-inference --bin toploc -- tiny-2 512
```

An inference job names the models it calls: `decomp-node submit job.wasm
--class inference --model tiny-1 ...`.

A model too large for one device is published in stages. `decomp-model pack`
cuts a Hugging Face model into a card and files, `decomp-node store` stores
each, and nodes hold parts with `decomp-node start --listen ADDR --hold
qwen3-0.6b:ends` (to run jobs with it) or `--hold qwen3-0.6b:stage-1` (to run
that stage for other nodes). `decomp-model try` runs a packed model here:

```bash
cargo run --release -p decomp-inference --bin decomp-model -- pack ./Qwen3-0.6B --name qwen3-0.6b --stages 2 --out ./qwen3-packed
cargo run --release -p decomp-inference --bin decomp-model -- try ./qwen3-packed --prompt "The capital of France is"
```

### Devnet settings

| Variable | Default | Meaning |
| --- | --- | --- |
| `DECOMP_ADDR` | `127.0.0.1:8787` | Address to listen on |
| `DECOMP_DATA_DIR` | `data` | Where logs and blobs are kept |
| `DECOMP_BATCH_SECS` | `600` | How often a batch of receipts is posted |
| `DECOMP_STORAGE_CHALLENGE_SECS` | `3600` | How long a stored blob goes between challenges |
| `DECOMP_RECOVERY_WAIT_SECS` | `172800` | How long an account recovery waits before it takes effect |
| `DECOMP_REVIEWERS` | none | Account IDs, comma-separated, that may review apps and assign held names |
| `DECOMP_TREASURERS` | none | Account IDs, comma-separated, that may act for the treasury: record credits bought, pay cash-outs, give out tokens |
| `DECOMP_RESERVED_NAMES` | none | Names, comma-separated, held for their brands' owners |

The devnet's own key, printed at startup and kept in `control.key`, signs the
settlement log. `decomp-replica` takes `DECOMP_REVIEWERS`, `DECOMP_TREASURERS`,
and `DECOMP_RESERVED_NAMES` too. An account that must be named before it
exists, such as a treasurer, is made with `decomp-node new-account-key`.

## Contributing compute

A node earns credits for the work it does. Its owner decides what kind of
work it is offered: job classes, tiers, a rate floor, slots, and working
hours. The recommended setting is to take everything the node is eligible
for at any rate. Narrow settings are picked less often, which slows the
growth of reliability and keeps the account's share cap low. The node
operator guide on [thirdspace.network](https://thirdspace.network/docs/compute/preferences-and-bidding)
covers preferences, the open market, and bidding programs in depth.

## Development

See [CONTRIBUTING.md](CONTRIBUTING.md). In short:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets
```

and, in `client-ts`, a TypeScript check with `tsc --noEmit`. Changes to the
wire format in `decomp-core` change `client-ts/src/protocol.ts` in the same
commit.
