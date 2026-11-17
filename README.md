# DeComp

The Decentralized Compute Protocol: applications run on compute contributed by
devices instead of a central cloud. Nodes run jobs, applications submit them, and
a coordinator matches the two. The design is sections 1 to 6 of
[the spec](../docs/spec.md).

DeComp stands alone. Nothing in this folder depends on, or mentions, anything
built on top of it. What it publishes for others to build on is:

- the coordinator's HTTP API (`crates/decomp-coordinator/src/api.rs`);
- the Rust crates in `crates/`, for programs that act as a device or a node;
- the TypeScript client in `client-ts/`, for interfaces that act as a device;
- the module interface a job is written against
  (`crates/decomp-sandbox/src/lib.rs`).

## What works today

Build step 1 of 14: **accounts, device keys, and device linking**.

- An account is an Ed25519 key. Creating one stores no personal data anywhere.
- Each device has its own key. A device joins an account when a device already in
  it approves the new device's code, and any device can remove another.
- The coordinator keeps the account records in an append-only log of the signed
  requests that produced them.

Build step 2 of 14: **node agent and metered sandbox**.

- The sandbox runs a WebAssembly job module with no access to a clock, network,
  files, or randomness, so the same module and input give the same output on any
  machine.
- Compute is billed in units of ten billion WebAssembly instructions, never in
  time. A job stops when it reaches its compute cap, and it gets 512 MB of memory.
- The node agent can run a module on this device alone and report the output hash
  and the units used.

Build step 3 of 14: **coordinator check-in, assignment, hash comparison, receipts**.

- A device submits a job: a module, an input, a compute cap, a deadline, and how
  many nodes must run it (at least two).
- Nodes check in for work and never choose it. The coordinator hands each waiting
  job to idle online nodes drawn at random, one per account.
- Each node runs the job in the sandbox and signs what it found. When all have
  reported, identical results verify the job and produce a receipt; differing
  results mark it disputed. A job that is not verified by its deadline expires.
- A receipt holds the job ID, the module, input, and output hashes, the units
  used, and the signed report of every node, so anyone can recheck it.

Build step 4 of 14: **credits, fee split, and penalties**.

- Every account has a balance. A new account is given 25 starter credits, which
  can be spent but are tracked apart so they can never be cashed out.
- Submitting a job sets aside its highest possible fee: the compute cap, times
  the price of a unit, times the number of nodes. A job the balance cannot cover
  is refused.
- A verified job is charged for the units it used. 85% goes to the accounts of the
  nodes that ran it, split equally, 10% to the verification fund, and 5% to the
  protocol treasury. The rest of what was set aside is returned.
- A job that expires or is disputed pays no one and is refunded in full. A job
  that fails the same way on every node is still paid for, because the nodes did
  the work.

Build step 5 of 14: **tiers, splitting, node scores, spot checks, and checkpoint
disputes**.

- **Splitting.** A job can carry a rule that cuts its input into pieces of a
  given size. Each piece runs as a job of its own (assigned, checked, paid for,
  and given a receipt separately), so no node sees more than its piece, and a
  large job becomes small-tier jobs. The job's output is the pieces' outputs
  joined in order.
- **Scores.** Each node has a reliability and an availability score, moved by
  what it does and pulled slowly back toward the network average. A new node is on
  probation until 20 of its results have been confirmed.
- **Tiers.** A job's tier is how many times its compute cap doubles one unit. A
  node on probation takes tier 0 only; a proven node takes tier 1, tier 2 from
  reliability 0.95, and tier 3 and up from 0.98 with stake that covers the job.
- **Assignment by score.** Nodes are drawn at random in proportion to
  reliability times availability, never two from one account.
- **Spot checks.** A proven node runs a job alone. One job in ten is quietly given
  to a second node as well, paid from the verification fund. A node on probation
  is always paired.
- **Known-answer jobs.** Now and then a node is handed a job that two nodes have
  already agreed on. Getting it wrong sends the node back to probation.
- **Disputes.** When nodes disagree, the coordinator finds the first checkpoint
  where their runs parted, re-runs the job that far, and keeps the nodes whose
  checkpoint matches. One honest node is enough. Wrong nodes lose reliability and
  pay the cost of refereeing out of their stake.
- **Stake.** An account can stake earned credits behind its nodes and take them
  back when none of its nodes is on a job. A node sent back to probation costs
  its account a tenth of its stake.

Also built, from build step 7: host functions a job can call (SHA-256 hashing,
Ed25519 signature checks, sealing and opening, randomness fixed by the job, so
every node draws the same bytes, and reading and storing blobs), and a check of a job's input against
the types its module declares, made when the job is submitted and before any
node runs it. A job reads the blobs its manifest names, and the blobs it stores
are part of its result, so nodes are held to them like the output.

**Storage, done by nodes** (the first of the data services in build step 7).

- A node that other devices can connect to says so when it checks in, and from
  then on may be asked to hold data. Nodes that cannot be reached do compute only.
