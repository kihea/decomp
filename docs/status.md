# Version 1 status

Version 1 is the whole system in [the spec](spec.md), with nothing deferred.
This page tracks each part of DeComp against it. **Available** means built
and running on a local devnet. **In development** means work has started.
**Not started** means nothing is built yet, and the reason is given.

Last updated 2026-11-21.

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
| Disputes, known-answer jobs, spot-check audits, holdback | Available | A dispute is settled by further nodes drawn at random, which run the job again; the first result two nodes report stands. Checkpoints are taken once per compute unit |
| Splitting, tiers, follow-up jobs | Available | |
| Node scores | Available | Reliability, availability, hardware fit (known-answer jobs and audits timed by the job board, with declared cores, memory, and GPUs), and bandwidth (timed transfers of test data between nodes). GPUs are declared once the agent runs inference |
| Always-on fallback | Available | The module runs in the control canister, on data the submitter left there |
| Inference class with TOPLOC | In development | `model.generate` and `model.embed`, TOPLOC proofs and checks (`decomp_core::toploc`), transcripts that every node after the first replays and checks, disputes, and a rerun after a rejected transcript, on the tiny approved models (`decomp-inference`). The runtime for large models on GPUs, split across devices, is next |
| Attested nodes (Intel TDX, AMD SEV-SNP) | Not started | Needs nodes on that hardware |

## Market and rewards

| Part | Status | Notes |
| --- | --- | --- |
| Credits in microcredits; posted rates with tier premiums between a floor and the fallback ceiling | Available | 250 microcredits per unit at the base rate; premiums retune every minute |
| Fee split, stake, penalties, starter grant | Available | Each node is paid 85% of its own run |
| Share caps, taper, balanced accounts first | Available | Caps and the taper apply once a tier has 100 active accounts |
| Open market: standing bids in tiers 3 and 4 | Available | Per unit or per job; bids can only go below the posted rate |
| Buying and cashing out credits | In development | Treasurer accounts record purchases and cash-outs; no payment rail is connected until the legal review is complete |
| Storage, relay, and inference rates | In development | Storage and relays are priced by starting values the spec leaves open |

## Control plane and settlement

| Part | Status | Notes |
| --- | --- | --- |
| Local devnet | Available | `decomp-devnet` runs the accounts, registry, job board, and receipts services in one process |
| ICP canisters | In development | The control canister (`canisters/control`) runs every control-plane service from the same library as the devnet (accounts, registry, job board, receipts), over the HTTP gateway with the same signed calls. Leases and audits draw on the Internet Computer's randomness, the settlement log is signed by the subnet's threshold Ed25519 key, value keys come from its vetKD key, always-on fallbacks run on an interpreter in the canister, it stops taking new jobs when its cycles run low, and state survives upgrades; job data stays on storage nodes. `decomp-replica` runs it on a local replica. `decomp-load` measures its throughput: about 6 jobs a second taken on a local replica, at about 0.4 billion cycles a job. Splitting the job board across canisters by class and tier waits until measured load calls for it |
| Batched receipts, a signed settlement log, inclusion proofs | Available | Anyone can recompute a batch fingerprint from its receipts. Each batch also fingerprints the draws published since the last, which is what proves a draw |
| Name registry | Available | Deposits, held brand names, look-alike blocking, expiry, review, and a searchable directory |

## Data

| Part | Status | Notes |
| --- | --- | --- |
| Blobs: 1 MB chunks, three replicas on nodes of different accounts, storage challenges | Available | Placement is computed from the blob's name and the storage nodes' keys |
| Repair of lost replicas | Available | A holder that fails three challenges, or stops answering, is lost; the next node in the placement order copies the blob from the others |
| Deletion by the owner | Available | `decomp-node delete`; nodes drop the blob when they next ask for work, and what is left of its fee returns |
| Records with versioned writes | Available | `write_record`, `get_record`, `list_records` |
| Relays | Available | |
| Job inputs and outputs travelling only between clients and nodes | Available | Modules, inputs, outputs, record values, and app files live on storage nodes; the control plane holds their roots. Only jobs with an always-on fallback leave their data with it too |
| Threshold keys for private values (ICP vetKeys) | Available | Value keys from the control canister's threshold key (vetKD): every device of an account gets the same key for a label, and no single party holds it. The devnet has no threshold key |

## Beyond the core

| Part | Status | Notes |
| --- | --- | --- |
| Zero-knowledge proofs for small, high-value jobs | Not started | Needs a proving system for WebAssembly |
| Compute for other chains | Not started | Needs a contract on each chain and the canisters' chain-key signing |
| Governance by token-holder vote | Not started | The token exists; votes on settings are not built |
| Tradable token | Available | Held in the control plane's ledger (`decomp_core::token`), given out by treasurers, burned into credits one for one, and withdrawn to and deposited from DFINITY's standard ICRC ledger (`canisters/token`), of which the control canister is the only minter. Listing or selling it waits on legal advice |
| Node agent for phones | Not started | Phones run only while charging on Wi-Fi |
