# Point-to-point routing for linked queues

A companion to [queue.md](queue.md). That file describes the intended queue
design. This one covers two related things:

1. **the routing cost of a request/reply pair** — two linked queues used so that
   one node listens for an answer and exactly one other node writes it — and what
   it takes to make that route direct rather than cluster-wide;
2. **using the same route as transport for a requested process** — where a client
   asks for work to be *run* rather than looked up, and the answer is produced.

Code is referenced by file and symbol rather than by line number, deliberately:
line numbers in a published document are stale on arrival.

## The shape in question

A queue is one-way. A request/reply exchange therefore needs two:

```
A watches   /q/rpc-reply-{A}/consumer/{A}      reply queue, A is the sole consumer
A produces  /q/rpc/producer/{A}                request, tail carries A's id
B consumes  /q/rpc/{idx}/{A}                   B reads the tail
B produces  /q/rpc-reply-{A}/producer/{corr}   reply
```

Both queues are created implicitly — `get_or_create_queue` (`src/queue.rs`) on
the first put, `create_watcher` on the first consumer watch. Nothing has to be
declared.

The delivery semantics are already point-to-point and do not need changing:
`next_undelivered` only considers messages with `handled_by == None`, and
delivery sets `handled_by = Some(cid)`, so each message is owned by exactly one
client until it acks. What is *not* point-to-point is the **route**.

## Where it stands

| Piece | State |
|---|---|
| 1:1 delivery + ack | built |
| implicit queue creation | built |
| key rewrite `producer/` → `{idx}/` | built |
| dispatcher registry | **built** (Phase 1) |
| consumer→host registry | **built** (Phase 2) |
| queue linking / affinity | **built** (Phase 3) |
| co-location short circuit | **built** (Phase 5) |
| persistent peer stream | **not built** (Phase 4) |

Two registries were missing, and everything else followed from them.