- An account pays to have a blob held for a number of days. The coordinator
  chooses up to three such nodes, each from a different account, and the device
  uploads the blob to each of them directly. The coordinator never holds it.
- A stored blob is named by the root of a hash tree over its chunks. A node takes
  in only a blob it was asked to hold and that hashes to its root.
- Every so often each node is challenged to produce one randomly chosen chunk,
  with the hashes that link it to the root. A node is paid for the time up to
  each challenge it passes. Failing, or not answering within a minute, costs it
  that stretch of pay and some availability; three failures in a row and its
  copy is counted as lost.
- Anyone who knows a blob's root can ask where it is held and fetch it from a
  node. What arrives is checked against the root, so a node cannot pass off
  other data.

**Relays, done by nodes** (the second data service in build step 7).

- Two devices that cannot connect to each other both connect to a relay: a node
  that accepts connections. The sender opens a channel to the receiving device,
  and the coordinator picks the relay and sets aside the fee for the most the
  channel may carry. The data itself never passes through the coordinator.
- What a device sends is sealed to the receiving device's key before it leaves,
  so a relay carries bytes it cannot read. Each device publishes a sealing key
  under its own signature, and a sender checks that signature itself.
- The sender signs every frame it sends, so the receiver can tell that nothing
  was changed, dropped, or reordered on the way. Only the receiving device can
  collect a channel.
- A relay is paid for what arrived. Each time the receiver asks for more, it
  signs for what it already has, and the relay shows that signature and the
  sender's to the coordinator. A relay that takes data and does not deliver it
  earns nothing, and the sender gets back whatever was not delivered.
- A relay holds a few megabytes for a receiver that is behind and makes the
  sender wait beyond that. A channel closes when its last frame is delivered, or
  after an hour.
- A node keeps checking in while it runs a job, so taking in blobs, answering
  storage challenges, and carrying channels do not wait for the job to finish.

**Three ways to a device** (the base of build step 8). A sender does not choose
between them.

- A device shows that it is waiting to receive by asking what is addressed to
  it. If it is waiting and other devices can connect to it, the sender's channel
  goes straight to it, and nobody is paid.
- If it is waiting and cannot be reached, the channel goes through a relay.
- If it is not waiting, the data is left in storage for it, sealed and with the
  sender's signature inside, for as many days as the sender pays for. The device
  is told it is there the next time it asks.
- The command-line node and the TypeScript client both do all of this, and send
  to each other. A node's data server answers web pages, so a browser can reach it.

**Apps and the jobs they pay for** (also build step 7).

- An account can publish an app: a name, the hashes of its job modules, and
  whether it pays for its users' jobs. The app's ID is the account's ID, and the
  app's credit pool is the account's balance.
- A job is charged to whoever submits it, unless its manifest names an app as
  sponsor. The app then pays, if the job runs one of the app's own modules and
  the submitter is within what the app allows one account in a day.
- Publishing again replaces the manifest.

**App state** (also build step 7).

- An app's state is cut into shards, each a set of records under a name the app
  chooses. A job can be handed one shard if it runs one of the app's own
  modules, whoever submits it.
- The job reads the records as they were when it was handed out, with `kv.get`
  and `kv.scan`, and changes them with `kv.put`. What it leaves behind comes back
  in its result and is checked like its output: nodes that ran the job must
  agree on it, and a referee settles it if they do not.
- Jobs on the same shard run one at a time, each starting from what the last
  one left, so no change is lost. Jobs on different shards run side by side.
- A job that fails changes nothing.
- Anyone can ask what a shard holds now and read its records.

**Follow-up jobs** (also build step 7).

- A job can ask for more jobs with `job.spawn`: a module, an input, and a
  compute cap for each. The module must be one of the blobs the job was given
  to read.
- They are started once the job that asked is verified, in the order it asked,
  and paid for out of what that job left of its fee cap. Those the rest does not
  cover are not started. They keep the asking job's deadline, node count, and
  payer, and belong to the same submitter.

**Private jobs** (the first half of build step 9).

- A job is open or private. The nodes that run an open job can read its data;
  of a private job the spec promises that "no node ever sees the data
  unencrypted". A module says which mode its job was written for in the
  description it gives of itself, and a job's manifest must say the same. The
  coordinator refuses a job whose manifest and module disagree.
- A device runs a private job by "the first route that fits", and says which
  it used. There are two routes so far.
- **Local** is taken when "the job fits the user's device": the module runs
  there, with no verification, and nothing of the job reaches a node or the
  coordinator.
- **Private totals** is taken next, when "the job only adds, counts, averages,
  or applies fixed weights". The device cuts each private number in the input
  into two shares: random numbers that add up to it. It submits the two
  resulting inputs as two jobs on the same module, and adds the two outputs
  together. A share alone says nothing about the number it is a share of.
- The two jobs carry one name, and jobs with one name are never handed to nodes
  of the same account. A share waits until a node of another account is free.
- Each share's job is assigned, run, checked, and paid for like any job, so the
  route costs two jobs. Verification is unchanged: nodes that run a share must
  agree, and a referee settles it if they do not.
