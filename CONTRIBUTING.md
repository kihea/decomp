# Contributing to DeComp

## How the code is organized

- **Protocol rules live in `decomp-core` and do no I/O.** The devnet, the
  canisters, the node agent, and desktop apps all call the same functions, so
  every party applies the same rules. Time, randomness, and anything stored
  are passed in.
- **State changes by applying entries.** Accounts, the ledger, the registry,
  records, scores, storage, and relays each change only through an `apply`
  of a logged entry, so a log is a complete copy and a restart replays it.
  Open jobs and leases are deliberately not durable.
- **The wire format has two copies.** A change to a call or answer in
  `decomp-core/src/request.rs` (or the types it carries) changes
  `client-ts/src/protocol.ts` in the same commit.
- **The module interface has two sides.** The host functions a job can call
  are declared in `decomp-sandbox` and in the SDKs that compile modules
  against it. Change them together.
- **Every tunable value is a named constant** with a comment that says what
  it bounds and whether it is a starting value from the spec.
- **The spec is the source of truth.** Where code differs from
  [docs/spec.md](docs/spec.md) on purpose, say why in a comment and in
  [docs/design-notes.md](docs/design-notes.md). Keep
  [docs/status.md](docs/status.md) current in the same change that moves a
  part forward.

## Checks

Before you open a pull request:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets
```

In `client-ts`:

```bash
npx tsc --noEmit -p .
```

The same checks run in CI on every push and pull request.

## Trying a change end to end

Run a devnet, two accounts with a node each, and a third account that submits
jobs; the [README](README.md#quick-start) has the commands. `decomp-node log`,
`decomp-node receipts`, and `decomp-node draws` check the settlement log,
receipt inclusion, and every published draw from the node's side.

## Commits

Write the subject as what the change does, in the imperative ("Add records
with versioned writes"). Use the body for why, and for anything a reviewer
should check. Keep unrelated changes in separate commits.
