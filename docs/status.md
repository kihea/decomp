# Version 1 status

Version 1 is the whole system in [the spec](spec.md), with nothing deferred.
This page tracks each part of DeComp against it. **Available** means built
and running on a local devnet. **In development** means work has started.
**Not started** means nothing is built yet, and the reason is given.

Last updated 2026-11-18.

## Accounts and identity

| Part | Status | Notes |
| --- | --- | --- |
| Accounts as a key with no personal data; device keys; device linking | Available | `create_account`, `approve_device`, `revoke_device` |
| The six account fields | Available | ID, devices, balance, reputation, controllers, and when the account appeared |
| Controllers: recovery keys, a recovery service's key, guardian accounts, passkeys, wallets | Available | Recovery waits 48 hours and any device of the account can cancel it; an account can require two controllers |
| Device-scope identity per app | Not started | Apps see the account ID today |

## Nodes and jobs

| Part | Status | Notes |
| --- | --- | --- |
| Node agent and metered sandbox | Available | Deterministic WebAssembly; one compute unit is 1 billion instructions; command line on Windows, macOS, and Linux, and inside the desktop app |
| Slots | Available | A node runs as many leases at once as its owner offers |
| Work preferences | Available | Classes, tier range, rate floor, slots, hours, battery, metered networks, market-only. Battery and metered state are not yet read from the system, so devices are taken to be on mains power and an unmetered network |
| Leases drawn from public randomness | Available | drand on the devnet; ICP's verifiable randomness on canisters. Every draw is published and can be rechecked with `decomp-node draws` |
| Commit and reveal; verification that follows supply | Available | `opportunistic`, `required`, and `none`; every receipt states its level |
| Checkpoint disputes, known-answer jobs, spot-check audits, holdback | Available | Checkpoints are taken once per compute unit rather than every 10 million instructions, because each one hashes the job's whole memory |
| Splitting, tiers, follow-up jobs | Available | |
| Node scores | In development | Reliability and availability are kept; bandwidth and hardware-fit scores are not yet measured |
| Always-on fallback | Available on the devnet | On canisters the module runs in the job board canister |
| Inference class with TOPLOC | Not started | Needs the approved model list and a runtime for it, and GPU nodes |
| Attested nodes (Intel TDX, AMD SEV-SNP) | Not started | Needs nodes on that hardware |

## Market and rewards

| Part | Status | Notes |
| --- | --- | --- |
| Credits in microcredits; posted rates with tier premiums between a floor and the fallback ceiling | Available | 250 microcredits per unit at the base rate; premiums retune every minute |
| Fee split, stake, penalties, starter grant | Available | Each node is paid 85% of its own run |
| Share caps, taper, balanced accounts first | Available | Caps and the taper apply once a tier has 100 active accounts |
| Open market: standing bids in tiers 3 and 4 | Available | Per unit or per job; bids can only go below the posted rate |
| Buying and cashing out credits | In development | The treasury records purchases and cash-outs; no payment rail is connected until the legal review is complete |
| Storage, relay, and inference rates | In development | Storage and relays are priced by starting values the spec leaves open |

## Control plane and settlement

| Part | Status | Notes |
| --- | --- | --- |
| Local devnet | Available | `decomp-devnet` runs the accounts, registry, job board, and receipts services in one process |
| ICP canisters | In development | The control canister (`canisters/control`) serves accounts, sealing keys, names, and records over the HTTP gateway with the same signed calls as the devnet, and keeps its state across upgrades; `decomp-replica` runs it on a local replica. The job board and receipts canisters are next; until then the devnet serves jobs, storage, and relays |
| Batched receipts, a signed settlement log, inclusion proofs | Available | Anyone can recompute a batch fingerprint from its receipts |
| Name registry | Available | Deposits, held brand names, look-alike blocking, expiry, review, and a searchable directory |

## Data

| Part | Status | Notes |
| --- | --- | --- |
| Blobs: 1 MB chunks, three replicas on nodes of different accounts, storage challenges | Available | Placement is computed from the blob's name and the storage nodes' keys |
| Repair of lost replicas | Available | A holder that fails three challenges, or stops answering, is lost; the next node in the placement order copies the blob from the others |
| Deletion by the owner | Available | `decomp-node delete`; nodes drop the blob when they next ask for work, and what is left of its fee returns |
| Records with versioned writes | Available | `write_record`, `get_record`, `list_records` |
| Relays | Available | |
| Job inputs and outputs travelling only between clients and nodes | In development | The devnet still holds a copy of job inputs and outputs |
| Threshold keys for private values (ICP vetKeys) | Not started | Needs the canisters |

## Beyond the core

| Part | Status | Notes |
| --- | --- | --- |
| Zero-knowledge proofs for small, high-value jobs | Not started | Needs a proving system for WebAssembly |
| Compute for other chains | Not started | Needs a contract on each chain and the canisters' chain-key signing |
| Governance by token-holder vote | Not started | Waits on the token decision |
| Tradable token | Not started | Waits on legal advice |
| Node agent for phones | Not started | Phones run only while charging on Wi-Fi |