- Nodes do see every value that is not marked private, and how many private
  numbers there are.
- A private job that no route fits is not run. `submit` stops with the spec's
  `NoPrivateRoute` before anything is sent. The coordinator applies the same
  rule to what it is sent, so no node is handed a private job that came by no
  route.
- **What every result reports.** "Each result carries a receipt with five
  fields: the privacy route used, how the result was verified, what it cost,
  how long it took, and whether an enclave was involved." A receipt already
  had the cost. It now also has the private route, if the job took one; whether
  nodes agreed, a referee settled a disagreement, or one node ran the job and
  nothing was compared; the seconds from submission to result; and
  `enclave`, which is false. `submit` shows the five as `route`, `checked`,
  `fee`, `time`, and `enclave`, and so does `run` for a private job.
- **Sealing inside a job.** A job can seal a message to a key with `seal`, so
  that only the device holding the opening key that goes with it can read the
  result. Every node that runs the job seals to the same bytes.
- A job can ask to `open` a sealed message. A private job run by the local
  route opens what was sealed to the device it runs on. A node holds no user's
  key, so on a node `open` answers that it cannot, whatever it is given.
- Run with the command-line node: a message sealed by a job on two nodes, which
  agreed on the bytes, was opened by a private job on the device it was sealed
  to, and by no other device. The TypeScript client was not used to open one.
- Private totals was run on four proven nodes of four accounts. With two nodes
  to a share, no account was in both shares in ten runs, and the sums were
  right. With three nodes online the second share waited for a fourth account
  and expired. A fifth node that reported a wrong result for a share was found
  out by the referee. That a share is never reissued as a known-answer job was
  not seen in a run: such jobs come up one time in fifty.

## Layout

| Path | What it is |
| --- | --- |
| `crates/decomp-core` | Protocol rules: keys, signed envelopes, account state, credits, node scores, the job rules (assignment and verification), the storage and relay rules, sealing, app state, private jobs and their routes, and every message shape. No I/O. |
| `crates/decomp-schema` | How a job's input and output types and its privacy mode are described and checked, and the byte form of a set of records. Kept apart from `decomp-core` so that code compiled into a job module can use it. |
| `crates/decomp-sandbox` | Runs job modules: deterministic, metered WebAssembly. Its crate docs define the module interface. |
| `crates/decomp-coordinator` | The coordinator service: the HTTP API, the dispute referee, and what it stores (logs, blobs). |
| `crates/decomp-node` | The node agent, as a library and as the `decomp-node` command: joins an account, works as a node, holds and relays data, submits jobs, stores files, and sends files to other devices. |
| `client-ts` | The TypeScript client: message shapes, signing, calls, and sending to and collecting from other devices, from all senders or only those the device accepts, and submitting a job and waiting for its result. It has no dependencies: a device supplies its keys and its cryptography, and a ready-made one for platforms with WebCrypto is included. |

## Run it

The coordinator (listens on `127.0.0.1:8787`, writes its data to `./data`):

```bash
cargo run -p decomp-coordinator
```

A node. `link` prints a device code and waits until a device already in an account
approves it. `start` then works until stopped. Give each node on one machine its
own `--dir`, which is where its key is kept (default `node`):

```bash
cargo run -p decomp-node -- link
```

```bash
cargo run -p decomp-node -- start
```

A node that also holds and relays data. Other devices must be able to connect to
the address it listens on; `--public-url` says how, if that differs:

```bash
cargo run -p decomp-node -- start --listen 0.0.0.0:9000
```

A file stored on the network for a week, and fetched back by the root that
`store` prints:

```bash
cargo run -p decomp-node -- store path/to/file --days 7
```

```bash
cargo run -p decomp-node -- fetch <root> --output path/to/copy
```

A file sent to another device. The receiving device runs `receive`, which shows
its code and waits; the sender names that code. If the receiver is not waiting,
`send` leaves the file in storage for it, for 7 days unless `--keep-days` says
otherwise, and the next `receive` there collects it:

```bash
cargo run -p decomp-node -- receive --output path/to/copy
```

```bash
cargo run -p decomp-node -- send path/to/file --to <device code>
```

An app published by this device's account, which pays for up to 5 credits of
each user's jobs a day, and a job another account runs at the app's expense. The
app ID is the one `publish` prints:

```bash
cargo run -p decomp-node -- publish --name "My app" --module path/to/module.wasm --sponsor-daily 5
```

```bash
cargo run -p decomp-node -- submit path/to/module.wasm --input path/to/input --compute-cap 4 --sponsor <app ID>
```

A job sent to the network, from a linked device:

```bash
cargo run -p decomp-node -- submit path/to/module.wasm --input path/to/input --compute-cap 4
```

A job may be given files to read as blobs with `--read path/to/file`, once for
each file. The module asks for one by the hash of its contents.

A job that asks for follow-up jobs needs room for them in its fee cap:
`--follow-up-credits 5` sets aside five credits more than the job itself can
cost. `submit` waits for the follow-ups and shows how each went.