> **As built.** `src/route.rs` (pure, 11 unit tests) plus the routing in
> `queue.rs` / `kv.rs` / `peer.rs`. Phases 1, 2, 3 and 5 are in; Phase 4 is not —
> its design is [§Transport for a requested process](#transport-for-a-requested-process)
> below, and §Phase 4 says why it is still right to leave last.
>
> **A prerequisite bug had to be fixed first, and it would have made the
> registries unusable.** `QueueNameKey` classifies *anything* under `/q/` or
> `/queue/` as a queue, so a put to `/q/{name}` or `/q/{name}/c/{client}` went
> through `get_or_create_queue` and was **enqueued as a message** — the registry
> would have filled the queue it exists to route. `route::is_control` now
> separates them **structurally**, straight from `queue.md`: a delivery is
> `/q/{name}/c/{cid}/{idx}/{key}` and carries an idx and a tail; a registration
> carries neither.
>
> **`Option<EtcdPeerNodeType>` was the wrong type, not just an unwritten field.**
> `None` meant *both* "I am the dispatcher" and "nobody has been elected", so even
> once something wrote it the fallback would still have been ambiguous. It is now
> `route::Dispatch` — `Local`, `Remote(id)`, `Unknown` — and `Local` is what makes
> Phase 5 a case rather than a special case.
>
> **The cost table below is asserted by a test**
> (`route::tests::the_cost_model_holds`), not left as prose, so a change that
> quietly reintroduces the broadcast shows up as a number.

## Cost model

`N` = cluster size, `nA` = the node A is connected to, `nB` = B's node. Cost is
counted in peer RPCs per round trip; `peer_request` (`src/peer.rs`) is a unary
call per message.

| Stage | Request leg | Reply leg | Round trip |
|---|---|---|---|
| broadcast fallback (before Phase 1) | N−1 | N−1 | **2(N−1)** |
| registries, dispatcher at neither end | 2 | 2 | **4** |
| **dispatcher at an endpoint** (as built) | 1 | 1 | **2** |
| co-located (`nA == nB`) | 0 | 0 | **0** |

Row three is what the endpoint rule buys, and it is reached on the **first**
watch rather than after a hysteresis. Row two is what you get only when the
dispatcher is a third party — a node with neither the producer nor the consumer —
which the claim rules below avoid by construction.

The first row is the one that hurts: a round trip on a 9-node cluster costs 16
peer RPCs to move two messages between two nodes, and every node stores a copy of
both until they are acked.

## Plan

### Phase 1 — dispatcher registry

Land what `queue.md` already specifies: the queue key itself (`/q/{q_name}`, the
`queue`-but-neither-producer-nor-consumer case that `QueueNameKey` already
classifies) holds the **dispatcher node id** as its value, replicated by the
ordinary KV broadcast.

- On first touch of a queue (`get_or_create_queue` or `create_watcher`), read
  `/q/{q_name}`. If unset or naming a node that is not `Online` in `EtcdCluster`
  (`src/peer.rs`), attempt to claim it by writing this node's id, then re-read to
  confirm — last-writer-wins is acceptable here because a wrong winner costs an
  extra hop, not correctness.
- Populate `Queue::dispatcher` from that value and refresh it on change.
- Keep the broadcast fallback, but make it reachable only when the registry read
  fails. It is the safety net, not the default path.

This alone takes the request leg from N−1 to 2.

**Watch for:** `queue.md` already flags that a producer not connected to the
dispatcher can lose a message if the dispatcher dies after accepting and before
indexing. Phase 1 makes that path the common one rather than the rare one, so it
needs the ack-before-index ordering settled at the same time.

### Phase 2 — consumer→host registry

`Queue::clients` is a plain in-process `HashMap<ClientId, EtcdClientType>` —
node-local. No node knows where another node's consumers are, which is exactly
why the fallback has to broadcast. `queue.md` specifies the fix:
`/q/{q_name}/c/{client_id}` with the **client's host node** as its value.

- On `make_consumer`, publish that key with this node's id, via the normal
  replicated put so every node — and therefore the dispatcher — can resolve it.
- On watcher drop (`drop_watcher`, called from the dispatch loop), delete it.
- The dispatch loop builds `candidates` from local `clients` only. Extend it to
  include remote consumers, resolved through the registry, and send those a
  *command* to the owning node rather than the payload — `queue.md`: "no network
  data call, only command".

Request leg is now `nA → D → nB`, two hops, independent of `N`.

### Phase 3 — linked-pair affinity

Two hops per leg is still one more than necessary, because the dispatcher is
placed arbitrarily. For a *linked pair* it can be placed deliberately.

- **Declare the link.** Extend the `/q/{q_name}` value from a bare node id to a
  small record: `{dispatcher, reply_to}`. A queue that names a `reply_to` is half
  of a pair, and the two are scheduled together.
- **Pin the reply dispatcher to the requester's node.** The reply queue has
  exactly one consumer — A — and A's node is known from Phase 2. Setting
  `D_reply = nA` makes dispatcher and consumer node the same, so the reply leg
  collapses to a single hop `nB → nA`.
- **Pin the request dispatcher to the consumer's node** when the request queue has
  one stable consumer, giving `nA → nB`. With several consumers, fall back to the
  Phase 1 placement — affinity is an optimization, never a constraint on where a
  consumer may live.
- **Hysteresis.** A consumer that reconnects to a different node would otherwise
  drag the dispatcher with it and churn the registry. Only move a pinned
  dispatcher after the consumer has been stable on its new node for `PIN_STABLE`,
  and never while messages are outstanding and unacked.

Round trip is now 2 RPCs, from `2(N−1)`.

### Phase 4 — persistent peer stream  *(not built)*

`queue.md` proposes `/{q_name}/producer@{node_id}` with a connection URL, the
dispatcher opening a watcher stream into the peer. Worth doing after Phase 3, not
before — it reduces the cost *of* a hop rather than the *number* of hops.

The motivation is measured: on this stack roughly half of a round trip is wire and
half is gRPC/HTTP2 and async scheduling, with protobuf serialization a rounding
error. For a hot pair, replacing per-message unary calls with one long-lived
bidirectional stream removes the per-call preamble. Note also that `peer_request`
takes `node.kv_client.lock()`, so concurrent messages to the *same* peer already
serialize on that mutex — a hot p2p pair is exactly the case that contends on it.

The full design, and the use case that motivates building it,
is [§Transport for a requested process](#transport-for-a-requested-process).

### Phase 5 — co-location short circuit

`queue.md` notes "all might be on same node". Made explicit: when producer node,
dispatcher and consumer node are the same, the message never touches the network.
Worth an assertion in the dispatch loop, because it is the case a local
development setup and a single-node deployment always hit, and a regression there
is invisible until it is measured.

## As built: the dispatcher is an ENDPOINT

The correction that shaped the implementation, and it is worth stating before the
phase table because it changes what "p2p" means here:

> **p2p is: whichever node picked up the consumer(s) and created the watch queue,
> or the node the producer was created on. One of those dispatches. The route is
> then a direct dispatch (same node) or one direct p2p node connection.**

A dispatcher placed at *neither* end is a third party and costs **two** hops —
`producer node → dispatcher → consumer node`. Placed at the consumer's node it
costs **one**, and one for *every* producer at once. So:

| claim | rule |
|---|---|
| `Claim::Consumer` | this node hosts a consumer. Claims immediately, and **takes over** from a dispatcher that hosts none |
| `Claim::Producer` | this node has a producer. Claims only an **unclaimed** queue — taking one from a consumer's node would move it away from the end that benefits most |

That placement is **not** the Phase 3 hysteresis. `PIN_STABLE` exists for
*relocating* an established dispatcher after a consumer moves; a consumer's node
claims the moment it creates the watch queue, with no delay. Waiting for a
stability interval to reach the correct placement would have made the common case
the slow one.

Two consumers on two nodes leave the first in place: there is no single right
answer then, and `pin_to_consumer` deliberately declines to pick one — affinity is
an optimization, never a constraint on where a consumer may live.

| Phase | Where |
|---|---|
| 1 dispatcher registry | `Queue::elect(Claim, ..)` — read `/q/{name}`, adopt if the named node is **Online** *and* an endpoint, otherwise claim and re-read to confirm. Called from `create_watcher` (Consumer) and `put_impl` (Producer) |
| 2 consumer→host | `Queue::publish_consumer` / `withdraw_consumer` / `remote_consumers`; the dispatch loop tries every local watcher first, then unicasts a delivery key to the node the registry names |
| 3 relocation | `Queue::pin_to_consumer`, run when the queue drains; `EtcdNode::queue_link` (`src/cluster.rs`) declares a pair |
| 5 direct dispatch | `Dispatch::Local` in `Queue::put` — nothing leaves the node |

Three details worth keeping:

**`elect` claims for *this* node rather than picking a candidate.** Choosing
"best" would need agreement about what best means, and last-writer-wins already
settles a race — at the cost of one extra hop for the loser's traffic until the
registry converges, never a lost or duplicated message. Delivery ownership is
`handled_by`, which none of this touches.

**A re-election preserves `reply_to`.** The link is a property of the queue
*pair*, not of whichever node happens to dispatch it, so losing it when a
dispatcher dies would silently undo Phase 3.

**The registry write does not go through `put_impl`.** `Queue::elect` calls
`EtcdNode::put_kv` directly. That breaks a genuine `put_impl → elect → kv_put →
put_impl` cycle, but the reason it is right is independent of the compiler: a
registry write is by definition not queue traffic and should not be asking the
function that decides whether something is a message.

---

## Transport for a requested process

The case: a **client asks for a process to be run** — a model completion, a job,
anything where the answer is produced rather than looked up. The worker is a
client on some other node. This server is neither the compute nor the store; it is
the **transport**, plus the directory that says where to send things.

```text
requester ─stream─▶ node A ═══stream═══▶ node B ─stream─▶ worker
 client             dispatcher           consumer host    client
        ◀──────────────── response ────────────────────
                 ▲                     ▲
                 └── /q/{name} ─────────┘  catalog: who dispatches,
                     /q/{name}/c/{cid}     and where each consumer lives
```

The KV entries are a **catalog, not the message path**. A put says *where* a thing
goes; the payload travels over a stream. `route::is_control` is explicit that
these keys are not queue traffic, and separates them structurally rather than by
convention: three segments for the dispatcher record, five for a consumer
registration — a *delivery* has an idx and a tail as well.

### What a process demands that queue delivery does not

Queue delivery is one message in, one message out. A process is not:

1. **The response is a stream of many, for one request.** Tokens, progress,
   partial results. The reply leg is not a message on a queue, it is a sub-stream
   bound to one request.
2. **There is a silence before the first output.** The worker is computing, not
   idle, and no timer can tell those apart from outside — against a TTL of a dozen
   seconds that silence may be most of the request's life.
3. **It can be cancelled.** The requester gives up, the connection drops, the job
   is superseded. Something has to tell the worker to stop, and free the binding.
4. **Exactly one worker must take it.** Running a process twice is not a duplicate
   message, it is duplicate cost.

Point 4 holds **while a job is bound**: `next_undelivered` only considers messages
with `handled_by == None`, and delivery binds `handled_by = Some(cid)`. It does
not survive a deadline expiring — see *At-most-once* below, which is the only part
of this section that changes an existing guarantee rather than adding to it.
Points 1–3 are what the rest of it is about.

### The middle leg is not a stream

The client legs are already bidirectional —
`rpc Watch(stream WatchRequest) returns (stream WatchResponse)`
(`proto/rpc.proto`). The middle leg is one unary KV put per message
(`Dispatch::Remote` → `peers.unicast(BroadcastRequest::Kv(..))`), because
`EtcdPeerNode` holds only `kv_client`, `mt_client` and `cluster_client`. A grep
for `WatchClient` across the crate returns nothing: no node has ever opened a
stream to a peer.

### Decisions

| | |
|---|---|
| **Who dials** | the **dispatcher** dials the consumer's node |
| **Granularity** | **one stream per queue**, opened lazily, closed when idle |
| **Relay shape** | a **separate remote-delivery path** in the dispatch loop |
| **Backpressure** | on stall past `DISPATCH_TIMEOUT`, **drop the stream, fall back to unary** |

#### The dispatcher dialling means a new RPC

Reusing the client `Watch` only works if the **receiving** side dials, because the
dialer on a Watch stream is a subscriber: it sends `WatchRequest`, and that is a
oneof of create / cancel / progress with **no payload field**. A dialer cannot
push over it.

So the delivery stream is a new peer-facing bidirectional RPC — dispatcher sends,
receiving node acks on the same stream:

```proto
rpc Deliver(stream DeliverRequest) returns (stream DeliverAck)
```

`Cluster` (`proto/rpc.proto`) is the natural home: already the peer service, and
every peer already holds a `ClusterClient`, so no new connection appears. A fourth
client on `EtcdPeerNode` is nearly free — all three are built off one
`conn.clone()`.

The stream ack is **not** the queue ack. The queue ack stays the consumer deleting
`/q/{name}/c/{cid}/{idx}/{key}`, which retains the indexed line out on the
dispatcher. The stream ack only says this node took it.

#### Idle close, against a short deadline

Lazy opening and idle closing collide, and the process case breaks the tie.

The scale settles it. A request's TTL is on the order of **a dozen seconds** —
this is a short-deadline RPC, not a long-running job — so the whole exchange is
over inside one idle interval, and the question is not whether a stream survives a
long silence but whether it is worth opening at all for something that brief.

It is, because the alternative is a connection preamble on every delivery, and at
this deadline the preamble is a visible fraction of the budget. But the idle
interval should be set in **tens of seconds to a minute** — several exchanges long
— so a queue in steady use never closes between requests, and a queue that has
genuinely stopped releases its stream promptly.

The resolution is that these are two different streams and only one of them is
idle-closable:

- **Request leg** (dispatcher → consumer host): per queue, lazily opened, idle
  close. Deliveries are short and bursty; this is the traffic the decision was
  made for.
- **Response leg** (worker → requester): closed by completion, cancellation,
  failure, or **the request's own TTL running out** — never by an *idle* timer.
  Silence is not evidence of death when the work is compute, so the declared TTL
  is the only honest bound, and at a dozen seconds it is a tight one. That is why
  it belongs in the envelope rather than in node config.

Set the request leg's idle interval well above `DISPATCH_TIMEOUT` so a stall is
diagnosed before an idle close can mask it. Treat it as a tunable, not a constant:
a queue bursty on a period near the timeout will churn open/close, which is worse
than either extreme.

**`DISPATCH_TIMEOUT` itself needs revisiting at this scale.** It is a fixed 5s
(`src/queue.rs`), which against a dozen-second TTL is roughly **40% of the entire
request budget** spent waiting for one consumer to accept. A message can therefore
be declared timed-out by the transport while most of its life was spent in a
single accept wait. It should be derived from the TTL remaining on the message in
hand — accept must not be allowed to consume the budget the work still needs —
with the 5s constant kept only as a ceiling for traffic that carries no TTL at
all.

#### Stall, and why not the alternatives

Mirror the rule the local path already follows: the dispatch loop gives a local
consumer `DISPATCH_TIMEOUT` to accept and drops it otherwise. The peer stream gets
the same deadline; on expiry it is dropped and that queue reverts to
`peers.unicast(..)` per message, re-opening on the next delivery.

**Condition the fallback on the failure kind.** Falling back assumes the stall was
congestion. If the send failed because the payload exceeds a message limit, the
unary path sends the same bytes and fails identically — an unbounded retry loop
that reads from outside as a merely slow queue. A size or encoding rejection must
fail the message to the requester, not retry it on another transport.

Deliberately **not** `Dispatch::Unknown` + re-election. That is right for an
*unreachable peer* because the route is wrong. A slow worker is not a wrong route,
and re-electing moves a replicated registry key to fix something that is not a
routing problem.

Deliberately **not** blocking the queue. Holding a queue on one slow consumer is
what the old `iter().next()` behaviour did by accident, and `src/queue.rs` records
the cost: a dead entry handed back forever wedged the queue for good.

**Count the fallback.** A silent downgrade from streaming to unary reads as
"works, just slower", which is the failure nobody reports. It belongs with the
other queue counters, wherever the embedding application surfaces them.

### The request envelope

The producer's value is a JSON envelope carrying execution config — deadline, size
limits, and whatever else the process needs to be run correctly. Four rules, and
the first is the one most often got wrong.

**Deadline is a relative TTL, decremented per hop.** The usual advice is an
absolute instant, because a duration naively copied forward restarts at every hop.
That advice assumes the nodes agree on the time, and this server has no
time-synchronisation requirement — so an absolute deadline would be exact in form
and wrong in practice, with jobs expiring early or never and no signal that a
clock was the cause.

TTL is therefore the deliberate choice, and it carries one obligation that has to
be met everywhere or it fails silently: **every hop decrements by measured elapsed
time, including the time the message sat in the queue.** A hop that forwards
without decrementing extends the deadline, and nothing downstream can detect that
it happened. Two rules follow:

* decrement at *dequeue*, not at receive — queue residence is the largest term and
  the easiest to forget;
* a TTL that reaches zero in transit is failed where it is found, not forwarded for
  someone else to notice.

The error accumulates across hops. That is the accepted cost, bounded in practice
because the path is short — ingress, dispatcher, consumer — but note that at a
dozen-second TTL the error is proportionally *larger*, not smaller: the same few
milliseconds of sloppy measurement that vanish against an hour are measurable
against twelve seconds. Measure honestly at each hop rather than rounding in the
generous direction.

**Version the envelope.** `DispatchRecord` already got this right — a bare node id
is a valid record, so a Phase-3 node reads a Phase-1 registry without a parse
error. The request envelope is replicated the same way and will be read by both
old and new nodes during a rolling upgrade. Unknown fields are ignored, never
fatal.

**Capability is matched at ingress.** Consumers publish what they can run
alongside their host in `/q/{name}/c/{cid}` — the record is already replicated and
already extensible — so the producer's node matches the envelope against it and
refuses synchronously rather than paying for a hop to be nacked. A worker nack
remains the backstop for what a registry cannot express or has not yet caught up
with; it is the exception, not the routing mechanism.

**Enforce limits at ingress.** A size cap checked only at the worker has already
been paid for: enqueued, replicated, forwarded, streamed. Validate on the
producer's node, before `get_or_create_queue`, where rejecting is a cheap
synchronous error to the caller rather than an orphaned message somewhere in the
cluster.

**The ceiling is node config; an envelope may only lower it.** A request can
declare a smaller limit for itself and never a larger one, so no caller can talk a
node into accepting more than it was configured for. A node's memory is bounded by
that node's own configuration, which is the only place it can be bounded honestly.

**There is nothing to enforce against yet.** A prerequisite rather than an aside:
`QueueMsg.value` is a `Vec<u8>` held in an unbounded in-memory `VecDeque`, tonic's
`max_decoding_message_size` / `max_encoding_message_size` are never configured
anywhere in the crate, and no queue depth cap exists. A declared size limit is
currently a promise the transport cannot keep. Both caps — message bytes and queue
depth — have to land before the envelope can meaningfully reference either.

### Accept, reject, and running twice

An envelope a worker can read is an envelope a worker can **refuse** —
unsupported model, config it does not implement, a size over its own limit. Today
the dispatch loop knows only accept, stall and drop. A refusal is none of those:

- a **stall** is the consumer being slow, and the watcher is dropped;
- a **reject** is the consumer being wrong for this job. The message must clear
  `handled_by` immediately and be offered to a *different* consumer, never retried
  into the one that just refused it.

Without that distinction a reject looks like a stall, the worker is dropped as
dead, and a job that one node simply could not run takes a healthy consumer out of
the pool with it.

#### At-most-once, and being honest about it

"Exactly one worker takes each job" holds while a job is bound. It does **not**
survive a deadline: if a declared deadline expires and the message is redelivered,
a first worker that was merely slow rather than dead is now running the same
process twice — which was the thing point 4 said must not happen.

These cannot both be true, so pick one and say so:

- **at-most-once** — an expired deadline fails the request to the requester and
  does not redeliver. Redelivery happens only on *proven* worker death: its
  consumer registration gone from `/q/{name}/c/{cid}`. The caller retries if it
  wants to, which it can decide with context the transport does not have.
- **at-least-once** — redeliver on deadline expiry and require workers to be
  idempotent. Cheaper to implement, and wrong for anything whose cost is the
  execution itself.

Given point 4 states duplicate execution is duplicate cost, **at-most-once** is
the consistent choice, and the deadline is a failure trigger rather than a retry
trigger. This is the same question as the dead-queue TODO in `src/queue.rs` and
should be settled once, for both.

### Correlation, and the one-segment limit

The requester has to say where the answer goes, and the worker has to know which
request an output belongs to.

The request key's **tail survives the move** — `put` rebuilds it as
`<fq_name>/<idx>/<tail>` — so one token of routing rides along for free. But
`tail` is a **single path segment**: anything after the next `/` is silently
dropped. A uuid fits; a reply path does not.

So: correlation id in the tail, the rest — reply address, parameters, anything
structured — in the **value**. And one reply queue per *client*
(`/q/rpc-reply-{client}`) rather than per *request*, because nothing removes a
queue from `EtcdNode::queues`: no TTL, no reaper, and it is not on the TODO list.
Per-request reply queues leak one `Queue` per request for the node's lifetime.

### Cancellation

Nothing today expresses "stop". The pieces that exist:

- the requester can delete its consumer key, which is how an ack works;
- `handled_by` binds a job to one worker but is never cleared except by the ack
  retaining the message out;
- `src/queue.rs` has an open TODO for a dead queue — redelivering a message whose
  consumer died.

A cancel is the same problem seen from the other end — both are "this binding is no
longer valid" — so design it with that TODO rather than separately. What happens
next is not an open question: *at-most-once* above already answers it. A cancelled
or deadline-expired job **fails to the requester and is not redelivered**; only a
worker proven gone, its registration absent from `/q/{name}/c/{cid}`, puts a
message back in the pool.

What is missing is the signal itself. A cancel has to reach a worker that is
mid-process and therefore not reading the request leg at all, which means it
belongs on the **response** stream — the one already open to that worker for this
job — travelling the other way. That is the concrete reason the delivery RPC is
bidirectional rather than server-streaming, and it is worth writing down before the
stream is built, because it is cheap now and a protocol change later. Until it
exists, an abandoned process holds its `handled_by` binding and its output stream
until the declared deadline expires.

### Where it lands in the dispatch loop

A **separate remote-delivery path**, so routing stays visible where it happens:

```text
candidates
  ├─ local  → Sender<Result<WatchResponse, Status>>      (unchanged)
  └─ remote → peer delivery stream for (queue, node)     (new)
```

`candidates` currently collects only local `queue.clients`; it grows a second
source resolved through the consumer registry. Two things must stay in step across
both paths, and both have subtle history:

- **Timeout and drop.** Local is `DISPATCH_TIMEOUT` then `drop_watcher`. Remote
  needs the same, or a dead peer wedges the queue the way a dead local watcher used
  to.
- **Exactly-once binding.** `handled_by = Some(cid)` is set on the dispatcher
  before the message leaves, by both paths identically.

And the invariant not to break: **the dispatcher holds the only copy.**
`src/queue.rs` records why — once the registry let every node see every consumer,
keeping a local copy while forwarding made both the producer's node and the real
dispatcher deliver, so the consumer got the message twice and it leaked, because
the ack deletes by idx on the dispatcher only.

### Build order

1. `Deliver` RPC on `Cluster`; a `DeliverClient` on `EtcdPeerNode` off the existing
   channel.
2. Dispatcher side: open on first remote delivery, keyed by (queue, node); idle
   close on the **request leg only**; stall → drop and fall back to `unicast`.
3. Receiving side: accept, hand to the local consumer through the existing
   `WatcherConsumer` sender, ack on the stream.
4. Dispatch loop: the remote branch, sharing timeout, drop and `handled_by` with
   the local one.
5. Response leg: a sub-stream bound to a request id, closed by completion,
   cancellation, failure or the declared deadline — never by an idle timer.
6. Counters: streams open, deliveries streamed, falls back to unary, idle closes,
   rejects, deadline expiries.

Prerequisite, before any of the above is worth doing: **the caps have to exist.**
`max_decoding_message_size` / `max_encoding_message_size` on the server and on
every peer client, and a bound on queue depth. Until then the envelope can declare
a size limit that nothing enforces.

---

## What must not regress

- **Zone scoping — honoured.** `EtcdCluster::unicast` applies the **same** zone
  test as `broadcast_scoped` before sending, and returns `false` rather than
  sending when it fails; the caller falls back to a broadcast and logs it. Being
  targeted rather than broadcast changes nothing about which peers may legitimately
  receive a key. The rule behind it: `broadcast_scoped` excludes a peer whose zone
  is unknown from a zone-scoped broadcast, deliberately — a peer added since the
  last reconfigure is unlabelled, and treating "no label" as "same zone" is the
  leak zoning exists to prevent.

  A **stream** has to apply the same test **at open**, and re-apply it if zones are
  reconfigured underneath: a long-lived stream outlives the check that authorised
  it, which a per-message path never had to consider.
- **Peer state — honoured.** `elect` treats a record naming a non-`Online` peer as
  unusable and re-elects, and `unicast` refuses a peer that is not `Online`.
  `InProgress` deliberately does **not** qualify: such a peer receives writes but
  is still syncing, and a dispatcher has to index and route rather than merely
  store. Only `Online` peers count toward quorum; `InProgress` receive writes
  without counting, `Spare` are skipped.

  **`PeerState::Spare` here is this server's own liveness state.** A peer is
  `Spare` because it timed out or has not synced — nothing more. An embedding
  application may have its own notion of node roles, and a node that is
  permanently ineligible for some role *there* is still a completely ordinary peer
  *here*: it may host a consumer, dispatch a queue and produce. The two senses of
  the word are unrelated, which is why this is stated rather than assumed.
- **Ack semantics.** Nothing in this document changes `handled_by` ownership or the
  delete-on-consumer-key ack. Redelivery after a dead consumer is a separate TODO
  in `src/queue.rs`.

## Open

- **Idle interval for the request leg.** Argued above as a tunable; no default
  proposed.
- **Consumer moves while a stream is open.** Phase 3 hysteresis governs moving a
  pinned dispatcher; nothing says what happens to an open stream when
  `/q/{name}/c/{cid}` changes node under it.
- **Ordering across the fallback.** Dropping a stream mid-queue and reverting to
  unary must not let a unary message overtake one still buffered in the stream.
- **Backpressure on the response leg.** A requester that stops reading while a
  worker keeps producing has no defined behaviour yet. This is the one place where
  blocking, rather than dropping, is probably right — the worker should slow down,
  not lose output. Note this is the *opposite* rule from the request leg,
  deliberately: a dropped delivery can be re-sent, a dropped token cannot.
- **Proving every hop decrements the TTL.** The choice of a relative TTL trades
  clock agreement for an obligation on each hop, and a hop that forgets is
  undetectable from downstream. This wants a test that walks a message through
  ingress, a forward and a delivery and asserts the TTL strictly decreased at each,
  rather than trusting review to catch a missing subtraction.
- **Capability registry staleness.** Capability lives beside the consumer host at
  `/q/{name}/c/{cid}`, which already exists and whose record is extensible, so
  ingress can match an envelope and reject synchronously. The open part is
  freshness: a consumer that changes what it can run, or moves, leaves a record
  that routes confidently to the wrong place. The consumer registry has the same
  problem today and the same answer will do for both — but a stale *host* costs a
  retry, while a stale *capability* costs a job accepted that cannot be run, so the
  tolerance is not the same.
- **Queue teardown.** Nothing removes a queue from `EtcdNode::queues` — no TTL, no
  reaper, and it is not on the TODO list. A reply queue per *client* is bounded and
  is the shape this document assumes; if per-request reply queues are wanted,
  teardown becomes a prerequisite rather than a nicety.
- **Consumer filters.** Without them, a *shared* reply queue hands each reply to
  whichever consumer accepts first, not to the one it was addressed to. That is why
  the pattern above uses one reply queue per client. Filters would allow a single
  shared reply queue and remove the per-client queue entirely, which is a smaller
  cluster-wide footprint than any routing change in this document.
