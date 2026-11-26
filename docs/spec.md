# DeComp and Third Spaces: Version 1 Spec (Draft)

2026-11-15 · @Kihea Adams-Wilson

## Summary

DeComp, the Decentralized Compute Protocol, is the infrastructure (Sections 1 to 6). It lets applications run on compute contributed by their users' devices instead of a central cloud. Third Spaces is the application layer and offering built on it (Sections 7 to 10): the SDK and the apps made with it. This draft specifies DeComp first, then Third Spaces with three draft apps.

Nine decisions shape everything below.

| Decision | Choice in this draft | Why |
| --- | --- | --- |
| Job format | Deterministic WebAssembly module plus a typed manifest | Two honest nodes produce identical output, so results verify by comparing hashes |
| Job distribution | Exponential by size tier, replacing the 50% normal split | Most jobs fit any device; fewer, larger jobs pay more and require more trust |
| Allocation | Random, weighted by reliability and hardware fit, within each owner's work preferences; standing bids fill the open market tiers | Random assignment stops colluding nodes from verifying each other |
| Verification | A second pickup confirms a result when nodes are available; checkpoint disputes on a mismatch; TOPLOC for inference | Re-execution is cheap for small jobs; GPU output is not bit-exact and needs a different check |
| Pricing | Metered compute units, not wall-clock time | Cost is identical on every device and cannot be inflated by a slow or dishonest node |
| Settlement | Control-plane canisters track jobs; receipts are batched | A ledger write per job would cost more than a small job pays |
| Execution modes | Batch on user devices by default; always-on components run in ICP canisters | Batch is cheap and open to anyone; always-on is fast and reliable but funded by the developer |
| Privacy | Two modes per job: open (cheap and general) and private (private and general, at a higher cost) | The developer makes one clear choice and the SDK picks the method behind it |
| Accounts | A keypair with six fields and no personal data; one account identity across apps, with jobs signed by device keys | One person's devices can work together, and sign-in and recovery methods such as passkeys and email reset live outside the protocol |

<!-- diagram: job path · 6 steps, 1 decision -->

Read across, then down: a mismatch starts a dispute over the first segment where the nodes differ, and the result is then paid and recorded.

## System design

DeComp is a backend with no server. A control plane of ICP canisters keeps the shared records, a data plane of user devices does the computing, storing, and relaying, and spaces reach both through the SDK. No machine run by the Third Spaces team sits on the path of a job.

<!-- diagram: system design · 3 planes, 9 components -->

### Planes and components

| Plane | Component | Responsibility | Runs on |
| --- | --- | --- | --- |
| Control | Accounts | Account records, device lists, controllers, balances | ICP canister |
| Control | Registry | App IDs, names, manifests, module hashes, verified status | ICP canister |
| Control | Job board | Accepts jobs, splits them, grants leases, collects result commitments, runs disputes | ICP canisters, split by job class and tier |
| Control | Receipts | Batched receipts, fee splits, penalties | ICP canister |
| Data | Compute nodes | Run leased jobs in the sandbox | User devices |
| Data | Storage nodes | Hold chunks and answer storage challenges | User devices |
| Data | Relay nodes | Carry encrypted traffic between devices that cannot connect directly | User devices |
| Client | SDK | Builds manifests, signs, encrypts, moves data peer to peer, shows receipts | Inside each space |
| Client | Spaces | The apps people use | Browsers and the desktop app |

One device can fill all three data-plane roles at once.

**Two rules shape the control plane**

- **Hashes, not data.** Canisters hold records, hashes, and proofs. Job inputs, outputs, and files travel peer to peer between clients and nodes.
- **Batches, not single jobs.** A canister call can cost as much as a small job earns, so nodes claim leases and report results in batches.

### Job states

| State | Entered when | Left when |
| --- | --- | --- |
| Submitted | The client posts the manifest and the escrow | The job board splits the job and opens leases |
| Leased | A node asking for work is granted the lease at random | The node commits a result hash, or the lease expires and reopens |
| Committed | A node has posted the hash of its result | A second commitment arrives, or the confirmation window ends |
| Confirmed | A second node's commitment matches | The receipt is batched |
| Unconfirmed | The window ended with one commitment | The result is accepted on the node's reputation, and a random share is audited later |
| Disputed | Two commitments differ | The checkpoint dispute names the correct one |
| Settled | Payment and penalties are recorded | Final |
| Expired | No node finished before the deadline | The escrow is returned, or the fallback runs |

**Verification follows supply.** There is no fixed number of runs per job. A job that is picked up more than once is verified by the match, and a job picked up once is accepted unconfirmed. When nodes are scarce the network keeps working at lower assurance, and every receipt states which level the result got.

| `verify` setting | Behavior | Suits |
| --- | --- | --- |
| `opportunistic` (default) | Accepted after one result; confirmed if a second node picks it up within the window | Most jobs |
| `required` | Not accepted until a second node confirms; waits if none is free | Results that change balances or other high-value state |
| `none` | One result, and no confirmation is offered | Work the app can check for itself |

**Why a second pickup can be trusted.** Nodes ask for work and are handed a lease at random, so a node cannot choose to confirm its own job. Results are committed as hashes before they are revealed, so a second node cannot copy the first.

**Fallback without a server.** If a job expires and the app has set `fallback: always_on`, the same WebAssembly module runs in an ICP canister. That costs more and excludes GPU work and very long jobs, but it needs no machine run by the team.

### Data management

Every kind of data has one owner, one home, and a stated consistency level.

| Data | Owner | Lives in | Consistency | Kept |
| --- | --- | --- | --- | --- |
| Account records | The account | Accounts canister | Strong | For the life of the account |
| Names, manifests, module hashes | The developer | Registry canister | Strong | Until the name expires |
| Leases, commitments, disputes | The protocol | Job board canisters | Strong | Until the dispute window closes |
| Receipts | The protocol | Fingerprints in the receipts canister; details on storage nodes | Strong for fingerprints | Fingerprints permanently |
| Module code | The developer | Storage nodes, addressed by hash | Never changes | While any manifest refers to it |
| Blobs: files, media, job inputs and outputs | A user or an app | Storage nodes, as chunks addressed by hash | Never changes | For the period paid for |
| Records: app state | A user or an app | Values on storage nodes; the current-version pointer in a canister | One writer wins per version | For the period paid for |
| Private values | The user | The same stores, encrypted, with threshold keys | As above | As above |
| Local data | The user | The user's own devices | Per device, merged by version | Until the user deletes it |

**Blobs**

- **Chunking.** A blob is cut into 1 MB chunks (starting value). Each chunk is named by its hash, and the blob is named by the hash of its chunk list.
- **Placement.** Each chunk is kept on 3 storage nodes (starting value), chosen by hashing the chunk's name against the list of storage nodes. Any client can compute where a chunk lives, so there is no central index.
- **Repair.** When a storage challenge fails, a repair job copies the chunk to a new node.
- **Retention.** Storage is rented by the gigabyte-month from the owner's balance or the app's pool. Unpaid data is dropped after a grace period.
- **Deletion.** The owner signs a delete, the pointers are removed, and nodes drop the chunks. For private data, destroying the key makes every remaining copy unreadable.

**Records.** A record is a key, a version number, the hash of its value, and the writer's signature. A write names the version it read and is accepted only if that version is still current. The check runs in the canister that holds the pointer, so two devices or jobs writing the same record cannot both win; the loser re-reads and retries. Each record write is a canister call, so high-volume state should be grouped into larger records.

**Queries.** A job reads records by key or scans a key range within the shard it was given. Indexes are built by batch jobs and stored as records. The protocol has no query engine of its own.

**Optional indexers.** Anyone can copy receipts and public records into an analytical database such as ClickHouse for dashboards and history. An indexer is replaceable and never on the job path.

### Interfaces

| Between | Calls |
| --- | --- |
| Client and control plane | `submit_job`, `get_receipt`, `write_record`, `resolve_name` |
| Node and control plane | `register`, `request_lease`, `commit_result`, `reveal_result`, `answer_challenge` |
| Node and node, client and node | `put_chunk`, `get_chunk`, `relay` |

### Failure handling

| Failure | Detected by | Response |
| --- | --- | --- |
| A node disappears mid-job | Lease expiry | The lease reopens for another node |
| Too few nodes | Unclaimed leases, or no second pickup | Jobs are accepted unconfirmed; `required` jobs wait; expired jobs use the always-on fallback if it is set |
| A wrong result | Mismatched commitments, or a later audit | Checkpoint dispute; the wrong node is penalized |
| A storage node disappears | A failed storage challenge | A repair job restores the third copy |
| Two writers collide | The version check on the record pointer | One write wins and the other retries |
| A canister runs low on cycles | A balance threshold | The treasury tops it up; the canister stops taking new jobs before it runs out |
| ICP is unreachable | Failed calls | Nodes finish leased work and hold the results; new jobs queue on the client |

