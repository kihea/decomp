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
| `crates/decomp-devnet` | A local network: the control plane's services in one process, behind the same calls the canisters take. |
| `crates/decomp-node` | The node agent and command-line client: create or join an account, contribute compute, submit jobs, store and send files, and check receipts and draws. Its library is shared with desktop apps. |
| `crates/decomp-sandbox` | The metered, deterministic WebAssembly sandbox jobs run in. |
| `crates/decomp-schema` | How a job's input and output types are described and checked. |
| `client-ts` | `@decomp/client`, the TypeScript client for browsers and desktop web views. |
| `docs` | The spec, status, and design notes. |

## Quick start

You need Rust 1.88 or newer with the `wasm32-unknown-unknown` target.

Start a local network:

```bash
cargo run --release -p decomp-devnet
```

It listens on `http://127.0.0.1:8787` and keeps its data in `./data`. In
another terminal, create an account for this device and keep the account key
somewhere safe:

```bash
cargo run --release -p decomp-node -- create-account --account-key account.key
```

Contribute this device's compute:

```bash
cargo run --release -p decomp-node -- start
```

From a second account, submit a job. The input is a JSON array of the job
function's arguments:

```bash
cargo run --release -p decomp-node -- --dir other create-account --account-key other.key
cargo run --release -p decomp-node -- --dir other submit module.wasm --input input.json --compute-cap 1
```

The node prints when it commits and reveals; the submitter prints the result,
how it was verified, and what it cost. `decomp-node --help` lists every
command, including `preferences`, `rates`, `bid`, `register-name`, `log`,
`receipts`, and `draws`.

### Devnet settings

| Variable | Default | Meaning |
| --- | --- | --- |
| `DECOMP_ADDR` | `127.0.0.1:8787` | Address to listen on |
| `DECOMP_DATA_DIR` | `data` | Where logs and blobs are kept |
| `DECOMP_BATCH_SECS` | `600` | How often a batch of receipts is posted |
| `DECOMP_STORAGE_CHALLENGE_SECS` | `3600` | How long a stored blob goes between challenges |
| `DECOMP_RECOVERY_WAIT_SECS` | `172800` | How long an account recovery waits before it takes effect |
| `DECOMP_REVIEWERS` | none | Account IDs, comma-separated, that may review apps and assign held names |
| `DECOMP_RESERVED_NAMES` | none | Names, comma-separated, held for their brands' owners |

The devnet's own key, printed at startup and kept in `control.key`, signs the
settlement log and the published draws, and acts as the treasury on a local
network.

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