A job may be handed a shard of an app's state with `--state <app ID>/<shard>`,
if its module is one of that app's. The records a shard holds:

```bash
cargo run -p decomp-node -- state <app ID>/<shard>
```

The same job on this device alone, without the network:

```bash
cargo run -p decomp-node -- run path/to/module.wasm --input path/to/input --compute-cap 4
```

A private job is sent with the same two commands, and each shows the route it
took. `run` is the local route. `submit` takes the first route that fits: the
local route for a job of one compute unit, which is then never sent anywhere,
and private totals for a larger job that only adds up private numbers. Any
other private job is refused with `NoPrivateRoute`.

Coordinator settings are `DECOMP_ADDR`, `DECOMP_DATA_DIR`, and
`DECOMP_STORAGE_CHALLENGE_SECS` (how often each node is asked to show that it
still holds a blob; an hour unless set). The node agent takes
`--coordinator` and `--dir`, or `DECOMP_COORDINATOR_URL` and `DECOMP_NODE_DIR`.

Checks, run before every commit, from this folder:

```bash
cargo fmt --all -- --check && cargo clippy --all-targets
```

## Design decisions

**One signed-message format.** Every request is an envelope: a JSON payload as a
string, the signer's public key, and a signature over `"decomp/v1\n"` plus that
exact string. The verifier checks the bytes it received and only then parses them.
Because nothing is ever re-serialized, there is no canonical encoding to keep in
step across languages.

**No sessions.** The coordinator has no logins or tokens. Reads are signed too,
because an account's device list is not public.

**Two endpoints.** `POST /v1/ops` takes a change to an account, which is logged.
`POST /v1/request` takes everything else as one tagged message type, `Request`,
defined once in `decomp-core` and used by both the coordinator and the node agent.
Every request carries the time it was issued and is refused if that is more than
five minutes from the coordinator's clock.

**The job rules live in `decomp-core`, not in the coordinator.** Assignment and
verification are pure functions of the requests, the time, and a source of
randomness. Later, other coordinator operators can rerun the same rules to audit
a batch.

**Results are compared whole.** Nodes must agree on the units used as well as the
output, so a node cannot inflate its bill. A job that fails the same way on every
node is a verified failure, not a dispute.

**Credits are whole millionths.** Amounts are integers, so fee shares are exact
and the parts of a split always add up to the fee; the treasury takes what
rounding leaves.

**Credits are set aside before a job runs.** The highest possible fee is held
from the moment a job is accepted, and a result claiming more compute than the
cap is refused, so settling a job can never fail or overdraw an account.

**Logs are the database.** The coordinator rebuilds its account records by
replaying `ops.jsonl`, rebuilds balances by replaying `ledger.jsonl` on top of the
starter grants, and appends each receipt to `receipts.jsonl`. Credits set aside
for jobs that were open when the coordinator stopped are returned at startup. Open jobs and
online nodes are kept only in memory, as the spec intends. The two files are the
"local signed log" the spec allows during development; ledger settlement replaces
them in build step 13.

**A module counts its own instructions.** Before a module runs, the sandbox
rewrites it (`crates/decomp-sandbox/src/meter.rs`): it adds a counter that the code
subtracts from as it executes, and a call to the host when the counter runs out.
The count is a property of the rewritten module, not of the engine, so any
WebAssembly engine bills the same job the same way. The first version used the
engine's own fuel counter, which tied the bill to one engine version and offered no
way to pause a job at an exact instruction count.

**Checkpoints.** Each time a module's counter runs out (every billion
instructions), the host hashes the job's memory and instruction count into a
checkpoint. A result carries the hash of its checkpoint list. The spec's interval
is 10 million instructions; at that interval hashing memory would cost more than
the job, so the starting value here is a hundred times longer.

**A compute unit is ten billion instructions.** The spec fixes that a unit is a
count of instructions but not the count. Ten billion measured at about one desktop
core-second, which fits the spec's tiers: tier 0, one unit, is a job of under a
second. The owner confirmed this value on 2026-11-16.

**Everything that grows with data is billed.** Every byte a host function copies
costs one instruction, and so does every byte a module copies or fills with a
single bulk instruction, so a module cannot do unmetered work either way. What a
job hands back, its output and the blobs it stores, is also bounded by a slot's
memory, so a job cannot fill its host's memory for the price of one unit.

**A job's blobs are named before it runs.** The spec lets a job fetch any blob by
its hash. But whether a blob exists changes over time: a node that ran a job
before some blob was stored and a node that ran it after would both be honest
and would disagree, and the first would be judged wrong. So a job's manifest
lists the blobs it may read, each must exist when the job is submitted, and
asking for any other blob gives the same answer on every node: not given.

**Clients in other languages mirror `decomp-core`.** `client-ts/src/protocol.ts`
repeats the message shapes and the signing convention in TypeScript, and must
change together with the Rust. It holds no keys: a caller hands it a `Signer`, so
each platform keeps secrets in its own secure storage.