## 1. Network model

The network has three roles: nodes that run jobs, applications that submit jobs, and a control plane that matches them.

**Slots.** The unit of supply is one slot: a sandbox with 1 CPU core, 512 MB of memory, and a metered compute budget. A phone might offer 1 slot and a desktop 8. A job states how many slots it needs at once on one machine, so an inference job can require 10 free slots plus a GPU. A model need not fit on one device: a model too large for one is cut into stages of layers stored on the network, and the job board draws nodes holding the stages a job's node lacks, which run them for it as a pipeline.

**Node agent.** Each device runs a small agent that registers slots, pulls jobs, runs them in the sandbox, and returns results. The owner sets contribution rules in the agent: hours of the day, maximum slots, battery and metered-network limits, and which job classes to accept.

**Identity.** Each node holds a device key and signs every result with it. Accounts, devices, and app identities are specified at the end of this section.

**Node profile.** The control plane keeps four scores per node and updates each as an exponential moving average.

| Score | Measured by | Used for |
| --- | --- | --- |
| Reliability | Share of results that matched verification | Tier eligibility and payout weight |
| Availability | Share of accepted jobs finished before the deadline | Assigning deadline-sensitive jobs |
| Bandwidth | Timed transfers of dummy files | Assigning data-heavy jobs |
| Hardware fit | Declared specs, confirmed by benchmark jobs | Slot count and GPU eligibility |

A healing factor slowly pulls every score back toward the network average. Idle or recently penalized nodes then get retried instead of being starved forever.

**Control plane.** The shared records and the job board run as ICP canisters, so no operator's server is involved (see System design).

### Accounts and identity

An account is a keypair with six fields. DeComp stores no name, email, phone number, or other personal data.

| Field | Content | Purpose |
| --- | --- | --- |
| `account_id` | Hash of the account's public key | The account's identity |
| `devices` | Public keys of the devices the account has approved, each with a revoked flag | Lets one person's devices act and talk as one account |
| `balance` | Credits and any stake | Payment and rewards |
| `reputation` | Combined scores of the account's nodes | Tier eligibility |
| `controllers` | Public keys allowed to approve or revoke devices, and how many must agree: the account key, passkeys, a crypto wallet, a recovery key, guardian accounts, or a recovery service | Sign-in and recovery with no personal data in the record |
| `created` | Batch in which the account first appeared | Account age for probation rules |

**Which key does what.** The account key is the identity, and device keys do the work.

| Key | Held by | Signs |
| --- | --- | --- |
| Account key | The user, in a device's secure hardware or as a passkey | Identity in apps; approving and revoking devices; changing controllers; cash-outs and raised spending caps |
| Device key | One device; every device has its own | Job results; job submissions on the account's behalf; receipt of data sent to that device |

**Delegation.** The account key signs rarely. When a device is approved, it receives a certificate from the account key, and from then on the device key signs day-to-day actions for the account. Apps and the coordinator check the certificate, so they know which account is acting.

**Linking devices.** A new device shows a code. An approved device scans it and signs the new device's key. Any approved device can revoke another, and a passkey or any other controller can approve a device the same way.

**Device-to-device communication.** Devices on one account find each other through the account's device list. They connect directly when they can and through relay nodes when they cannot. Everything is encrypted to the receiving device's key, and data for an offline device waits in network storage.

**Application identity.** Each app has an app ID, the hash of its developer's signing key, and its manifest is signed with that key.

**User identity in apps.** By default an app identifies a user by the account ID, so one identity and one set of contacts work in every app. For any single app, a user can choose a device identity instead. Calls to always-on components carry whichever identity the user chose.

| Scope | What the app sees | Works on all the user's devices | Suits |
| --- | --- | --- | --- |
| Account (default) | The account ID, the same in every app | Yes | Social apps, shared contacts, anything with a profile |
| Device (chosen per app) | One device's key, with no link to the account | No | Send targets, shared or single-purpose machines, apps the user wants kept apart |

**Account identity in jobs.** A job can check that a device signs for an account, so an app that knows a user by the account ID accepts actions signed on any of that user's devices, and one that knows a user by a device identity accepts only that device's.

**Trade-off.** With the account scope, apps can recognize the same user across apps. For the device scope to hide the account, the list of an account's devices must not be publicly readable.

**What the control plane sees.** The accounts canister holds each account's record, including which devices belong to it, so jobs are billed to the right balance. Canister state is visible to ICP's node providers. Compute nodes never see the record, and apps see only the identity the user chose.

**Sign-in and recovery.** DeComp knows only controller keys. How a person proves they should control one is handled by Third Spaces and by apps, outside the account record.

| Method | What DeComp stores | Where the personal data lives |
| --- | --- | --- |
| Passkey | The passkey's public key | On the user's devices or with their passkey provider |
| Recovery key or phrase | A public key | With the user, offline |
| Guardian accounts | The guardians' account IDs and how many must agree | Nowhere; guardians are other accounts |
| Email reset | The public key of a recovery service | With the recovery service, run by Third Spaces or by an app |
| Crypto wallet | The wallet's public key | Nowhere; the key stays in the user's wallet |

**How email reset works.** The user registers an email with a recovery service and adds that service's key as a controller. To recover, the user proves control of the email, and the service signs the approval of a new device. The email never enters DeComp.

**Guard rails.** Every recovery method is also a way in for an attacker, so three rules apply:

- **Opt-in.** Each method is added by the user, and an account can require two methods to agree.
- **Waiting period.** A recovery takes effect after 48 hours (starting value), and any approved device can cancel it in that time.
- **Trusted services.** An account that uses a recovery service is only as safe as that service and the email behind it.

**Not stored.** Names, emails, phone numbers, contacts, location, and the list of apps an account uses.

## 2. Jobs

A job is a WebAssembly module, its inputs, and a manifest that tells the coordinator how to run and check it.

**Manifest fields**

| Field | Meaning |
| --- | --- |
| `module` | Hash of the WebAssembly code, stored once and reused across jobs |
| `inputs` / `outputs` | Typed references to data blobs |
| `class` | Menial, data, or inference |
| `slots` | Slots needed at once on one machine |
| `compute_cap` | Maximum compute units the job may burn before it is stopped |
| `deadline` | Latest acceptable finish time |
| `verify` | opportunistic (default), required, or none; see System design |
| `split` | Optional rule for cutting the job into independent pieces and merging the results |
| `max_fee` | Credits the application escrows for the job |
| privacy | open or private; a private job may also set allow_enclave (Section 7) |
| fallback | Optional: always_on, to run the job in a canister if no node finishes it in time |

**Job classes**

| Class | Examples | Typical runtime | Verified by |
| --- | --- | --- | --- |
| Menial | Hashing, signature checks, validating another node's result | Under 1 second | Re-execution |
| Data | Storing, serving, and relaying blobs; encoding images and video | Seconds to minutes | Re-execution, plus storage spot checks |
| Inference | Model generation, embeddings, ranking with a model | Seconds to minutes on a GPU | TOPLOC proof (Section 4) |

**Decomposition.** A job with a `split` rule is cut into pieces before assignment. Each piece carries only its own shard of the input, so no single node sees the whole task. Splitting also moves large jobs into the small tiers that most devices can run (Section 3).

**Lifecycle**

1. The client submits the job and escrows `max_fee`.
2. The job board splits the job if a rule exists and places each piece in a size tier.
3. Nodes ask for work, and the job board grants each piece's lease to one of them at random.
4. The node runs the piece, commits the hash of its output, and then reveals the result.
5. If a second node picks up the piece within the confirmation window, the two hashes are compared. A match confirms the result; a mismatch starts a checkpoint dispute.
6. Payouts and penalties are recorded, and the receipt joins the next batch.
7. The client receives the result, its verification level, and any unspent escrow.

## 3. Distribution

Jobs are spread across size tiers on an exponential curve: each tier holds half as many jobs as the tier below it, and each job in it pays twice as much. This replaces the earlier plan of a normal spread around a 50% bid.

