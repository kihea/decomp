# DeComp design notes

This document records why DeComp is built the way it is: the choices the
specification leaves open, how each was settled, and the reasoning behind it.
It is meant for engineers changing the protocol, the control plane, the node
agent, or a client, who need to know which constraints a piece of code is
honoring before they change it.

[`docs/spec.md`](spec.md) is the source of truth. Where these notes and the spec
disagree, the spec wins and these notes are out of date. Quotations from the
spec are kept beside the decisions they justify.

Each entry starts with a one-line claim in bold, followed by the reasoning.
Questions the team has not yet settled are collected under
[Open questions](#open-questions) at the end.

## Requests and the control plane

**One signed-message format.** Every request is an envelope: a JSON payload as a
string, the signer's public key, and a signature over `"decomp/v1\n"` plus that
exact string. The verifier checks the bytes it received and only then parses
them. Because nothing is ever re-serialized, there is no canonical encoding to
keep in step across languages.

**No sessions.** The control plane has no logins or tokens. Reads are signed
too, because an account's device list is not public.

**Account changes are separate from every other request.** A change to an
account is an `AccountOp`, which is ordered and logged. Everything else is one
tagged message type, `Request`, defined once in `decomp-core` and used by both
the control plane and the node agent. On the devnet these arrive at
`POST /v1/ops` and `POST /v1/request`. Every request carries the time it was
issued and is refused if that is more than five minutes from the control
plane's clock, which bounds how long a captured request can be replayed.

**The rules live in `decomp-core`, not in the services.** Assignment,
verification, pricing, and settlement are pure functions of the requests, the
time, and a source of randomness, and `decomp-core` does no I/O. The canisters
and `decomp-devnet` call the same functions, so the local network behaves as
the deployed one does, and anyone can rerun a published draw or recompute a
batch with the code that made it.

**Logs are the database.** Every lasting part of the control plane's state
changes only by applying entries, so a log of entries is a complete copy. On the
Internet Computer each part lives in the stable memory of the canister that owns
it; on the devnet each is a log file in the data directory (`ops.jsonl` for
account records, `ledger.jsonl` for balances, `receipts.jsonl` for receipts, and
so on). Open jobs, leases, commitments, and nodes asking for work are kept only
in memory, as the spec's "Data management" intends. Credits set aside for jobs
that were open when the devnet stopped are returned at startup.

**Clients in other languages mirror `decomp-core`.** `client-ts/src/protocol.ts`
repeats the message shapes and the signing convention in TypeScript, and must
change together with the Rust. It holds no keys: a caller hands it a `Signer`, so
each platform keeps secrets in its own secure storage.

## Accounts and recovery

**A controller approves a device by signing two lines of text.** The text is
`decomp/v1/approve-device`, the account's ID, and the device's key. It names no
time and no place in the account's history, because guardians may take days to
sign and a device that is in no account cannot read the account's record. It can
still be used only once: a device key belongs to one account for good, whether
its recovery takes effect or is cancelled. The request that carries the
approvals is signed by the device itself, so nobody else can spend them. An
approval by a passkey or a wallet does not name its key, because a device that
is in no account cannot read the account's record to learn it: each is tried
against the account's passkeys or wallets. A request carries at most 24
approvals.

**Everything a controller approves waits.** The spec says "a passkey or any other
controller can approve a device the same way" as an approved device does, and
also "A recovery takes effect after 48 hours (starting value)". It lists
passkeys among the recovery methods the waiting period applies to: "Every
recovery method is also a way in for an attacker, so three rules apply". So only
the account key and approved devices add a device at once, and a device approved
by a passkey waits like any other. Signing in to a new device with a passkey
therefore takes two days. This is the reading that lets nobody in early.

**Only the account key changes controllers.** The spec's table of keys has the
account key sign "changing controllers", and names no other key for it. A device
that could add a controller could keep a way in after it was removed.

**A recovery key and a recovery service's key are one kind of controller.** The
spec stores "a public key" for one and "the public key of a recovery service" for
the other, and "DeComp knows only controller keys". Who holds the key is not
DeComp's to know.

**A passkey is checked as a WebAuthn assertion, and its site is not.** The
signature (ECDSA on P-256, in the form passkeys give it) must be over the
authenticator data and the hash of the client data; the client data must be of
the kind made when signing in, and its challenge must be the hash of the
statement; and the authenticator must say a person was present. The client data
also names the site the passkey was used on, and the authenticator data the hash
of the site's name. DeComp stores only "the passkey's public key", so it has no
site to compare with: which sites may use a passkey is the browser's rule.

**A wallet is a secp256k1 key that signs as Ethereum wallets do.** The spec says
"a crypto wallet" and "the wallet's public key" and names no chain. One scheme is
the least: the key Ethereum wallets hold, and the signature their
`personal_sign` gives, which is over the statement as a person can read it. The
record holds the public key, not the shorter address made from it.

**A guardian account signs with its account key or any of its devices.** The
spec stores "the guardians' account IDs", and whoever may act for an account
elsewhere may approve as that guardian. Two devices of one guardian count once.

**A recovery's time is in the request, not read from a clock.** Account records
are rebuilt by replaying requests, which must give the same records every time.
So the request that starts a recovery carries the time, and the request that
finishes it carries the time; the second is refused if it is before the first
plus the wait. The control plane refuses either when it first arrives if its
time is more than five minutes from its own clock, so a recovery can finish
early by that much and no more. The wait is a setting of the control plane
(`DECOMP_RECOVERY_WAIT_SECS` on the devnet, 48 hours by default), and a replay
of the account records must use the same value.

**Up to four recoveries of an account wait at once.** One at a time would let
whoever started first keep the account's owner out.

**An account's ledger address comes from the request that created it.** The
spec requires "No public balance under a user's identity ... a balance sits
under a separate ledger address that only the account's controllers can link to
that ID." The address is a hash that takes in the account key's signature on
the creating request. Nobody can work it out from the account's ID or key. The
account's controller can, and can show anyone the link by showing the request.
The control plane can too, since "The accounts canister holds each account's
record ... so jobs are billed to the right balance." Nothing had to be added to
what a client sends, and every account has an address from the moment it
exists.

## Jobs and verification

**Results are compared whole.** Nodes must agree on the units used and the
checkpoints as well as the output, so a node cannot inflate its bill. A job that
fails the same way on every node is a verified failure, not a dispute.

**A node commits a salted hash of its result before anyone reveals.** The
spec's lifecycle has a node commit "the hash of its output, and then" reveal
the result. The commitment hashes the result, the node's key, and a secret salt
of the node's choosing. Commitments can be read by anyone who reads the job
board, and without the salt a second node cannot copy a commitment it did not
compute. Results are revealed only after the confirmation window closes.

**A result is confirmed when a second node happens to pick it up.** The spec's
rule is "Verification follows supply". By default (`verify: opportunistic`) a
job is accepted after one result and confirmed if a second node commits within
the confirmation window. `required` waits for a second node until the deadline,
for results that change balances or other high-value state; `none` takes one
result and offers no confirmation, for work the app can check for itself. A job
may also set `fallback: always_on`, to be run in the control plane if no node
finishes it in time.

**A result accepted unconfirmed is open to audit, and part of its pay waits.** A
random 10% of unconfirmed results are re-run by another node as a spot check,
and 10% of the node's pay for each is held back for 24 hours so that a late
check can still be enforced ("Nodes on spot checks have 10% of earnings held for
24 hours"). An audit that finds the result wrong takes the held part.

**A node on probation is paid only once it is confirmed.** "New nodes start at
tier 0, and their results are paid only once a second pickup confirms them." So
a first result from a node on probation waits for a second pickup until the
deadline, as a `required` job does.

**A result nothing was compared with says so.** "A result that is unverified or
under dispute is flagged as such." Every receipt names one of four levels:
`confirmed` (a second node's result matched), `refereed` (results differed and a
checkpoint dispute named the right one), `always_on` (the control plane ran it
after no node finished in time), or `unconfirmed` (one node's result, accepted on
its reputation and open to audit). A job whose pieces were checked in different
ways reports the weakest of them.

**A draw is a rule anyone can work out again.** The spec names "the drand beacon
or ICP's verifiable randomness" as sources; drand can be reached from anywhere,
so it is the one used, on its "quicknet" chain. Draws are made per lease, not
per job: each `Pickup` (the first, a confirming second, or an audit) is drawn
separately. A draw's numbers are SHA-256 hashes of a seed and a count, and the
seed is a hash of the round's randomness, the job's ID, the pickup, and the list
of nodes that could be drawn with their weights. Nodes are picked with chances
in proportion to weight, from balanced accounts' nodes first. The rule is
`drawn` in `crates/decomp-core/src/draw.rs`, and the job board grants leases by
calling the same function a checker calls.

The list of nodes is part of the seed for a reason. A node knows its job's ID
and can fetch any round, so from those two alone it could work out a draw ahead
of time. It does not know who else is asking. For the same reason a draw is
published only once its job has closed.

**Whether a result is audited is drawn after the result is fixed.** The audit
draw uses the job's ID and the beacon round in which the unconfirmed result was
accepted. That round's randomness does not exist until after the node has
committed and revealed, so the node cannot know beforehand whether this job
will be checked, which is what the spec requires: a node "never learns which
jobs are checked, so honesty on every job is the only safe strategy". Anyone can
recheck the audit draw once the job has closed.

**Weights in a draw are whole millionths.** A checker must start from exactly
the numbers the draw was made with, and a fraction written as text does not
always read back as the same number. A draw's record lists only the nodes that
could be drawn, not the ones the job was kept from, which would show whose result
a job matters to.

**Only the draws the spec names come from the beacon.** Those are "the random
draws for assignment and spot checks". Which known-answer job a node is handed
and when, which relay carries a channel, and which chunk a storage challenge asks
for still use the control plane's own randomness. A known-answer job has to stay
unpredictable to the node it tests.

**A job's blobs are named before it runs.** The spec lets a job fetch any blob
by its hash. But whether a blob exists changes over time: a node that ran a job
before some blob was stored and a node that ran it after would both be honest and
would disagree, and the first would be judged wrong. So a job's manifest lists
the blobs it may read, each must exist when the job is submitted, and asking for
any other blob gives the same answer on every node: not given.

**Follow-up jobs wait for the verdict.** A job's request for more jobs is part of
its result, so it counts only once the result stands. Nothing a job that was
wrong, or failed, asked for is ever run. The follow-ups are funded from what the
job left of its own fee cap, so one cap, set by the submitter, bounds what a job
and everything it sets off can cost.

**A job is kept from accounts, and they are named by key.** The spec asks for
this of one app: "A player's own nodes are never assigned the zone that player
is in", and "The nodes simulating a zone change every tick." Both are rules about
which nodes may run a job, so they are built as two fields any job's manifest can
carry, `keep_from` and `apart_from`, and applied wherever a node is drawn, in
`jobs.rs`. Neither is shown to the node a job is handed.

An app knows a user by an account ID or, if the user chose, by one device's key,
and from a device's key nobody but the control plane can tell the account. So
`keep_from` takes keys, and the control plane finds the accounts: "The accounts
canister holds each account's record, including which devices belong to it." It
bars the whole account, because a second node of the same owner has the same
stake.

`apart_from` names jobs, not nodes, because a submitter is not told which
accounts ran its job. Independence is counted in accounts, as it is for the two
leases of one job: "A second lease on the same piece never goes to the same
account."

**A module counts its own instructions.** Before a module runs, the sandbox
rewrites it (`crates/decomp-sandbox/src/meter.rs`): it adds a counter that the
code subtracts from as it executes, and a call to the host when the counter runs
out. The count is a property of the rewritten module, not of the engine, so any
WebAssembly engine bills the same job the same way. An engine's own fuel counter
would tie the bill to one engine version and offer no way to pause a job at an
exact instruction count.

**Checkpoints are taken once per compute unit.** Each time a module's counter
runs out, every billion instructions, the host hashes the job's memory and
instruction count into a checkpoint, and a result carries the hash of its
checkpoint list. The spec's interval is "every 10 million instructions (starting
value)"; at that interval hashing memory would cost more than the job, so the
interval is a hundred times longer and equal to the spec's compute unit ("one
unit is 1 billion WebAssembly instructions").

**Everything that grows with data is billed.** Every byte a host function copies
costs one instruction, and so does every byte a module copies or fills with a
single bulk instruction, so a module cannot do unmetered work either way. What a
job hands back, its output and the blobs it stores, is also bounded by a slot's
memory, so a job cannot fill its host's memory for the price of one unit.

## Apps and state

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
form, named by its hash like any blob. The control plane keeps only which hash is
current for each shard. A job is told the hash when it is leased, every node on
it reads those same records, and the records it leaves are a new blob whose hash
is in its result. Running a shard's jobs one at a time means none has to be
redone because another got in first. The price is that a job reads and writes its
whole shard, so shards are small, 8 MiB at most, and an app with more state uses
more shards.

Records that devices write directly are a different tool. Each is a key, a
version, and the hash of a value, and "A write names the version it read and is
accepted only if that version is still current", so two writers cannot both win.
Each write is a call to the control plane, so high-volume state belongs in
fewer, larger records or in shards.

## Market and pricing

**Credits are whole millionths.** Amounts are integers of microcredits, so fee
shares are exact and the parts of a split always add up to the fee; the treasury
takes what rounding leaves. Premiums are whole thousandths for the same reason:
every party that applies the same events reaches the same prices.

**Rates are posted by formula, and locked when a job is submitted.** The base
rate is 250 microcredits per compute unit. Each class and tier has a premium
that starts at 1.0, rises when the tier has a queue, and falls when it has idle
nodes, held between 0.5x and 4x. The ceiling is the cost of running the same job
as an always-on fallback: "No rate rises above the cost of running the same job
in a canister." A job pays the rate posted when it was submitted, so a later
retune cannot change what its submitter already agreed to.

**Credits are set aside before a job runs.** The highest possible fee is held
from the moment a job is accepted: two runs at the whole compute cap at the
locked rate (one run for `verify: none`), or the fallback cost if that is more. A
result claiming more compute than the cap is refused, so settling a job can never
fail or overdraw an account.

**Each node is paid 85% of its own run's fee.** The spec splits a fee 85% to
nodes, 10% to the verification fund, and 5% to the treasury. Runs of one job can
be priced differently (a filled bid or a tapered account is paid less than the
posted rate), so each run is priced on its own and its node receives 85% of
that run.

**Share caps and the taper wait for a large enough tier.** "No account is
granted more than its cap of the leases in a tier", and above half its cap an
account's pay per unit falls to 70% at the cap. Both apply only once a tier has
100 active accounts: "Below that, every available node is needed."

**Balanced accounts are served first.** The spec serves lease requests first
from accounts that also spend credits in apps, and "pure providers fill what
remains", so the network does not come to depend on professional providers. An
account counts as balanced if it spent credits within the last 30 days, and a
draw picks from balanced accounts' nodes whenever any are eligible.

**A node owner chooses kinds of work, never a job.** Work preferences name job
classes, a tier range, the lowest rate the node works for, how many slots it
offers, and when it works. "Preferences describe kinds of work, never a job, a
module, or an app, so a node still cannot choose which job it confirms." The job
board applies what it can see; the agent applies hours, battery, and metered
networks on the device before it asks for work.

**Standing bids are taken in tiers 3 and 4 only, and only below the posted
rate.** "Jobs in tier 3 and above are also listed for bids." An account can take
its nodes out of standard allocation there and work only through bids. A bid
above the posted rate waits until the rate rises to meet it, so bids cannot
raise prices. The lowest matching bid fills a job's first pickup and is paid what
it asked, and the payer keeps the difference; the draw then only breaks ties
among the lowest bids. The confirming node is still drawn from standard
allocation.

## Storage and relays

**Data services are node work.** Storing, serving, and relaying data are done by
nodes and paid for in credits. DeComp does not lean on an outside peer-to-peer
network for them.

**Only reachable nodes hold or relay data.** A device behind a home router cannot
accept connections, so it cannot serve a file to anyone or pass one on. Nodes
that can be reached take the data work; every node can still do compute, which
needs only outgoing connections. The control plane stays out of the data path:
it decides who holds or carries what, checks them, and pays.

**Stored blobs are named by a hash-tree root over 1 MB chunks.** With a root, one
chunk can be proven to belong to the blob, so the control plane can check that a
node still holds a blob without holding it too. Modules, inputs, and outputs keep
their plain hashes.

**Placement is computed, not recorded.** A blob goes to the storage nodes whose
hash with the blob's name ranks highest (rendezvous hashing), each from a
different account. The spec asks that "Any client can compute where a chunk
lives, so there is no central index." Each node's place depends only on the two
hashes, so adding or losing a node moves few blobs, and anyone with the list of
storage nodes works out the same order.

**A device has a second key, for sealing.** A device signs with an Ed25519 key.
Data is sealed to a separate X25519 key, because a browser can keep a signing key
that cannot be used for anything else, and that is how a browser client keeps
its own. The device publishes the sealing key in a request signed with its device
key. The control plane keeps the signed request and hands it to anyone who asks,
so a sender checks the device's signature and does not have to trust the control
plane about whose key it is. Sealing itself uses X25519, HKDF with SHA-256, and
AES-256-GCM, all of which a browser does natively, so every kind of device seals
the same way.

**Asking is how a device shows it is there.** Only nodes ask for leases, so the
control plane cannot otherwise tell whether a device is around to receive. A
device that wants to receive asks, every second or so, what is addressed to it,
and that is taken as waiting for the next fifteen seconds. A channel is opened
only to a waiting device, so a sender finds out at once that it should use
storage instead, and no relay holds data for a device that may never come.

**A relay is paid on the receiver's signature.** Paying a relay for whatever the
sender handed it would pay a relay that threw the data away. So a claim needs two
signatures: the sender's, on a frame header that counts the bytes sent so far,
and the receiver's, on a request that says it has that frame. The receiver signs
as part of asking for more, so no extra message is needed. The most a relay can
lose to a receiver that takes data and does not sign is one answer's worth, four
megabytes.

**A node keeps asking for work while it works.** A job runs on its own thread
and the node keeps calling `request_lease` every two seconds. That call also
carries storage orders, storage challenges, and relay channels. Storage
challenges must be answered within a minute and a new channel must be picked up
at once, and a job can run far longer than either.

**Storage is priced by size and time, relaying by the byte.** The spec prices
storage by "Gigabyte-months held" and relay by "Gigabytes moved", and sets
neither rate yet. Before the usual fee split, a node is paid
`MICROS_PER_GIB_DAY` (in `crates/decomp-core/src/storage.rs`, 57,000) to hold
a gibibyte for a day and `MICROS_PER_GIB_RELAYED` (in
`crates/decomp-core/src/relay.rs`, 715,000) to relay a gibibyte. They match what
cloud storage (about 0.08 cents a gigabyte-day) and hosted bandwidth (about a
cent a gigabyte) cost at the credit's value: one credit buys about one
slot-hour, which the spec's economics put near 1.5 cents.

## Privacy

**What `seal` and `open` do inside a job, and why.** The spec lists both among
the host functions and says "Sealing encrypts to a user's key". Two things about
a job decide what they can be.

Every node must produce the same bytes, and a job has no randomness but what its
seed fixes. Ordinary sealing draws a new key pair for each message. Inside a job
that one-time key is worked out instead: it is the SHA-256 hash of a label, the
job's seed, the receiving key, and the message. Everything after that is ordinary
sealing, so a device opens the result as it opens anything else sealed to it. The
message is part of the hash for a reason. The cipher uses a fixed nonce, which is
safe only while each one-time key seals one message. A key made from the seed and
a counter would seal two different messages under one key whenever two runs had
the same seed, and every run on a device alone has the same seed.

Bytes that are fixed can be worked out again, and that is what sealing in a job
gives up. Someone who knows the job's ID and the receiving key, and can guess the
message, can seal the guess and compare. The nodes that ran the job had the
message to begin with, and anyone holding an open job's module and input can run
it again. So `seal` protects a message after the job, where it is stored and
carried, from those who never had the job's input. It does not hide anything from
the nodes that run the job. Hiding data from them is what the private routes are
for.

A node holds no user's opening key, and must not. So `open` is given no key by
the job: it opens with a key the device running the job was given for it. The
command-line node gives its own opening key to a private job it runs by the local
route, and to nothing else. Nodes, the control plane's referee, and an open job
run on a device alone are given none. There `open` answers "cannot be opened" for
any input, and charges the same, so every node agrees and results still compare.

Each costs a job what checking a signature does, and one instruction more for
each byte (`SEAL_INSTRUCTIONS` in `crates/decomp-sandbox/src/lib.rs`). That
figure is an estimate and has not been measured for sealing itself.

**A job's ID is one-time because it takes in the request's signature.** A job's
ID hashes the submitting device's key, the request it signed, and the request's
signature. Without the signature, a node, which is handed the ID and nearly
everything in the request, could hash a device's key with a rebuilt request and
see whether a device it suspected had submitted the job. The signature is fixed
by the request, so the same request sent twice still names one job and is refused
the second time, and nobody without the device's key can make it or guess it.
Random bytes in the request, kept from nodes, would also work, but would change
the request every client sends and give no more. The pieces of a split job and a
job's follow-ups get IDs made from the job's own.

A node still learns which app pays for a job and which app's state it works on,
where there is one. Those name an app, not a person, and a node holds the app's
module anyway.

**A module declares its own privacy.** The mode is part of the description a
module gives of itself, next to its types, because it is a property of the code:
a private job is one written with private types. The manifest repeats it, as the
spec's manifest does, and the control plane checks one against the other. So a
module written to be private cannot be run as an open job by mistake, nor the
other way round. A module that does not describe itself is open.

**The route rule is in one place and applied twice.** Which route a private job
takes is decided by `privacy` in `decomp-core`. The device that wants the job run
asks it first, so a job with no route is stopped before anything leaves the
device. The control plane asks it again about every job it is sent, because a
device could be wrong or could lie, and what the rule protects is on the control
plane's side: which jobs nodes are handed.

**A share is an ordinary job with a name on it.** "Shares and encrypted values
are ordinary bytes to a job, so duplicate runs and checkpoint disputes work in
both modes." So there is no second kind of job for shares. A share's job is
submitted, leased, checked, refereed, and paid for by the rules every job goes
by. The manifest's `share` field names the set of shares a job belongs to, and
the job rules add two things for a job that has one: it is not leased to an
account that was leased another job of the same set, and it is never reissued as
a known-answer job or an audit, because the node asked could be of such an
account. The name is the submitter's own: two accounts that use the same name do
not affect each other. A share is also refused a shard of state, since a job
could leave its share there and anyone can read a shard.

**Shares are numbers that add up, wrapping at 2^53.** Nothing else is needed for
adding and for weights that are not private, so no heavier scheme is used. All but
one of a number's shares are drawn from the operating system's randomness and the
last is what is missing from the sum. The sum wraps at 2^53 and not at 2^64
because a share is as likely to be large as small, and a whole number below 2^53
comes through every JSON reader unchanged, including one that keeps numbers as
64-bit floating point, as a browser does. A private number is therefore a whole
number from 0 to 2^53 - 1.

**Two shares.** The spec prices this route at "About 2x" an open job. Each
share's job costs what the job would cost open, so two shares is the count that
matches (`SHARES` in `crates/decomp-core/src/shares.rs`). More shares would keep a
number private from more accounts working together, at that many times the price.
With two, the two accounts that hold a job's shares can read it if they work
together.

**A job's types say whether it can run on shares.** A module's description names
each private number it takes and returns. A job whose private values are all
private numbers is taken to do nothing but add them and weight them, which is
what the types it is written with allow. The device cuts shares and adds results
by that description, and the control plane accepts a share only of such a job.
Privacy does not rest on the module keeping to this: its nodes are handed only
shares whatever it does. What rests on it is the answer, which is wrong if a
module does anything else with a share.

**A result by private totals is the same values, not the same bytes.** The device
reads the shares' outputs as JSON, adds, and writes the result out again, and that
writes a struct's fields in the order of their names. The local route gives the
module's own bytes. A result by encrypted compute is written out the same way,
once its private values have been read.

**A share is verified as any job is, and the receipt says how.** The spec's table
has this route "verified by duplicate runs on each share", and also says
"Verification is unchanged." Unchanged is what is built. Under the default
`verify: opportunistic`, a share is confirmed when a second node picks it up and
is otherwise accepted unconfirmed; `verify: required` makes every share wait for
a confirming node. Because a share is never reissued, a share accepted
unconfirmed is not spot-checked. The spec's "about 2x" holds either way, since an
open job is checked by the same rule.

**A job on the local route has no receipt.** Receipts are written by the control
plane, and a job that runs on its user's device never reaches it. The device
reports the five things itself: the route is local, checking is "not needed: this
device is its user's own", the cost is nothing, the time is what the device
measured, and no enclave was involved.

**`enclave` is reported before any enclave exists.** It is one of the spec's five
fields, and "no" is a true answer today: no node can show that it runs in an
enclave, so no job is sent to one. The field is set in one place, where a receipt
is made.

**What fits a device is one unit, for now.** The spec takes the local route when
"The job fits the user's device". How much a device can do is its hardware-fit
score, which nothing measures yet. Until something does, `submit` takes a device
to fit what the spec's tier table says any node can run: tier 0, a job of one
compute unit (`LOCAL_ROUTE_MAX_UNITS` in `crates/decomp-node/src/main.rs`). `run`
sets no limit, because there the user has said to use this device.

The cap `submit` is given is what the job may use on nodes. A job on encrypted
values needs thousands of units there and a fraction of one on its user's device,
where the values are plain, so a cap of one unit sends it nowhere and a larger
one sends it to nodes.

**Encrypted values are bits, encrypted with TFHE-rs.** The spec says only that
"Encrypted compute uses homomorphic encryption". DeComp uses TFHE-rs, the `tfhe`
crate, at version 1.8.2 exactly, on the device and inside the job module. It is
the homomorphic encryption library that is maintained in Rust and compiles to
WebAssembly, and its kind of encryption cleans a value up at every gate, so a
calculation can be as long as it likes: "Encrypted compute handles any
calculation in principle." DeComp writes no cryptography of its own. It calls the
library to make a key, to encrypt a bit, and to read one, with the library's
default parameters for bits, which the library documents as 132 bits of security
and a chance below 2^-64 that a gate gives the wrong bit.

A private number is encrypted as its 32 bits, and a true-or-false value as one.
What a module does with the bits is its own business. DeComp fixes how a value
travels and nothing else (`crates/decomp-core/src/encrypted.rs`).

**The sandbox was not changed for encrypted compute.** "Shares and encrypted
values are ordinary bytes to a job." The library is compiled into the module, so
computing on encrypted values is metered in compute units like everything else a
module does, and a job gets its key as one of the blobs it is given to read. No
host function was added.

The sandbox already gives every node the same result for the same module and
input. Inside it, the library has one thing left to vary unless a module settles
it: it picks a Fourier transform by timing several, and a module has no clock, so
a module must name the transform when it is built. With that done, two nodes'
signed reports of a job name the same output and the same checkpoints, and two
runs of a job in one sandbox give the same bytes.

**A key for each job.** The device makes a new key every time and keeps the part
that reads values in memory until it has read the result. Nothing is stored, so
there is no key to lose or to guard, and no two jobs can be seen to be one user's
by the key they came with. It also bounds a known weakness of this kind of
encryption: a node that returned values of its own making, and learned what the
user did with the result, could learn something about the key. With a key for
each job, that is at most a key that reads one job. The price is 13 MB sent with
every job, and about 51 compute units on every node to get the key ready.

**An encrypted value names its key.** A module reads a blob by its hash, so it has
to be told which blob holds its key. Every encrypted value starts with that hash.
A module needs nothing but the job's own input to find the key, and a value under
a key the job was not given fails the same way on every node.

**The control plane checks the form of an encrypted value, not the encryption.**
On this route it refuses an input in which a private value is a plain number or
`true` or `false`, which is what a device that forgot to encrypt would send. It
cannot tell whether base64 text in a private value's place was encrypted well,
and does not try: it holds no key.

**The size cap for encrypted compute is set from measurements.** The spec says:
"Benchmark standard encrypted operations and set the size cap from the results."
The reference measurements, on an AMD Ryzen 5 5500U with a release build, are:

| What | Compute units (billions of instructions) | Seconds |
| --- | --- | --- |
| Getting the key ready, once a job | about 51 | 8 |
| One gate between two encrypted bits | about 12.5 | 2.6 |
| The smallest job: two private true-or-false values, one gate | about 63 | 11 |
| The higher of two private 32-bit numbers: 191 gates | about 2,436 | 540 |

A gate is 12.5 billion WebAssembly instructions, where the same gate on plain
bits is a handful. That is far past the spec's "100 to 100,000x", which is a
figure for hardware built for the purpose: here the library runs in WebAssembly,
on one thread, with no vector instructions.

The same gates took 0.8 seconds each in the same WebAssembly engine without the
sandbox's instruction counting and checkpoints. So about two thirds of an
encrypted job's time is the sandbox's, which hashes the job's memory, key and
all, at every checkpoint.

`ENCRYPTED_COMPUTE_MAX_UNITS` in `crates/decomp-core/src/privacy.rs` is 2,560.
It allows about 200 gates: one addition, or one comparison and a choice, of two
32-bit numbers, about nine minutes on each node, at the far end of the spec's
"Network jobs finish in seconds to minutes".
Any job of 8 units or more is in tier 3 or above, which only staked nodes may
take, so every job on this route is.

## Settlement

**Bought credits are counted with earned ones.** The spec sets granted credits
apart ("Granted credits can be spent but not transferred or cashed out") and no
others, so a bought credit can be spent, staked, and cashed out, as an earned one
can.

**The treasury says that money moved.** Something outside DeComp must say that a
payment arrived or a payout was made. The key the control plane signs the
settlement log and draws with is also the treasury's, and only that key may
record a credit purchase or pay or refuse a cash-out. The name of the payment
goes into the credit record, one payment is credited once, and that is all that
ties a credit to money.

**A cash-out's credits are set aside, then removed or returned.** A cash-out must
be signed by the account key itself. The request names an amount and nothing
about where money should go: there is no route in the protocol for it to go by.
The request's signature names the cash-out, so the same request sent twice asks
once. Credits set aside for a cash-out still count as the account's on the
settlement log, as credits set aside for a job do, until the treasury records it
as paid.

**The settlement log holds what the ledger holds.** "The ledger records balances
and batched proof of the work done; it never sees individual jobs." Every 10
minutes the receipts service combines the period's receipts into one fingerprint,
the root of a hash tree over them, and posts it with the net change at each
ledger address. The log is a list of entries signed by the control plane, each
naming the hash of the one before, so an entry cannot be changed or removed
without every later one ceasing to fit. Anyone given the log can check it from
the first entry and ends up with every balance.

**Anyone can recompute a batch from its receipts.** "Anyone can recompute a batch
fingerprint from its receipts." A batch is checked by rehashing its receipts, not
by trusting a second party to audit the first. A node can also ask for a proof
that one of its receipts is in a batch: the receipt's exact text and the hashes
that link it to the fingerprint ("Any node can check that its own receipts are
included").

**A receipt is hashed as the text it was written as.** As with signed requests,
nothing is written out a second time to be hashed, so a proof hands over the
receipt's exact text and a checker in any language hashes what it was given.

**A batch's changes are what the records hold, less what the log holds.** The
control plane does not add up the credit movements of a period. It takes what
every holder has now and subtracts the balance the log has for that address. So
a batch can be made at any moment from the records and the log alone, and the
log's balances cannot drift from the control plane's.

**A balance on the log counts stake and credits set aside.** The spec's account
record has "`balance`: Credits and any stake", and credits set aside for an open
job, a data service, or a cash-out are still their owner's until they are paid
out, as is pay held back for a spot check. So putting up stake, taking it back,
and setting credits aside change no balance on the log. Paying for a job does,
and so does a forfeit.

## Hosts

**One library, two hosts.** The control plane's services and interface live
in `decomp-control`, which does no I/O. The devnet keeps its logs and blobs
in files and serves HTTP; the control canister keeps them in its heap, saves
them to stable memory across upgrades, and serves the HTTP gateway. Both
answer with `decomp_control::handle`, so a rule changes in one place.

**The canister's randomness is the network's.** Leases and audits are drawn
from `raw_rand`, fetched every few seconds (spec section 6: "Lease grants draw
on ICP's verifiable randomness"). A draw records its source; `decomp-node
draws` checks the rule for such a draw, since the randomness cannot be
fetched again, and checks drand rounds against drand. The control plane's own
generator (known-answer jobs, challenges, relays) is reseeded from the same
source.

**The canister runs jobs on an interpreter.** Disputes, fallbacks, and the
type check run on wasmi, which compiles into a canister; nodes run wasmtime.
Metering is part of the module, and both engines give NaN results the
canonical bit pattern, so they count and compute the same. An update call has
an instruction limit, so a dispute over a large job cannot yet be refereed in
the canister.

## Open questions

- **Passkey sign-in speed.** A device approved by a passkey waits the full
  recovery period, so signing in to a new device with a passkey takes two days.
  Should passkeys be allowed to add a device at once, as the account key and
  approved devices do?
- **Randomness source on the Internet Computer.** Draws use drand's quicknet
  chain. Section 4 of the spec allows "the drand beacon or ICP's verifiable
  randomness", but section 6 says "Lease grants draw on ICP's verifiable
  randomness". Should the deployed canisters switch to ICP's
  randomness, keep drand, or accept either in a draw's record?
- **Unconfirmed shares are never audited.** A share cannot be reissued as a spot
  check, so a share accepted unconfirmed is not checked later. Should
  private-totals jobs default to `verify: required`?
- **Local route fit.** A device is taken to fit one compute unit until its
  hardware-fit score is measured. Is one unit the right starting value?
- **TFHE-rs licensing.** The library's licence is BSD-3-Clause-Clear, which grants
  no patent rights, and its maker asks companies to buy a patent licence before
  using it commercially. This must be settled before the encrypted route is
  offered for money.
- **Cost of `seal` and `open` in a job.** They are charged as a signature check
  plus one instruction per byte. That figure should be measured.
- **The canister's signing key.** The settlement log and draws are signed with
  a key the canister derives at install and keeps in its state, which the
  subnet's node providers can read. The subnet's threshold Ed25519 signing
  should replace it before the canister holds real credits.