**An app is an account.** The spec gives each app an ID that is the hash of its
developer's signing key, a manifest signed with that key, and a credit pool of
its own. An account already is all three: its ID is the hash of its key, changes
to it are signed and ordered, and it has a balance. So publishing an app is one
more kind of change to an account, and nothing new had to be built to hold apps,
fund them, or replay them at startup. A developer with two apps uses two
accounts. As with other changes to an account, a device the account has approved
may sign for it.

**A shard is one blob, and jobs on it take turns.** Results are checked by
comparing them, so what a job reads must be fixed and what it changes must be
part of its result. A shard is therefore a set of records with exactly one byte
form, named by its hash like any blob. The coordinator keeps only which hash is
current for each shard. A job is told the hash when it is handed out, every node
on it reads those same records, and the records it leaves are a new blob whose
hash is in its result. Running a shard's jobs one at a time means none has to be
redone because another got in first. The price is that a job reads and writes
its whole shard, so shards are small, 8 MiB at most, and an app with more state
uses more shards.

**Follow-up jobs wait for the verdict.** A job's request for more jobs is part
of its result, so it counts only once the result stands. Nothing a job that was
wrong, or failed, asked for is ever run. The follow-ups are funded from what the
job left of its own fee cap, so one cap, set by the submitter, bounds what a job
and everything it sets off can cost.

**Data services are node work.** Storing, serving, and relaying data are done by
nodes and paid for in credits. DeComp does not lean on an outside peer-to-peer
network for them.

**Only reachable nodes hold or relay data.** A device behind a home router cannot
accept connections, so it cannot serve a file to anyone or pass one on. Nodes
that can be reached take the data work; every node can still do compute, which
needs only outgoing connections. The coordinator stays out of the data path: it
decides who holds or carries what, checks them, and pays.

**A device has a second key, for sealing.** A device signs with an Ed25519 key.
Data is sealed to a separate X25519 key, because a browser can keep a signing key
that cannot be used for anything else, and that is how the web app keeps its
own. The device publishes the sealing key in a request signed with its device
key. The coordinator keeps the signed request and hands it to anyone who asks,
so a sender checks the device's signature and does not have to trust the
coordinator about whose key it is. Sealing itself uses X25519, HKDF with SHA-256,
and AES-256-GCM, all of which a browser does natively, so every kind of device
seals the same way.

**Asking is how a device shows it is there.** Only nodes check in, so the
coordinator cannot otherwise tell whether a device is around to receive. A device
that wants to receive asks, every second or so, what is addressed to it, and
that is taken as waiting for the next fifteen seconds. A channel is opened only
to a waiting device, so a sender finds out at once that it should use storage
instead, and no relay holds data for a device that may never come.

**A relay is paid on the receiver's signature.** Paying a relay for whatever the
sender handed it would pay a relay that threw the data away. So a claim needs two
signatures: the sender's, on a frame header that counts the bytes sent so far,
and the receiver's, on a request that says it has that frame. The receiver signs
as part of asking for more, so no extra message is needed. The most a relay can
lose to a receiver that takes data and does not sign is one answer's worth, four
megabytes.

**A node checks in while it works.** A job runs on its own thread and the node
keeps checking in every two seconds. Storage challenges must be answered within a
minute and a new channel must be picked up at once, and a job can run far longer
than either.

**Stored blobs are named by a hash-tree root, not a plain hash.** With a root, one
chunk can be proven to belong to the blob, so the coordinator can check that a
node still holds a blob without holding it too. Modules, inputs, and outputs keep
their plain hashes.

**Storage is priced by size and time, relaying by the byte.** Before the usual fee
split, a node is paid 180 credits to hold a gibibyte for a day and 2,500 credits
to relay a gibibyte. The spec prices compute and not data. These starting values
are what cloud storage and hosted bandwidth cost at the credit value the spec's
compute price implies. They are `CREDITS_PER_GIB_DAY` in
`crates/decomp-core/src/storage.rs` and `CREDITS_PER_GIB_RELAYED` in
`crates/decomp-core/src/relay.rs`, and the owner has not yet confirmed them.

**What `seal` and `open` do inside a job, and why.** The spec lists both among
the host functions and says "Sealing encrypts to a user's key". Two things
about a job decide what they can be.

Every node must produce the same bytes, and a job has no randomness but what
its seed fixes. Ordinary sealing draws a new key pair for each message. Inside
a job that one-time key is worked out instead: it is the SHA-256 hash of a
label, the job's seed, the receiving key, and the message. Everything after
that is ordinary sealing, so a device opens the result as it opens anything
else sealed to it. The message is part of the hash for a reason. The cipher
uses a fixed nonce, which is safe only while each one-time key seals one
message. A key made from the seed and a counter would seal two different
messages under one key whenever two runs had the same seed, and every run on a
device alone has the same seed.

Bytes that are fixed can be worked out again, and that is what sealing in a job
gives up. Someone who knows the job's ID and the receiving key, and can guess
the message, can seal the guess and compare. The nodes that ran the job had
the message to begin with, and anyone holding an open job's module and input
can run it again. So `seal` protects a message after the job, where it is
stored and carried, from those who never had the job's input. It does not hide
anything from the nodes that run the job. Hiding data from them is what the
private routes are for.