**Note on the source.** The [INTELLECT-2 paper](https://arxiv.org/html/2505.07291v1) does not define an exponential distribution of jobs or rewards. It uses the word twice: for the exponential moving average behind its bandwidth estimates, and for a fraud check that expects sampled token probabilities to look exponential. The tiering here is a DeComp design choice, and it stands on its own merits.

**Tiers.** A job's size is its metered compute, and its tier is the number of doublings above the smallest job. The target share of jobs in tier t, for decay ratio r, is:

```latex
p(t) = (1 - r)\, r^{t}
```

The table shows the targets at r = 0.5. Eligibility is cumulative, so a node cleared for a tier can also take every tier below it.

| Tier | Size (compute units) | Target share of jobs | Pay per job | Who can take it | Example |
| --- | --- | --- | --- | --- | --- |
| 0 | 1 | 50% | 1x | Any node | Hash, signature check, verifying a result |
| 1 | 2 to 3 | 25% | 2x | Any node past probation | Thumbnail, one shard of a ranking job |
| 2 | 4 to 7 | 12.5% | 4x | Reliability of 0.95 or higher | Encoding one video segment |
| 3 | 8 to 15 | 6.25% | 8x | Reliability of 0.98 or higher, plus stake | Embedding a batch of posts |
| 4 and up | 16 or more | 6.25% | 16x or more | Top reliability, stake, GPU where needed | Model generation, long-running jobs |

At these targets, three of every four jobs are open to nearly every device. The better-paid jobs sit in the tail, where fewer nodes qualify and each one earns more.

**The ratio r is the main economic setting.** At r = 0.5 with pay doubling per tier, every tier pays out the same total. Below 0.5, most rewards go to the small tiers and casual devices. Above 0.5, rewards concentrate in the tail and favor strong machines.

**The protocol cannot dictate the curve.** Applications decide what jobs exist, so the curve is a target the job board steers toward with three levers:

- **Splitting.** Large jobs with a `split` rule are cut into lower-tier pieces until tier shares approach the target.
- **Pricing.** A tier with too many idle nodes gets a lower price multiplier, and a tier with a queue gets a higher one.
- **Eligibility.** Reliability and stake thresholds per tier move up or down to balance supply.

**Assignment.** Nodes ask for work; they never choose which job. The job board grants each lease at random among the eligible nodes asking, weighted by reliability x availability x fit. A second lease on the same piece never goes to the same account. Accounts that also spend credits in spaces are served first, and no account is granted more than its share cap (Section 5).

**Work preferences.** A node's owner decides what kind of work the node is offered: which job classes, which tiers among those it is eligible for, the lowest posted rate it works for, how many slots it offers, and when it works (hours of the day, on battery, on metered networks). Preferences describe kinds of work, never a job, a module, or an app, so a node still cannot choose which job it confirms. The node agent starts from the recommended setting, everything the node is eligible for at any rate, so most owners never change it.

**Open market.** Jobs in tier 3 and above are also listed for bids. An account can take its nodes out of standard allocation in those tiers and take work there only through standing bids: a job class, a tier range, a number of jobs, and a price per unit or per job, until a time it sets. Bids can only go below the posted rate; a bid above it waits until the rate rises to meet it. The lowest matching bid fills a job's first pickup and is paid what it asked, and the job's payer keeps the difference. The confirming node is still drawn at random from standard allocation, and share caps and the taper apply to bidders as to everyone. Bids can be placed by hand or by programs, so an operator who knows its own costs can bid just above them to win volume, or bid only when busy hours raise the rate.

**Narrow preferences cost.** Reliability rises only with confirmed results, share caps grow with reliability and age, and higher tiers open as reliability grows. A node set to take only large jobs is picked less often, earns its reliability more slowly, keeps a low cap, and misses the credits that small jobs pay in slow periods. Owners who narrow their preferences should keep the small tiers open while a node is building its record.

## 4. Verification

Verification follows supply: a result is confirmed when a second node picks up the same job, and audited later when none does. Five checks cover the three job classes; the percentages marked as starting values are proposals to tune in testing.

| Check | Applies to | How it works | Added cost |
| --- | --- | --- | --- |
| Second pickup | Menial and data jobs | A second node that picks up the same job commits its result, and the two hashes are compared | One extra run, only when a second node is free |
| Spot check | Unconfirmed menial and data results | The piece runs once; for a random share of pieces, a second node re-runs one random segment | About 10% (starting value) |
| Known-answer jobs | All classes | The job board slips in jobs whose result is already known | 1 to 2% (starting value) |
| TOPLOC proof | Inference jobs | The node commits to hashes of the model's internal states; a validator recomputes and compares them | About 1% slower generation, per the INTELLECT-2 paper |
| Storage challenge | Stored blobs | The node must return the hash of a random byte range plus a fresh random value | Negligible |

**Checkpoint disputes.** While it runs a piece, each node records a hash of the program's state every 10 million instructions (starting value) and submits that list with its result. When two results disagree, the job board finds the first checkpoint where the lists differ and re-runs only that segment from the last agreed state. The node whose next checkpoint matches is right and the other is penalized (Section 5), so one honest node is enough and no majority is needed.

**What disputes cannot catch.** Two colluding nodes that return the same wrong result raise no dispute. Spot checks and known-answer jobs still cover that case.

**Segment sampling.** Checkpoints also make spot checks cheaper. The checker re-runs one random segment of a large job, not the whole job, so more jobs can be checked for the same cost.

**Probation.** New nodes start at tier 0, and their results are paid only once a second pickup confirms them. As reliability rises, a node's unconfirmed results are accepted and audited by spot check. It never learns which jobs are checked, so honesty on every job is the only safe strategy.

**Deterrence rule.** The spot-check rate times the penalty must exceed the reward for one job. Cheating then loses credits on average, even when most jobs go unchecked.

**Inference.** GPU output differs slightly between machines, so hashes of the output cannot be compared directly. TOPLOC tolerates that variation and still detects a swapped or shrunken model. The validator needs its own copy of the model and a GPU, so validators are tier 4 nodes.

**Independence.** A second pickup only counts if the two nodes are independent. Four rules protect that: random assignment, different accounts per piece, probation for new nodes, and stake for tier 3 and above.

**Public randomness.** The random draws for assignment and spot checks come from a public source, such as the drand beacon or ICP's verifiable randomness. Each draw is published, so anyone can recheck that nodes were not hand-picked.

**Determinism.** WebAssembly is deterministic except for a few floating-point edge cases and threads. The sandbox fixes the first and disallows the second.

**Attested nodes (optional).** A node can run its agent inside a hardware enclave (Intel TDX or AMD SEV-SNP) and prove it at check-in with a report signed by the chip. Private jobs that allow the enclave route (Section 7) go only to these nodes. Their inputs are encrypted and are opened only inside the enclave, with a fresh key for each job.

**What attestation is not.** Attestation is treated as extra privacy, never as proof of correct work. The [TEE.Fail research](https://thehackernews.com/2025/10/new-teefail-side-channel-attack.html) showed that hardware costing under $1,000 can extract attestation keys from fully updated Intel and AMD machines, given physical access. Node operators have physical access, and both vendors class such attacks as out of scope.

Five rules follow from that:

- **Same checks.** Attested nodes get the same confirmations, spot checks, and disputes as any other node.
- **Split trust.** Where the computation can run on shares, each share goes to an enclave on a different account, and on a different chip vendor where possible. Reading the data then means physically breaking several randomly assigned machines. Other jobs on this route run in one enclave, and the app accepts that risk.
- **Small exposure.** Each node sees one shard, keys last for one job, and nodes rotate between jobs.
- **Fresh attestation.** Reports expire quickly, outdated firmware is refused, and a chip model can be suspended when a new attack is published.
- **Honest labeling.** The SDK labels the enclave route as hard to read, not impossible, and uses it only when the app opts in.

**In version 1: zero-knowledge proofs.** A single node can run a job and produce a proof that anyone checks in milliseconds. Proving costs far more than running the job, so this suits small, high-value jobs such as balance updates.

## 5. Rewards

Every account holds a credit balance: it earns credits when its nodes run jobs and spends them when its activity creates jobs. All numbers in this section are starting values.

**Who pays.** A user action in an app creates jobs, and those jobs are charged to the user's balance by default. An application can instead sponsor jobs from its own credit pool, which is how a developer fronts the cost while the user base is small.

**Contribution required for use.** An app can require its users to contribute compute. A job the app's manifest covers then runs for a user only if that user's account earned, over the last 30 days, at least a set share of what it spent. This suits apps that serve inference, where use that nobody supplies could otherwise run up empty demand. The app can exempt chosen models, so cheap models stay open to everyone while costly ones need contribution. An app should require it only where the economics call for it: the default AI chat space serves every model it can afford to without the requirement, and keeps it only for the models that need it.

**Three user profiles.** The compute, split, and siphon groups from the original draft become outcomes of the balance, not roles a user picks.

| Profile | Earlier name | Balance pattern | How it settles |
| --- | --- | --- | --- |
| Provider | Compute user | Earns more than it spends | Holds credits or cashes out |
| Balanced | Split-compute user | Earns about what it spends | Nothing to settle; the app feels free |
| Consumer | Siphon user | Spends more than it earns | Buys credits, or the app sponsors the gap |

**Units.** Balances and fees are whole numbers of microcredits, and one credit is 1,000,000 microcredits. The credit is not pegged to any currency. It is defined by compute: at the base rate, one credit buys 4,000 compute units, about one hour of one slot on a reference core.

**Price of a job.** Each job type has its own meter and a posted rate. Rates not yet listed are to be set from testing.

| Job type | Meter | Base rate (starting value) |
| --- | --- | --- |
| Compute: menial and data jobs | Compute units; one unit is 1 billion WebAssembly instructions | 250 microcredits per unit |
| Storage | Gigabyte-months held | Not yet set |
| Relay and serving | Gigabytes moved | Not yet set |
| Inference | Tokens processed, by model | Not yet set |

```latex
\text{fee} = \text{metered quantity} \times \text{rate} \times \text{tier premium} \times \text{runs}
```

Runs is 1 for an unconfirmed result and 2 when a second node confirms it. The tier premium starts at 1.0 for every tier and moves with supply and demand in that tier (Section 3). Rates move with network load between a floor and a ceiling, and the ceiling is the cost of the always-on fallback.

**Fee split**

| Share | Goes to | Purpose |
| --- | --- | --- |
| 85% | Nodes whose results matched, split equally | Payment for the work |
| 10% | Verification fund | Pays spot checkers, inference validators, and known-answer jobs |
| 5% | Protocol treasury | Funds starter grants and coordinator costs |

Example: a 6-unit compute job at 250 microcredits per unit, premium 1.0, confirmed by a second node, costs 3,000 microcredits. The two nodes receive 1,275 each, the verification fund 300, and the treasury 150.

**Starter grant.** Each new account receives 25 credits so a new user can use apps at once. Granted credits can be spent but not transferred or cashed out, which limits the payoff from farming accounts.

**Penalties**

| Event | Consequence |
| --- | --- |
| Missed deadline | No payment; availability score drops |
| Wrong result | No payment; reliability drops sharply; staked nodes pay the cost of the dispute |
| Repeated wrong results or a failed known-answer job | Return to probation; part of the stake is forfeited |
| Proven fraud, such as running a swapped model | Removal from the network; full stake forfeited |

**Stake and holdback.** Nodes in tier 3 and above post stake sized to the largest job they can take. Nodes on spot checks have 10% of earnings held for 24 hours so a late check can still be enforced.

**Cash-out.** Credits can be bought and cashed out for money, with stablecoins as the first route (Section 6). The legal questions this raises must be settled before launch (see Open questions).

### Limits on provider leverage

Providers are paid for what the network is short of, and no provider can grow large enough to steer prices or supply. Five rules do this; all numbers are starting values.

| Rule | How it works | What it prevents |
| --- | --- | --- |
| Price takers | The protocol posts each rate by formula. Providers can bid below it, never above | Providers raising prices together |
| Fallback ceiling | No rate rises above the cost of running the same job in a canister | Providers holding apps hostage when supply is short |
| Share caps | No account is granted more than its cap of the leases in a tier | One operator controlling a tier |
| Balanced users first | Lease requests from accounts that also spend credits in spaces are served first; pure providers fill what remains | Dependence on professional operators |
| No voting by compute | Providing compute gives no say over fees, rates, or rules | Providers rewriting the economy in their favor |

**Share caps and reliability.** A new account's cap is 0.1% of a tier's leases. The cap grows with the account's reliability score and age, up to a ceiling of 2% that no account passes. Reliability raises the cap toward the ceiling and opens higher tiers; it never lifts the ceiling itself.

**Taper.** Above half its cap, an account's pay per unit falls steadily, reaching 70% of the posted rate at the cap.

**Small networks.** Caps and the taper apply only once a tier has 100 active accounts. Below that, every available node is needed.

**Ways to earn more.** Higher earnings come from supplying what is scarce, not from volume.

| Avenue | How it pays |
| --- | --- |
| Reliability | Access to higher tiers, and a cap closer to the ceiling |
| GPUs | Inference jobs, the best-paid class |
| Storage and bandwidth | Storage and relay jobs, paid by the gigabyte |
| Scarce hours | Rates rise when a tier is busy, so being online then pays more |
| Confirming | A share of the fee for second pickups and audits |

**Weak spot.** A large operator can split into many accounts to stay under the caps. Stake and probation apply to each account, which makes splitting costly but not impossible.

### Economics: starting assumptions

At these assumptions a CPU node earns a few dollars a month, so for most users the reward is free app usage, not income. Every value is an estimate to replace with measurements from testing.

| Assumption | Starting value | Basis |
| --- | --- | --- |
| Cloud benchmark | 3.6¢ per core-hour on demand; 1.5¢ at the lowest spot price | [AWS c7g.large](https://www.doit.com/compute/spot/us-east-1/c7g.large): USD 0.0725 an hour on demand and USD 0.0307 spot, for 2 virtual cores |
| Price to apps | 1.5¢ per useful slot-hour | An assumption, not a protocol value: the cash price one credit would need for apps to pay the same as cloud spot |
| Redundancy | 2 runs when a second node confirms; about 1.1 when a result is accepted unconfirmed and audited | Section 4 |
| Fees | 15% of each fee | Fee split above |
| Node power draw | 10 watts per busy core | Estimate for a desktop processor |
| Electricity | 18¢ per kilowatt-hour | [US residential average for 2026](https://www.utilitydive.com/news/customers-should-not-expect-electric-bill-relief-in-2026-the-cake-is-bake/810450/) |
| Node schedule | 4 slots, 8 hours a day, half busy: 480 slot-hours a month | Example desktop |
| App usage per user | 29¢ of compute a month | Snap's infrastructure cost of [USD 0.86 per daily user per quarter](https://www.fool.com/earnings/call-transcripts/2026/02/04/snap-snap-q4-2025-earnings-call-transcript/) |

**Results**

| Result | Calculation | Value |
| --- | --- | --- |
| Node pay per slot-hour, confirmed job | 1.5¢ x 85% / 2 | 0.64¢ |
| Node pay per slot-hour, unconfirmed job | 1.5¢ x 85% / 1.1 | 1.16¢ |
| Electricity per slot-hour | 0.01 kWh x 18¢ | 0.18¢ |
| Monthly gross, example desktop | 480 x node pay | USD 3.06 to 5.56 |
| Monthly electricity, example desktop | 480 x 0.18¢ | USD 0.86 |
| Treasury income per active user per month | 29¢ x 5% | 1.45¢ |
| Treasury income per million daily users | 1,000,000 x 1.45¢ | USD 14,500 a month |

**What the numbers mean**

- **CPU nodes** earn pocket change. The offer to most users is that apps cost them nothing.
- **GPU nodes** are where income is real. An RTX 4090 rents for a [median of USD 0.49 an hour](https://getdeploying.com/reference/cloud-gpu/nvidia-rtx-4090), many times what a CPU slot earns.
- **Rented GPUs fill the gap at the start.** Until enough users offer GPUs, GPU nodes can run on rented machines. The node agent ships as a container image, with templates for GPU rental markets such as Vast.ai, and a rented machine runs as any other node of the account that rents it.
- **Third Spaces runs its own fleet at the start.** It contributes compute as any account can, on machines rented from Vast.ai and on EC2 instances (storage optimized, accelerated computing, and general purpose). The EC2 instances that offer AMD SEV-SNP (M6a, C6a, and R6a) are its first attested nodes, which the enclave route of private mode needs.
- **App prices can fall.** When most results are accepted unconfirmed, the same node pay supports a price near 0.8¢ per slot-hour, about half of cloud spot.

**What Third Spaces earns.** While one team runs both layers, the 5% treasury share is the base income, and at these values it is small. Four levers raise it:

- Apps that use inference, which spend far more compute per user.
- A margin on always-on capacity and on private compute.
- Credit sales to users who consume more than they contribute.
- A higher fee once the network has proven demand.

**Funding without dollars**

| Option | What it provides | Main cost or risk |
| --- | --- | --- |
| Internal credits (already in the design) | Early node supply at no dollar cost, because nodes are paid in credits redeemable for compute | Covers supply only; development, ICP cycles, and cash-outs still need money |
| A tradable token | Pays early nodes and contributors, and can raise capital | Price swings; rewards without real demand are inflation; US rules on token sales are unsettled |
| Stable credits plus a floating token | App prices stay stable while the token floats; tokens are destroyed to create credits | More moving parts; the same legal questions |
| Launch through [ICP's SNS](https://docs.internetcomputer.org/guides/governance/launching.md) | Raises ICP from participants in exchange for governance tokens | Irreversible; upgrades and treasury spending then need token-holder votes; requires a working, audited, open-source app |

**Chosen: stable credits plus a floating token.** The token lives in DeComp's ledger beside credits, where it moves between accounts as one more ledger entry, and is burned to make credits that can be spent but not cashed out. For trading, a copy of it sits on an ICRC ledger (ICRC-1, ICRC-2, ICRC-3), the standard Internet Computer exchanges list, and the control plane is that ledger's only minter: withdrawing tokens mints copies, and depositing copies burns them and returns the tokens. Its whole supply is created in the treasury at the start, and the treasury gives it out to early nodes, contributors, and buyers.

This section is not legal or financial advice. Any token sale needs a securities lawyer before it is designed.

## 6. Settlement and the ledger

The ledger records balances and batched proof of the work done; it never sees individual jobs.

**Receipts.** Each finished piece produces a receipt: job ID, module hash, input and output hashes, the signatures of the nodes that ran it, units burned, and the fee split.

**Batches.** Every 10 minutes (starting value) the receipts canister combines the new receipts into one fingerprint and posts it with the net balance change per account. Any node can check that its own receipts are included.

| Data | Where it lives | Why |
| --- | --- | --- |
| Balances, stakes, forfeits, application credit pools | Ledger | Shared record that every party must trust |
| Batch fingerprints | Ledger | Proves which work was paid for without storing it |
| Job code, inputs, outputs | Blob storage, addressed by hash | Too large and too frequent for a ledger |
| Assignment and result messages | Job board canisters | Needed for seconds, not kept |

**Ledger.** Balances and batch fingerprints live in ICP canisters. Building a new chain is out of scope.

**No coordinator server.** The control plane runs as ICP canisters, so its code and state are replicated across a subnet and no single operator can alter them. Three properties follow:

1. Canister code is public, and every upgrade is a recorded proposal.
2. Lease grants draw on ICP's verifiable randomness, so jobs cannot be steered to friendly nodes.
3. Anyone can recompute a batch fingerprint from its receipts.

During development and testing, a local replica of the canisters stands in for the network.

### Web3 integration

Web3 is plumbing behind the SDK. It adds ways to sign in, settle, pay, and reach other chains, and no user needs a wallet or a token to use an app.

| Where | What web3 adds | Layer | In version 1 |
| --- | --- | --- | --- |
| Sign-in | A crypto wallet as one more controller key, beside passkeys and email reset | Third Spaces | Yes |
| Settlement | Balances and batch receipts in ICP canisters | DeComp | Yes |
| Money in and out | Stablecoins as the first route for buying credits and cashing out | Third Spaces | Yes |
| Assets on other chains | Always-on components sign transactions for chains such as Bitcoin and Ethereum, using [ICP's chain-key signatures](https://docs.internetcomputer.org/concepts/chain-key-cryptography/) | Third Spaces | Yes |
| Compute for other chains | Smart contracts on any chain send heavy jobs to DeComp and receive a result with its receipt | DeComp | No |
| Governance | Token-holder votes on settings such as the decay ratio and the fee split | DeComp | No |

**Compute for other chains.** Smart contracts can run only small computations. A contract could pay DeComp to run a large job and accept the result after a challenge window, during which any node can open a checkpoint dispute (Section 4). This gives nodes a second source of demand beyond Third Spaces apps. For small, high-value jobs, a zero-knowledge proof removes the wait.

**Three rules**

- **No wallet or token required.** Ordinary users sign in with a passkey and pay in credits. Web3 stays behind the SDK, as ICP cycles do.
- **No ledger write per job.** Receipts stay batched.
- **No public balance under a user's identity.** Apps see the account ID. On a public ledger, a balance sits under a separate ledger address that only the account's controllers can link to that ID.

A tradable token is a separate decision, covered under "Funding without dollars" in Section 5.

## 7. Third Spaces: the application layer

An application is a client, a set of job modules, and a typed state schema; the developer writes all three with the Third Spaces SDK and runs no servers.

| Part | What it is | Runs on |
| --- | --- | --- |
| Client | Web, mobile, or command-line interface | The user's device |
| Modules | Typed functions compiled to WebAssembly, written in Rust first | Network nodes, or the user's own device for private work |
| State | Typed records and blobs | Network storage |
| App manifest | Modules, schemas, permissions, and sponsorship policy | Registered with the coordinator |

**Standard instruction set.** A module can call only the host functions below. Clock, network, file system, and unseeded randomness are unavailable, which keeps every job deterministic.

| Group | Calls | Notes |
| --- | --- | --- |
| Input and output | `input.read`, `output.write` | Types declared in the manifest |
| State | `kv.get`, `kv.put`, `kv.scan` | Limited to the shard handed to the job |
| Blobs | `blob.get`, `blob.put` | Addressed by content hash |
| Cryptography | `hash`, `sig.verify`, `seal`, `open` | Sealing encrypts to a user's key |
| Models | `model.embed`, `model.generate` | Approved models only, identified by hash; inference class |
| Jobs | `job.spawn` | Creates follow-up jobs inside the parent's fee cap |
| Randomness | `rand.seeded` | Seeded from the job ID, so every node draws the same values |

**Standard modules.** Common tasks ship as audited modules with known costs, so most apps start without writing WebAssembly: signature and result checks, thumbnailing, video segment encoding, blob storage and serving, message relay, batch embedding, and feed ranking over a shard.

**Typing.** Input and output types are checked when a job is submitted, and a mismatch is rejected before any node runs it. There is no custom language. Any language that compiles to WebAssembly and can import the host functions can write a job: Rust through the SDK, which declares types for the check, and C and C++ through a header, built freestanding or with Emscripten or wasi-sdk, as long as the module imports nothing besides the host functions.

```rust
// Illustrative only; the SDK surface is not final.
#[third_spaces::job(privacy = "open", class = "data", split = "by_chunk")]
fn rank_shard(posts: Vec<PostFeatures>, weights: RankWeights) -> Vec<ScoredPost> {
    posts.into_iter().map(|p| score(p, &weights)).collect()
}

#[third_spaces::job(privacy = "private")]
fn update_profile(profile: Private<Profile>, watched: Private<VideoFeatures>) -> Private<Profile> {
    profile.blend(&watched, 0.1)
}
```

**Privacy modes: open and private.** Every job runs in one of two modes, and the developer picks per job. Open is cheap and general. Private is private and general, and it costs more. A value marked private cannot be passed to an open job; the compiler rejects it.

| Aspect | Open | Private |
| --- | --- | --- |
| Promise | Correct results at the lowest cost | No node ever sees the data unencrypted |
| Who can read the data | The nodes that run the job | The user, and anyone the user shares a key with |
| Code it runs | Any WebAssembly module | Any computation written with the SDK's private types |
| Cost | The base price, at double redundancy | Free on the user's device, about 2x for totals, 100 to 100,000x for encrypted compute |
| Use it for | Public data and heavy work | Personal data, history, messages, anything tied to one user |

**How private mode runs a job.** The developer writes one private function. The SDK takes the first route that fits and reports which one it used.

| Order | Route | When the SDK takes it | Cost against open | Verified by |
| --- | --- | --- | --- | --- |
| 1 | Local | The job fits the user's device or another device on the same account | Free | Not needed; the device belongs to the user |
| 2 | Private totals | The job only adds, counts, averages, or applies fixed weights | About 2x | Duplicate runs on each share |
| 3 | Encrypted compute | Any other job, up to a size cap | [100 to 100,000x](https://arxiv.org/html/2602.22229) | Duplicate runs on the encrypted values |
| 4 | None | The job is over the size cap | Not run; the SDK returns an error at submit time | Not applicable |

**What each route uses.** Local runs the module on the user's device with no verification. Private totals splits each input into shares held by nodes on different accounts. Encrypted compute uses homomorphic encryption, so a node computes on values it cannot read.

**Limits of "general".** Encrypted compute handles any calculation in principle. The code must use the SDK's private number and true-or-false types, loops of fixed length, and no branching on a secret value.

**Enclave opt-in.** A private job over the cap can set `allow_enclave`. It then runs inside an attested enclave at close to open cost. This weakens the promise: a node owner with physical access may read the data (Section 4). The SDK never takes this route unless the app opts in, and the receipt says when it did.

**Storage and transport.** Private values are stored encrypted, with keys that no single party holds (ICP vetKeys). Always-on components can store private values but cannot compute on them. In both modes, jobs reach nodes through relays under one-time IDs, so a node cannot tell whose job it is running.

**Verification is unchanged.** Shares and encrypted values are ordinary bytes to a job, so duplicate runs and checkpoint disputes work in both modes.

**The third combination.** Cheap and private exists only as the private-totals route. It is not general, so it is a route inside private mode and not a mode of its own.

**Speed.** Network jobs finish in seconds to minutes. Anything a user waits on is served from results computed ahead of time and kept in storage, or by an always-on component (see below).

### Execution modes: batch and always-on

Each component of an application runs in one of two modes: batch on user devices, or always-on in an Internet Computer (ICP) canister. Batch is the default and needs no ICP account, tokens, or approval.

| Aspect | Batch | Always-on |
| --- | --- | --- |
| Runs on | User devices, through the DeComp protocol | An ICP canister, copied to every node of a subnet (13 nodes for most apps) |
| Paid by | Users' credits, offset by the compute they contribute | The developer, billed through the SDK, which handles ICP cycles |
| Cost per unit of work | Lowest: a job runs twice | Higher: every write runs on all 13 or more nodes |
| Reliability | Varies with which devices are online | High: vetted machines with specified hardware |
| Speed | Seconds to minutes | Reads under 100 ms; writes final in 1 to 2 seconds |
| State | None between jobs; reads and writes network storage | Keeps its own state between calls |
| Barrier to entry | None: a developer can launch with no funds | Must fund the component; the SDK buys cycles and keeps the canister topped up |
| Best for | Encoding, ranking, embedding, verification, simulation ticks | Accounts, inboxes, indexes, timers, anything a user waits on |

ICP figures are from the [ICP network overview](https://docs.internetcomputer.org/concepts/network-overview/).

**Mix modes inside one app.** The choice is per component, not per app. Most apps will keep a small always-on core that holds state and answers requests, and push the heavy work to batch jobs.

**Where the extra cost comes from.** A canister pays for the compute, storage, and bandwidth it uses, so an idle one costs mainly its storage. Always-on is dearer because each write runs on every node of the subnet, and because the developer pays instead of users.

**Billing through the SDK.** Developers never touch ICP tooling or cycles. The SDK is a framework over two runtimes, much as Next.js is over React: it deploys the canister, estimates its cost, watches its balance, and tops it up. Developers get one bill, in credits.

**Single bill.** The developer pays only in credits. A Third Spaces treasury holds a cycle reserve, tops up app canisters, and charges each app's credit pool at cost plus a small margin.

Cycles are bought with real money, so the single bill depends on credits being purchasable (Section 5). The treasury is a reseller of ICP capacity, which brings custody and price risk.

**The bridge.** A component declares `mode: batch` or `mode: always_on` in the app manifest. An always-on component submits batch jobs to the job board and collects results with their receipts. Canisters can make outbound HTTPS requests and run timers, which covers both steps.

**Trust at the boundary.** A batch result carries batch-level assurance: at best two matching nodes, not subnet consensus. An always-on component should read the receipt's verification level, and set `verify: required` for results that change balances or other high-value state.

**In the draft apps**

- **Feed.** Always-on holds accounts, the upload inbox, and the pointer to the latest topic lists. Batch does encoding, describing, counting, and list building.
- **Game.** Always-on holds the tick clock, the command inbox, and the accepted world hash for each tick. Batch runs the zone ticks.

### What the SDK exposes and what it handles

The developer decides what the app does and how private and durable each part must be. The SDK decides how it runs, and reports what it did on every result.

| Area | The developer chooses or sees | The SDK handles |
| --- | --- | --- |
| Compute | Privacy mode (`open` or `private`), job class, deadline, fee cap | Splitting, tiers, node assignment, redundancy, retries |
| Privacy | Which values are marked private; whether to allow the enclave route | Route choice, key creation, share splitting, encryption, relays, attestation checks |
| Execution | `batch` or `always_on` for each component | Canister deployment, cycle purchase and top-up, the bridge between the two |
| State | Typed schemas for records and blobs | Replication, storage challenges, encryption of private values at rest |
| Billing | A cost estimate before each job, spending caps, sponsorship rules, one bill | Metering, escrow, fee split, conversion of credits to cycles |
| Identity | The user's account ID, or a device identity if the user chose one; signed user actions | Key storage, device certificates, sign-in and recovery flows, one-time job IDs |
| Verification | An optional `verify` override | Checkpoints, spot checks, disputes, known-answer jobs, penalties |
| AI fingerprinting | Whether a piece of content came from an approved model run on the network, and how likely text or media is to be AI-made | Reading the content's receipt and model transcript; running the detection model as an inference job |
| Display | The tile's background, artwork, typeface, size, tagline, and indicators; the whole screen once the space is open | Rendering tiles on the home page, enforcing the fixed tile rules, and the home button |

**What every result reports.** Each result carries a receipt with five fields: the privacy route used, how the result was verified, what it cost, how long it took, and whether an enclave was involved.

**What the SDK always surfaces.** Three things are never handled silently:

- A job whose estimated cost exceeds the app's cap is quoted first, not run.
- A weaker privacy route is used only on opt-in and is marked on the receipt.
- A result that is unverified or under dispute is flagged as such.

**What the SDK never exposes.** Node identities and locations, other apps' jobs and data, raw keys, and ICP cycles stay behind the SDK.

**Errors.** Problems are typed, and most are caught before a job runs.

| Error | Raised | Meaning |
| --- | --- | --- |
| `PrivateInOpenJob` | When the code is compiled | A private value was passed to an open job |
| `TypeMismatch` | At submit time | Inputs or outputs do not match the manifest |
| `NoPrivateRoute` | At submit time | The private job is over the size cap and the enclave route is not allowed |
| `OverFeeCap` | At submit time | The estimate exceeds the job's or the app's cap |
| `InsufficientCredits` | At submit time | The payer's balance cannot cover the escrow |
| `DeadlineMissed` | At run time | No verified result arrived in time; the escrow is returned |
| `DisputePending` | At run time | Nodes disagreed and the dispute is still being resolved |

### Navigation and names

The home page puts the user's spaces first, and each one lives at its own name under one domain. Third Spaces is designed for web and desktop first, with a compact layout for phones. In the interface an app is called a space. Balances, devices, and settings sit one tap away, so a new user can open a space without learning about any of them.

**Domains.** Availability of these domains has not been checked.

| Domain | Role |
| --- | --- |
| `tothird.space` | The home page and every app, as `name.tothird.space` |
| `thirdspace.network` | Documentation, developer portal, and network status |
| `gothird.space` | Redirects to the home page |

Apps are served under one domain only. The same app on two domains would be two separate browser origins, with separate storage and sign-in, and would make phishing easier.

**Home page, signed in.** See the [mockups of the three tabs](https://claude.ai/artifact/2B3CQ8cfBfFWjDFo1Hfv8q).

| Area | What it does |
| --- | --- |
| Your spaces | Large tiles, one per space the user has opened or pinned, each designed by the space itself. This takes most of the screen |
| Add a space | The last tile opens the directory |
| Find a space | A typed name opens that space at `name.tothird.space`; any other text searches the directory |
| Discover | A tab of its own, reached from the tab bar or the Add a space tile |
| Tabs | Spaces, Discover, You |

**What stays off the home page.** Credit balance, devices, contribution settings, sign-in methods, and recovery all live under You.

**The other tabs**

| Tab | Contents |
| --- | --- |
| Discover | Search, category filters, one featured space, and lists of new and popular verified spaces, each with an Add button |
| You | Credit balance with credits earned and spent; each device with a contribution switch; and account settings for sign-in and recovery, space permissions, and spending caps |

**Discover lists verified spaces only.** A space is reviewed before it is listed, as in an app store, and it can be delisted. Any registered space still opens at its own name. Third Spaces does not moderate what happens inside a space; each space answers for its own content and users.

**Web and desktop first.** The main layout is a web page, not a stretched phone screen. The phone layout described above is the compact version of it.

| Element | Web and desktop | Phone |
| --- | --- | --- |
| Navigation | A top bar with Spaces, Discover, Developers, search, device status, and account | A tab bar with Spaces, Discover, and You |
| A space | A card with its address, a live content pane, and an Open button | A tile with indicators |
| Search | In the top bar, with a keyboard shortcut | A field at the top of the Spaces tab |
| Device status | One line in the top bar, since this is usually the device that contributes | Under You |
| More spaces | A row of directory cards below the user's own, each with its address | The Discover tab |
| Footer | Links to documentation, network status, and the desktop app | None |

**Content panes.** On web and desktop a space fills a live pane inside its card: World shows its map, Send shows a drop zone for files, and Feed shows a row of previews.

**How a space presents itself.** Each space designs its own tile on the home page, through a `display` block in its manifest. Once opened, a space draws its entire screen; the only fixed element is the home button.

| Tile setting | Options |
| --- | --- |
| Background | A color, a pattern, or an image |
| Icon or artwork | The space's own mark, or custom art that fills the tile |
| Typeface and colors | The space's own, for its name and text |
| Size | Featured or compact on web and desktop; standard or wide on phones. The shell picks from the sizes a space supports |
| Tagline | One line on what the space is for |
| Indicators | Up to two on a standard tile and three on a wide or featured one, chosen from the set below |
| Content pane | On web and desktop, a live area inside the card that the space fills, such as a map, a drop zone, or a row of previews |

**Indicators.** A space sends indicator values through the SDK and the shell draws them, so they look and behave the same on every tile.

| Indicator | Shows | Example |
| --- | --- | --- |
| Count badge | How many items are new or waiting | 8 new |
| Progress bar | How far along a task is | A transfer at 2 of 5 files |
| Status pill | One or two words on what needs the user | Your turn |
| Countdown | Time left until something happens | 2h 14m left to move |
| Presence | How many people are active | 3 allies online |
| Preview thumbnails | Up to three small images of new content | New videos |
| Live line | One short line of text | Ranked on your phone |

**What the shell keeps fixed.** Four things are the same on every tile so users are not misled:

- The space's registered name is always shown.
- Text must meet a minimum contrast against the tile's background.
- Indicators come from the fixed set and are drawn by the shell; a space cannot draw its own badges.
- A tile cannot imitate system controls or another space.

A signed-out visitor sees a short explanation, the directory, sign-in, and the download.

**Moving between apps**

- **Own origin.** Each app is its own subdomain, so browsers keep its storage apart from every other app's.
- **Getting back.** On the web, the SDK supplies a standard home button. In the desktop and mobile apps, the shell draws a bar around every app.
- **First open.** A permission screen shows which identity the app will see, its spending cap, and the private data it asks for.
- **One sign-in.** The user signs in at a central page, which hands each app a delegation, the same mechanism as device certificates (Section 1).

**How names resolve**

- **Registry.** A registry maps each name to an app ID and to the app's client files.
- **Gateway.** A gateway on `*.tothird.space` reads the registry and serves the app to browsers. The desktop and mobile apps read the registry directly.
- **Public Suffix List.** The app domain is registered there, so browsers treat each subdomain as a separate site and apps cannot share cookies.

**Name rules**

| Rule | Purpose |
| --- | --- |
| First come, with a credit deposit | Makes squatting cost something |
| Well-known brand names are held until ownership is proven | Stops others from claiming a company's name |
| Unused names expire | Returns abandoned names to the pool |
| Look-alike names are blocked | Limits phishing |

## 8. Draft app 1: private short-video feed

A TikTok-style app where the ranking that decides what you see runs on your own device, so no company and no node holds your viewing history. It uses all three job classes, the split rule, and local jobs.

**Pipeline**

| Step | What happens | Runs on | Class and tier | Privacy mode |
| --- | --- | --- | --- | --- |
| Upload | The creator's client cuts the video into segments and stores them | Network storage | Data, tier 1 | Open |
| Encode | Each segment is converted to 3 sizes | Network | Data, tier 2, split by segment | Open |
| Describe | A model produces a numeric summary of each video's content, plus safety labels | Network | Inference, tier 4 | Open |
| Count | Views, likes, and completions are totaled per video | Network | Menial, tier 0 | Private (totals) |
| Build pools | Each hour, videos are grouped into topic clusters with a ranked list per cluster | Network | Data, tier 2, split by cluster | Open |
| Personal ranking | The device fetches lists for clusters near its profile, plus some random ones, and ranks them | User's device | Local job | Private (local) |
| Serve | The client streams segments from nodes that store them | Network | Data, tier 1 | Open |

A 60-second video cut into 6-second segments yields 30 encode pieces, each small enough for a laptop. That is the split rule doing its job: one heavy task becomes many tier 2 jobs.

**What stays private**

- **Interest profile.** It is a set of numbers stored on the device and updated by a local job after each video. It is never uploaded.
- **Engagement.** The client splits each view and like into shares and sends them through relays to different nodes. Totals are computed from the shares, so no node sees one person's activity.
- **Fetch pattern.** Which cluster lists a device downloads hints at its interests. Fetching decoy clusters and using relays reduces this leak but does not remove it.

**What becomes public.** The ranking code and its weights are open modules. Anyone can read why a video ranks where it does, and a user could swap in a different ranking module.

**Who pays**

- **Viewers** spend a little on serve jobs. A device that stores segments and runs tier 0 checks while charging should roughly break even.
- **Creators** trigger the costly jobs: encode and describe. The app sponsors these at launch and later charges creators or funds them from a share of fees.

**Hard parts**

- **Moderation.** Nodes store and serve other people's videos. Safety labels must gate a video before any node is asked to hold it, and node owners need a way to refuse categories.
- **Fake engagement.** Anonymous totals are easy to inflate. Counts need per-account limits and weighting by account reputation.
- **Playback speed.** Home devices are slow and come and go. Each segment needs several replicas, and the client must fetch ahead.

## 9. Draft app 2: shared-world strategy game

A persistent strategy game whose world is simulated zone by zone on players' devices, so capacity grows with the player count and there is no central server to overload. It builds on the game-server idea in the original draft and relies on determinism, which the protocol already requires.

**How the world runs.** The map is divided into zones, and time advances in ticks of 5 seconds (starting value). Each tick, every zone with players in it becomes one job. Two nodes run it and their result hashes must match before the tick is accepted.

| Job | Input | Output | Class and tier | How often |
| --- | --- | --- | --- | --- |
| Command check | A signed player command | Valid or rejected | Menial, tier 0 | Per command |
| Zone tick | Zone state, valid commands, border summaries from neighbors | Next zone state and a new border summary | Data, tier 1 | Every tick, per active zone |
| Snapshot | All zone states | Compressed world save | Data, tier 2 | Every 10 minutes |
| World events | World summary | Storyline events and computer-player plans | Inference, tier 4 | Hourly, optional |

**Why it fits the platform**

- **Scaling.** A crowded zone splits into four smaller zones, each its own job. Empty zones create no jobs and cost nothing.
- **Verification.** The simulation is deterministic and seeded by zone and tick, so two honest nodes always agree. A cheating node is caught on the same tick.
- **Balance.** While someone plays, their device simulates zones for other players. A typical player earns about what they spend, so play is close to free.

**Fairness rules**

- A player's own nodes are never assigned the zone that player is in.
- The nodes simulating a zone change every tick.
- Simultaneous moves use commit and reveal: players first submit a hash of their move, then the move itself.

**Limits**

- **Speed.** A 5-second tick has to cover assignment, execution, and comparison, so zone jobs must finish in under 1 second on nodes with high availability scores.
- **Hidden information.** The nodes simulating a zone can read its state. The game design should keep most state public and not depend on secrets held inside a zone.
- **Genre.** Fast action games need responses within about 50 milliseconds and are out of scope for this design.

## 10. Draft app 3: Send, device-to-device transfer

Send moves any file or data to any device that has Third Spaces installed, and the receiving device can start a job on it automatically. It is useful to one person with two devices, so it needs no network of other users to get started.

**How a transfer works**

| Step | What happens | Runs on |
| --- | --- | --- |
| Choose target | The sender picks one of their own devices, or a contact who has shared an account ID or a device identity | Sender's device |
| Encrypt | The data is cut into chunks and encrypted to the target device's key | Sender's device |
| Deliver | Chunks travel directly when both devices are reachable, through relay nodes when not, and into network storage when the target is offline | Devices, relay nodes, storage nodes |
| Receive | The target checks the sender's signature and decrypts | Target device |
| Auto-run | If a rule matches, the target starts the job | Target device, or the network |

Nodes only ever handle encrypted chunks, so every transfer is private by default.

**Auto-run rules.** A rule has five parts: who the data is from, what type it is, which module to run, where to run it, and a spending cap.

| Example rule | Result |
| --- | --- |
| Video from my phone | The desktop converts it and files it in the library |
| Photos from my phone | The desktop makes thumbnails and stores an encrypted backup on the network |
| Dataset from my laptop | The desktop submits a batch job to the network and sends the results back |
| Anything from a contact | Nothing runs; the device asks first |

**Safety rules.** Auto-run executes code when data arrives, so six rules apply:

- **Off by default.** Only the device's owner creates rules.
- **Own devices first.** Rules accept only the owner's devices unless a contact is allowed for that rule.
- **Data, never code.** A sender cannot supply the module. A rule runs only modules the receiver has approved, identified by hash.
- **Sandboxed.** The job can read the received item and write to one output folder, and nothing else.
- **Capped.** Each rule limits file size, runs per day, and credits.
- **Logged.** Every run is recorded, and a rule can be paused in one step.

**Why it fits**

- **Personal compute.** A phone can hand heavy private work to the user's own desktop. This extends private mode's local route to every device on the account, at no cost.
- **First app.** It works for a single user, and every install adds a node to DeComp.
- **Base layers only.** It needs accounts, device keys, relays, storage, and private mode. An always-on inbox is optional.

**Costs.** Direct transfers are free. Relayed and stored transfers are data jobs charged to the sender.

**Limits**

- **Installs.** Both devices need Third Spaces.
- **Phones.** Mobile systems restrict background work, so desktops are the natural auto-run targets.
- **Connections.** Direct links fail on some networks, and relays cover those at a cost.
- **Unwanted data.** Receivers accept only from allowed identities and can block a sender.

## 11. Version 1: scope and acceptance

Version 1 is the whole system in this spec, built and released as one. There are no phases. Issues found after release, and later changes, produce new versions.

**In version 1**

| Layer | Included |
| --- | --- |
| DeComp | Accounts and device linking; desktop node agent with a metered sandbox; tiers, splitting, and weighted random assignment on public randomness; confirmation by second pickup, spot checks, checkpoint disputes, and known-answer jobs; TOPLOC checks for inference; attested nodes; credits, fee split, stake, and penalties; a control plane of ICP canisters; chunked, replicated storage with rented retention |
| Third Spaces | SDK with typed manifests and standard modules; open and private modes with all three private routes and the enclave opt-in; batch and always-on components on a single bill; sign-in by passkey, wallet, recovery key, guardians, and email reset; buying and cashing out credits; the home launcher, name registry, and gateway |
| Apps | Send; the private short-video feed; the shared-world strategy game |

**Also in version 1.** Nothing in this spec is left for later: zero-knowledge proofs, compute for other chains, governance by token-holder vote, job modules written in any language that compiles to WebAssembly, a node agent for phones, real-time apps, and a tradable token are all part of version 1, and come last in the build order.

**Acceptance criteria.** Version 1 is done when all eight hold. The targets are proposals.

| Test | Target |
| --- | --- |
| Fault detection | Across 10,000 tier 0 and tier 1 jobs on 10 devices, every deliberately wrong result is caught |
| Distribution | Measured tier shares land within 10 points of target on 50 devices |
| Cost | Cost per encoded minute of video is measured and compared with a cloud price |
| Balance | Over 2 weeks with 100 outside testers, balanced users end within 20% of zero net credits |
| No server | No job touches a server run by the team |
| Security | Verification, accounts, and credits pass an outside security review |
| Legal | Cash-out and the token decision are cleared with legal advice |
| Apps | Send delivers and auto-runs between two devices; the feed app and the game run with outside testers |

**Build order.** This is the order the work depends on, not a set of releases.

- Accounts, device keys, and device linking
- Node agent and metered sandbox
- Control-plane canisters: accounts, registry, job board, receipts
- Credits, fee split, and penalties
- Tiers, splitting, node scores, spot checks, and checkpoint disputes
- Test harness that injects wrong, late, and missing results
- SDK, standard modules, storage, and relays
- Name registry, gateway, and home launcher
- Send app
- Private mode routes and threshold keys
- Always-on components and the single bill
- Feed app and game
- Inference class with TOPLOC, and attested nodes
- Storage: chunking, placement, repair, and retention
- Sign-in and recovery services; buying and cashing out credits

**Versioning**

| Change | New version | Example |
| --- | --- | --- |
| Fix for a found issue; formats and rules unchanged | 1.0.1 | A bug in dispute handling |
| Added capability that existing apps and nodes still work with | 1.1 | Zero-knowledge proofs for small jobs |
| Change that breaks manifests, receipts, or account records | 2.0 | A new receipt format |

**Version history**

| Version | Date | Status | Contents |
| --- | --- | --- | --- |
| 1 | 2026-11-15 | Draft | Full system: DeComp, Third Spaces, and three apps |

## 12. Open questions and risks

The largest risk is cost: running each job twice on home hardware may cost more per useful result than a cloud server, and version 1 testing has to measure that.

| Question or risk | Why it matters | How to resolve it |
| --- | --- | --- |
| Cost against cloud | If it is higher, the pitch becomes "no servers to run and near-free for balanced users", not "cheaper" | Measure cost per encoded minute during version 1 testing |
| Supply at the right time | Home devices go offline, and demand peaks when owners are using them | Jobs run unconfirmed when nodes are scarce, with the always-on fallback for expired jobs; the base price floats |
| Collusion | One owner with many accounts can match their own wrong results | Stake, probation, and known-answer jobs raise the cost; none removes it fully |
| Privacy of computation | A node can read what it computes on | Open and private modes (Section 7); private compute on the network costs far more than open compute |
| Content liability | Nodes store and serve other people's content | Each space answers for its own content; Discover lists verified spaces only; node owners choose which spaces their devices serve |
| Credits as money | Buying and cashing out credits may fall under financial regulation | Legal advice before version 1 launches |
| Phones as nodes | Mobile systems restrict background work, and batteries drain | Desktop first; phones only while charging on Wi-Fi |
| Setting the decay ratio | It decides whether casual or strong devices earn most | Simulate with test data before fixing a value |
| Default payer | Charging users by default versus sponsoring by apps changes adoption | Decide before SDK v0 |
| Harmful jobs | A module could try to escape the sandbox or misuse nodes | No network access in the sandbox; review modules before they enter the registry |
| Two payment units | The SDK hides cycles from developers, so a treasury must convert credits into cycles | Decided: the SDK handles cycles. Still open: who runs the treasury and carries its price risk |
| Dependence on ICP | The control plane and always-on mode both inherit ICP's pricing, governance, and uptime | Keep the always-on interface generic so another host can be added |
| Collusion between share holders | Secret sharing fails if one owner controls all the nodes holding a job's shares | Random assignment across accounts and chip vendors; measure how concentrated node ownership is |
| Cost of encrypted compute | At 100 to 100,000 times the open price, the third private route may only suit very small jobs | Benchmark standard encrypted operations and set the size cap from the results |
| Account recovery | Each recovery method is also an attack path, and email reset relies on a trusted service | Set defaults for the waiting period and the two-method rule; test with real users before launch |
| Token or no token | A token can fund the start but adds legal exposure and price risk | Decide with legal advice before version 1 launches |
| Public balances | On a public ledger, anyone who knows an account ID could look up its credits | Keep balances under a ledger address that differs from the identity apps see |
| Name disputes | Someone must decide when two parties claim a name or a brand objects | Publish a naming policy and a dispute process before launch |
| Gateway as a central point | Whoever runs the gateway can block or alter apps for browser users | Let several operators run gateways; the desktop and mobile apps bypass it |
| Control-plane throughput | Every lease, commitment, and record write is a canister call, which caps jobs per second and adds cost | Batch leases and results; split the job board across subnets; measure the ceiling in testing |
| Unconfirmed results | With few nodes, most results rest on one node's reputation | State the level on every receipt; audit a random share; let apps require confirmation |
| Split accounts | A large operator can spread its machines over many accounts to stay under the share caps | Stake and probation per account; look for accounts that behave as one |
| Cash price of a credit | The credit is defined by compute, so its money value is set only where credits are bought and cashed out | Decide how that price is set, between the providers' cost floor and the cloud price ceiling |

## Sources

- [INTELLECT-2: A Reasoning Model Trained Through Globally Decentralized Reinforcement Learning](https://arxiv.org/html/2505.07291v1), Prime Intellect, May 2025. Used for TOPLOC, random-sample verification, the moving-average node scores with healing factor, pull-based task distribution, and the ledger's scope.

- [ICP network overview](https://docs.internetcomputer.org/concepts/network-overview/) and [ICP concepts](https://docs.internetcomputer.org/concepts/), DFINITY Foundation developer docs. Used for subnet size, speed, node vetting, and canister capabilities in the execution modes comparison.

- [New TEE.Fail Side-Channel Attack Extracts Secrets from Intel and AMD DDR5 Secure Enclaves](https://thehackernews.com/2025/10/new-teefail-side-channel-attack.html), The Hacker News, October 2025. Used for the limits of attestation in Section 4.

- [FHECore: Rethinking GPU Microarchitecture for Fully Homomorphic Encryption](https://arxiv.org/html/2602.22229), Daksha et al., July 2026. Used for the cost of homomorphic encryption in Section 7.

- [AWS EC2 c7g.large pricing in us-east-1](https://www.doit.com/compute/spot/us-east-1/c7g.large), DoiT Compute, read November 15, 2026. Used for the cloud benchmark in Section 5.
- [Customers, don't expect electric bill relief in 2026](https://www.utilitydive.com/news/customers-should-not-expect-electric-bill-relief-in-2026-the-cake-is-bake/810450/), Utility Dive, January 2026. Used for the electricity price in Section 5.
- [Snap Q4 2025 earnings call transcript](https://www.fool.com/earnings/call-transcripts/2026/02/04/snap-snap-q4-2025-earnings-call-transcript/), The Motley Fool, February 2026. Used for compute cost per user in Section 5.
- [Nvidia RTX 4090 cloud pricing](https://getdeploying.com/reference/cloud-gpu/nvidia-rtx-4090), GetDeploying, October 2026. Used for GPU rental rates in Section 5.
- [Launching an SNS](https://docs.internetcomputer.org/guides/governance/launching.md), ICP developer docs. Used for the funding options in Section 5.

- [Chain-key cryptography](https://docs.internetcomputer.org/concepts/chain-key-cryptography/), ICP developer docs. Used for signing on other chains in the web3 integration table.