A node holds no user's opening key, and must not. So `open` is given no key by
the job: it opens with a key the device running the job was given for it. The
command-line node gives its own opening key to a private job it runs by the
local route, and to nothing else. Nodes, the referee, and an open job run on a
device alone are given none. There `open` answers "cannot be opened" for any
input, and charges the same, so every node agrees and results still compare.

Each costs a job what checking a signature does, and one instruction more for
each byte (`SEAL_INSTRUCTIONS` in `crates/decomp-sandbox/src/lib.rs`). That
figure was not measured for sealing itself.

**A module declares its own privacy.** The mode is part of the description a
module gives of itself, next to its types, because it is a property of the code:
a private job is one written with private types. The manifest repeats it, as the
spec's manifest does, and the coordinator checks one against the other. So a
module written to be private cannot be run as an open job by mistake, nor the
other way round. A module that does not describe itself is open.

**The route rule is in one place and applied twice.** Which route a private job
takes is decided by `privacy` in `decomp-core`. The device that wants the job
run asks it first, so a job with no route is stopped before anything leaves the
device. The coordinator asks it again about every job it is sent, because a
device could be wrong or could lie, and what the rule protects is on the
coordinator's side: which jobs nodes are handed.

**A share is an ordinary job with a name on it.** "Shares and encrypted values
are ordinary bytes to a job, so duplicate runs and checkpoint disputes work in
both modes." So the coordinator has no second kind of job for shares. A share's
job is submitted, assigned, checked, refereed, and paid for by the rules every
job goes by. The manifest's `share` field names the set of shares a job belongs
to, and the job rules add two things for a job that has one: it is not handed
to an account that was handed another job of the same set, and it is not kept
to be reissued as a known-answer job, because the node asked could be of such an
account. The name is the submitter's own: two accounts that use the same name
do not affect each other. A share is also refused a shard of state, since a job
could leave its share there and anyone can read a shard.

**Shares are numbers that add up, wrapping at 2^53.** Nothing else is needed for
adding and for weights that are not private, so no heavier scheme is used. All
but one of a number's shares are drawn from the operating system's randomness
and the last is what is missing from the sum. The sum wraps at 2^53 and not at
2^64 because a share is as likely to be large as small, and a whole number
below 2^53 comes through every JSON reader unchanged, including one that keeps
numbers as 64-bit floating point, as a browser does. A private number is
therefore a whole number from 0 to 2^53 - 1.

**Two shares.** The spec prices this route at "about 2x" an open job. Each
share's job costs what the job would cost open, so two shares is the count that
matches (`SHARES` in `crates/decomp-core/src/shares.rs`). More shares would keep
a number private from more accounts working together, at that many times the
price. With two, the two accounts that hold a job's shares can read it if they
work together.

**A job's types say whether it can run on shares.** A module's description
names each private number it takes and returns. A job whose private values are
all private numbers is taken to do nothing but add them and weight them, which
is what the types it is written with allow. The device cuts shares and adds
results by that description, and the coordinator accepts a share only of such a
job. Privacy does not rest on the module keeping to this: its nodes are handed
only shares whatever it does. What rests on it is the answer, which is wrong if
a module does anything else with a share.

**A result by private totals is the same values, not the same bytes.** The
device reads the shares' outputs as JSON, adds, and writes the result out
again, and that writes a struct's fields in the order of their names. The
local route gives the module's own bytes.

**A share is checked as any job is, and the receipt says how.** The spec's
table has this route "verified by duplicate runs on each share", and also says
verification is unchanged. Unchanged is what is built. Left to the coordinator,
a share's job is run by one proven node and spot-checked now and then, or by
two nodes when the first drawn is on probation, exactly like an open job, and
`--redundancy 2` asks for two nodes on every share. The spec's "about 2x"
holds either way, since an open job is checked by the same rule.

**A result nothing was compared with says so.** "A result that is unverified or
under dispute is flagged as such." A proven node runs most jobs alone, and the
protocol counts its result as standing. The receipt's `verified` field then
reads `unchecked`, and `submit` prints "no: one node ran it, and no other was
asked to". The same is said of a job its submitter asked one node to run. A
job whose pieces were checked in different ways reports
the weakest of them.

**A job on the local route has no receipt.** Receipts are written by the
coordinator, and a job that runs on its user's device never reaches it. The
device reports the five things itself: the route is local, checking is "not
needed; the device belongs to the user", the cost is nothing, the time is what
the device measured, and no enclave was involved.

**`enclave` is reported before any enclave exists.** It is one of the spec's
five fields, and "no" is a true answer today: no node can show that it runs in
an enclave, so no job is sent to one. The field is set in one place, where a
receipt is made.

**What fits a device is one unit, for now.** The spec takes the local route when
"the job fits the user's device". How much a device can do is its hardware-fit
score, which nothing measures yet. Until something does, `submit` takes a device
to fit what the spec's tier table says any node can run: tier 0, a job of one
compute unit (`LOCAL_ROUTE_MAX_UNITS` in `crates/decomp-node/src/main.rs`).
`run` sets no limit, because there the user has said to use this device. The
owner has not yet confirmed this value.

## Known gaps

These are deliberate omissions at this step, listed so they are not forgotten.

| Gap | Why it is open | Closes with |
| --- | --- | --- |
| No automated tests | Tests are added only once the code is built, to guard against regressions. The flows were checked by hand. | Owner's go-ahead |
| No test vectors shared by Rust and TypeScript | Same reason. Until then, a change to the envelope format must be checked end to end by hand. | First test pass |
| The account key is the only controller | Passkeys, recovery keys, guardians, and email reset are build step 14. A device that loses its account key cannot be recovered. | Build step 14 |
| The command-line node keeps its keys in plain files | It has no access to the system keychain. On Unix the files are readable by their owner only. | When the command needs it |
| A job is not handed to another node if one goes silent | It waits for its deadline and expires; the silent node loses availability. A working node now checks in, but the coordinator does not yet use that to tell a node that has gone from one that is busy. | Re-assignment of jobs |
| Assignment draws on the coordinator's own randomness | The spec calls for a public source so anyone can recheck a draw. | With staked operators, build step 13 |
| The price of a unit is fixed at one credit | The spec lets the base price float with load and each tier carry a premium that moves with supply and demand. Both need load measurements that a test network cannot give. | When there is real traffic to steer |
| Disputes re-run a job from its start, and spot checks re-run all of it | The spec re-runs one segment between two checkpoints. That needs a job's state to be restored mid-run, which a compiled engine cannot do; an interpreter with saved state, or a proof system, could. A spot check therefore costs a whole second run. | Zero-knowledge proofs, or an interpreter built for it |
| A referee's verdict rests on one checkpoint | As the spec says, the node whose checkpoint matches the referee's is taken to be right. If two nodes are wrong in different ways, the one that went wrong later is believed. | A full re-run on every dispute, if that proves needed |
| Splitting cuts by byte count only, and only when asked | The spec's split rules follow the job's data types, and the coordinator also splits large jobs on its own to steer the mix of tiers. | Typed split rules, with the first job that needs them; steering when there is real traffic |
| No holdback | The spec holds a tenth of a spot-checked node's earnings for a day so a late check can be enforced. Checks here happen at the time of the job, so there is nothing to hold for. | If checks ever run after payment |
| Two of the four node scores | Bandwidth and hardware fit are not measured, and a node offers one slot. | Data-heavy jobs (step 7) and inference (step 12) |
| No penalty for proven fraud | Removal from the network and loss of the whole stake apply to offenses such as running a swapped model, which need inference to exist. | Build step 12 |
| Stake can be taken back between a wrong result and its verdict | Taking stake back is refused only while a node is on a job; refereeing takes seconds more. | A waiting period on taking stake back |
| The treasury only fills up | Starter grants are created rather than drawn from the treasury, and nothing is paid out of it. | Settlement, build step 13 |
| Starter credits can be turned into earned credits | An owner with several accounts can spend one account's grant on jobs run by nodes in the others, keeping 85% as credits that can be cashed out. Probation and stake raise the cost; nothing here prevents it. | Owner's call, before cash-out exists (step 14) |
| The coordinator handles one request at a time | All state sits behind one lock, including blob reads and writes. | When load needs it; revisit at build step 13 |
| A job that overflows the call stack may not fail the same way everywhere | Stack depth is measured in native stack bytes, which differ by processor type. Two honest nodes on different processors could disagree about such a job. | Before nodes on mixed hardware verify each other (build step 5) |
| Modules are compiled again on every run | The agent has no cache of compiled modules yet. Compiling is fast next to running a job. | When measurements show it matters |
| A torn final line in `ops.jsonl` stops the coordinator from starting | Acceptable for a development stand-in; the error names the line. | Build step 13 |
| A device behind a home router cannot be reached directly | The direct path needs the receiver to run a node that listens. Two devices that are both behind routers always go through a relay. | If relays turn out to cost too much |
| A stored file stays listed until its paid time ends | The receiving device remembers what it has collected. The sender is not told, and cannot end the storage early to get the rest of the fee back. | Build step 8, with the Send screens |
| A relay keeps frames in memory | A relay that restarts loses what it had not delivered and the pay it had not claimed. | When relays carry more than tests |
| A sender can make a relay take in data nobody collects | Up to 8 MiB a channel, for which the relay is not paid. Opening a channel costs the sender nothing if it is not used. | A small charge for opening a channel |
| Relays and storage nodes are picked at random | Bandwidth is not scored, and a relay that does not deliver loses its pay but not its standing. | With the bandwidth score |
| A node serves plain HTTP | A web page loaded over HTTPS is not allowed to call it, so a hosted web app cannot yet reach nodes. | TLS for nodes, before the web app is hosted |
| A receiver polls its relay | It asks twice a second, and each frame is its own request. Good enough for files; too slow for anything live. | Real-time apps |
| The storage and relay prices are not confirmed | 180 credits a gibibyte-day and 2,500 credits a gibibyte relayed are derived from the spec's compute price, not chosen by the owner. | Owner's decision |
| A blob's paid time running out has not been exercised | The shortest time that can be paid for is a day, so the end of a deal, the refund of what was not paid out, and nodes deleting the blob were not run. | A run that lasts a day, or tests |
| A lost copy is not replaced | When a node's copy is counted as lost, no other node is asked to take one. | With re-replication |
| A node's address is taken on trust | The coordinator does not check that a node can be reached where it says. A node that lies is never sent the blob, so it is never paid, but the blob ends up with fewer copies. | A reachability check at check-in |
| Job blobs are still held by the coordinator | Modules, inputs, and outputs do not yet go through node storage. | Later in build step 7 |
| The same blob cannot be stored twice | A second request for a root already stored is refused, even from another account. | When two accounts need the same data |
| Stored blobs are readable by anyone with the root | `store` keeps a file as it is. Sealing exists now, but who a stored file is sealed to is for the app that stores it to say. | Build step 8, when Send stores what waits for a device |
| Logs can fall out of step on a crash | A storage or relay change and the credits that go with it are written to two files, one after the other. | Settlement, build step 13 |
| An app's manifest lists no permissions or schemas | The one permission so far follows from the list of modules: only the app's own modules may be handed its state. Each module describes its own types. | With the first permission that cannot be read off the modules |
| What an app has set aside for an account today is kept in memory | A restart forgets it, so a daily cap can be passed once for each restart. It counts the highest possible fee of each job, not what the job cost. | Settlement, build step 13 |
| A daily cap limits an account, not a person | Accounts are free to make, so someone can get around an app's cap with new ones. What the app's account holds is the real limit. | Sign-in, build step 14 |
| A job seals to one device, not to a user | The spec's sealing "encrypts to a user's key". The sealing keys there are belong to devices, so what a job seals can be opened on one device, and a private job run by the local route opens only what was sealed to the device it runs on. | Keys that no single party holds, later in build step 9 |
| A follow-up job gets less than a submitted one | Its input is not checked against its module's types before it runs, so a wrong one fails when run. Nor is its module's privacy mode checked: it runs in the mode of the job that asked for it, and, if that job was a share, under the same name, so it stays off the accounts that hold the job's other shares. It cannot be split or handed state. | With the first app that needs any of these |
| A job reads and writes its whole shard | Every job on a shard fetches all its records and stores all of them again, which is why a shard is capped at 8 MiB. | A hash tree over the records, if apps need larger shards |
| Every past version of a shard is kept | Each is a blob on the coordinator, and nothing removes old ones. | With job blobs moving to node storage |
| Records have no declared types and cannot be removed | The SDK reads and writes them as JSON of whatever type the module asks for, and the spec lists no call to delete one. | With the first app that needs either |
| A job run on this device alone is handed no state | `run` has no way to name a shard. | When someone needs to try a stateful module off the network |
| A private job of more than one unit has no route unless it only adds up private numbers | Encrypted compute and the enclave route are not built. Such a job stops with `NoPrivateRoute`, or is run with `run`. | The other routes, later in build step 9 |
| The coordinator can read every share | Job blobs are still held by the coordinator, so the inputs of both shares, and both outputs, pass through one party that could add them. The spec's promise is about nodes, and it holds for nodes; the spec also has jobs reach nodes "through relays under one-time IDs" and private values stored "with keys that no single party holds". | Those two, later in build step 9, and job blobs moving to node storage |
| One owner with nodes on two accounts can be handed both shares | Shares go to different accounts, and accounts are free to make. The spec lists this as an open risk: "Secret sharing fails if one owner controls all the nodes holding a job's shares". | Stake and sign-in raise the cost; measuring how concentrated node ownership is |
| A share that is not run leaves the other paid for | The shares are jobs of their own. If one expires, is disputed, or is refused when submitted, the other still runs and is charged, and the device has no result. | A job of several parts that is paid for only as a whole |
| Which accounts hold a job's shares is forgotten with the shares' jobs | Jobs are kept an hour after they close and not across a restart. A share submitted later than that could go to an account that held another. The command-line node submits a job's shares together. | Settlement, build step 13 |
| A private total is of one device's numbers | The spec's use of this route totals numbers from many users, each sending shares of its own. Here one device submits the whole input. | The feed app, build step 11 |
| The number of shares is fixed at two | See "Two shares" above. | If the owner wants a job to choose |
| `submit` takes a device to fit one-unit jobs only | Hardware fit is not measured, so one unit, which the spec says any device can run, stands in for what this device can do. The owner has not confirmed it. | The hardware-fit score, build step 12 |
| A private job is not handed to another device of the account | The spec's local route also takes "another device on the same account". Only the device that was asked runs the job. | With a way to send a job to one's own devices |
| The manifest has no `allow_enclave` | The spec lets a private job allow the enclave route. That route needs attested nodes, and a field nothing reads would be a promise nothing keeps. | The enclave route, with attested nodes (build step 12) |
