# agent.md - etcds

A guide for coding agents (and people) who use this crate or change it. Each
statement below was checked against the source in `src/` for version 0.21.4.
The client side of a queue is in [queue.md](queue.md), the ClickHouse queues in
[clickhouse.md](clickhouse.md), the bridge to JMS, MQTT and Kafka in
[bridge.md](bridge.md), and the routing between nodes in
[queue-p2p-route.md](queue-p2p-route.md).

## What it is

`etcds` is an etcd v3 API **server**, on tonic. Any etcd v3 client talks to it
unchanged - the `etcd-client` crate, `python-etcd3`, a Go client. It does two jobs
on that one API:

- a **key-value store with watches**, replicated to every peer, for discovery and
  configuration;
- a **queue** under the keys `/q/` and `/queue/`: ordered, one consumer per
  message, routed between nodes - and, with the `clickhouse` feature, a queue that
  writes its messages straight into a ClickHouse table.

It is not etcd, and the differences decide what it can be used for:

- **Memory only.** Keys live in a map in memory, and a queue's messages in its
  dispatcher's memory. Nothing is written to disk: `data_dir`, `wal_dir`,
  `snapshot_count`, `heartbeat_interval` and `election_timeout` are accepted and
  ignored. A starting node loads the keys of the first peer that answers
  (`announce_and_sync`); a cluster that stops entirely starts empty.
- **Replication is a broadcast, not consensus.** A put or delete is applied on the
  node that took it and then sent to every peer, and the last write a node
  receives wins. There are no revisions: a watch starts from now, and `compact`
  answers and does nothing.
- **Peers must share a cluster uuid.** With `cluster` empty, a node invents one of
  its own, no peer accepts it, and its store is local to that node - it says so
  at start, at error level.

It runs as the standalone `etcds` binary (`src/main.rs`) or inside another tokio
program: YT Cluster's server (ytserv) and rppd embed it, ytserv on its own gRPC
port.

## Supported API

| service | state |
|---|---|
| KV | put, range (one key, or a range or prefix, with limit and count only), delete (one key or a range), txn; `compact` is a no-op |
| Watch | create (one key or a range), cancel, progress. A put is a PUT event; a delete - by key, range or txn - a DELETE event with the key and no value, except under `/q/` and `/queue/` (the registry is the server's). No start revision, no filters, no fragments |
| Cluster | `member_add` (how peers join); the rest is minimal |
| Maintenance | `status` (a reachability probe) |
| Lease, Lock, Election, Auth | registered, answer `unimplemented` |

## Keys

| key | what it is |
|---|---|
| anything outside `/q/` and `/queue/` | an ordinary key: stored, watched, replicated as the prefix policy says |
| `/q/{name}/p/{key}`, `/queue/{name}/producer/{key}` | a message put on queue `{name}`: queue traffic, never stored as a key |
| `/q/{name}/c/{uuid}`, `/queue/{name}/consumer/{uuid}` | watched: a consumer of `{name}`. Put by the server: the node hosting that consumer |
| `/q/{name}` | the queue's dispatcher record, `<node>[;reply_to=<queue>][;pinned]` (`route.rs`) |
| `/q/{name}/{idx}/{key}`, `/q/{name}/c/{uuid}/{idx}/{key}` | a delivered message, as the consumer receives it; deleting it acknowledges |
| `/q/ch:{table}/...`, `/queue/clickhouse:{table}/...` | a ClickHouse queue (feature `clickhouse`) |

`route::is_control` tells a registry key from queue traffic by its shape: three
segments, or five with `c`/`consumer`, is a registry key; everything else under
`/q/` is traffic.

## Running it standalone

```shell
cargo install etcds                          # or --features clickhouse, bridge
etcds --config node1.yml                     # default: config.yml in the working directory
```

The file is YAML whose keys are the field names of `EtcdConfig` (`src/cli.rs`);
the same settings are flags (`clickhouse_url` is `--clickhouse-url`). A flag given
on the command line wins over the file; a setting in neither takes clap's default.
A missing file is not an error - the flags and defaults stand - but a file that
does not parse is refused at start, naming the file (exit code 11).

The settings that do something:

| setting | default | what |
|---|---|---|
| `node` | random | this node's id, a UUID. Set it: an id that changes at each start leaves dispatcher records naming a node that no longer exists |
| `cluster` | random | the cluster's UUID, the same on every node, or nothing peers |
| `term` | 1 | the node's epoch |
| `listen_client_urls` | `localhost:2379` | where the gRPC server binds (the first URL), `host:port` or `http://host:port`; either way this node is left out of its own peer list (`EtcdConfig::peers`) |
| `initial_advertise_peer_urls` | | every node's URL, this one included, comma separated: the peers to connect to. Peer traffic uses the same gRPC port as clients |
| `local_prefix`, `local_prefix_except` | | key prefixes that never leave the node, and branches of them that do (`prefix.rs`) |
| `local_only_prefix`, `local_only_prefix_except` | | prefixes served only to a loopback caller, and branches served to anyone |
| `readonly_prefix` | | prefixes no API call may write: only the embedder writes them, in-process |
| `zone`, `zone_prefix`, `peer_zones` | | this node's zone, prefixes replicated only within a zone, and each peer's zone as `host:port=zone` |
| `clickhouse_url`, `clickhouse_db` | | the ClickHouse queues' endpoint and default database ([clickhouse.md](clickhouse.md)) |
| `bridge_queue`, `bridge_prefix`, `bridge_listen`, `bridge_to`, ... | | feature `bridge` ([bridge.md](bridge.md), `src/bridge/`): the node's queue or key prefix to a broker (JMS over AMQP 1.0, MQTT 5, Kafka), or JMS clients served as an AMQP peer; Started once the node serves, over its first client URL; stopped with it. Settings that make no bridge: exit code 14 (E104, E105 without the feature); a bridge that fails for good stops the node: exit code 15 (E106); one not stopped within `bridge_send_timeout` + 10 s of the signal: exit code 16 (E107). The binary's only: an embedder calling `EtcdNode::init` gets no bridge from these settings |
| `queue_down_timeout` | 300 | seconds a queue consumer's node may be down before its consumers count as gone (`Queues`); 0 never. An `Option`: unset is 300, also from `Default` |
| `log_level` | `info` | `error`, `warning`, `info`, `debug` or `trace`. Anything else - `warn` included - logs critical messages only |
| `max_txn_ops`, `max_request_bytes` | 128, 1.5 MiB | stored, not enforced |

Every other field is etcd's, accepted for compatibility and unused.

## Embedding it

| call | what |
|---|---|
| `EtcdNode::init(cfg, log[, tracer])` | builds the node, connects to its peers and loads their keys. The `tracer` argument exists with the `tracer` feature |
| `node.serve()` | binds `listen_client_urls` on a tonic server of its own, in a spawned task |
| `node.add_all_services(router)` | adds the etcd services to **your** tonic router instead, to share a port |
| `kv_put`, `kv_get`, `kv_delete`, `kv_prefix` | in-process KV. Writes follow the prefix policy and are replicated like a client's; there is no remote caller to check, and `readonly_prefix` is writable here |
| `new_watcher(requests, responses, &uuid)` | an in-process watch stream, the same protocol as the gRPC Watch: send `WatchRequest`s, read `WatchResponse`s. An in-process queue consumer is one of these |
| `queue_link(request_q, reply_q)` | declares a request/reply pair, so the reply queue's dispatcher is placed on the requester's node |
| `queue_dispatcher(q)` | the node dispatching `q`, from the registry |
| `reap_idle_queues()` | drops queues that are empty, consumerless and idle for 10 minutes. **Nothing in the crate calls it:** an embedder that creates queues calls it on a timer |
| `node.event` | a `Sender<EtcdEvents>`: `Data` applies a put, delete or txn; `Mgmt` reconfigures (`Config`), sets the tracer or meter, `Restart`s (term + 1 and a fresh sync), `Pause(secs)`, `Stop` |

### Language

Messages - errors, log lines, gRPC status texts, `--help` - are in en, ru, fr,
de, zh and ja (`locales/<language>.yml`, one file each; fr, de, zh and ja not
reviewed). The language is
the first of `etcds::set_lang("ru")`, the `lang` value of the logger given to
`init` (`o!("lang" => "ru")`), `LANG`, English. It is the process's, not a call's.
An error starts with its code, `ETCD-Ennn`, the same in every language: match
on the code, never on the text - a client of the gRPC API gets the server's
language.

A `Config` reconfigure replaces the peer URLs, the prefix policy, the zones, the
name, the term and the log level from the config it carries - send a whole
config, not a diff. Peers are only added, never removed. `clickhouse_url` is the
exception: an empty one keeps the endpoint already set; so is
`queue_down_timeout`: unset, it keeps the one set.

## Queues

What a client does is in [queue.md](queue.md). What an embedder or a change to
`queue.rs` has to keep in mind:

- **One dispatcher per queue holds its messages.** It is elected through the
  `/q/{name}` record (last writer wins, then confirmed by a read): a node hosting a
  consumer claims it and takes it over from one that hosts none; a producer's node
  claims only an unclaimed queue. Other nodes forward a put to the dispatcher and
  keep nothing. With no answer from the registry a put is broadcast, and the node
  hosting the consumer delivers it.
- **Delivery is a watch event.** The dispatch loop hands the oldest undelivered
  message to the first local consumer that accepts it within 5 s
  (`DISPATCH_TIMEOUT`), then to a consumer on another node by one unicast. A
  consumer that fails or stalls is dropped.
- **An acknowledge is a delete of the delivered key,** and removes the one message
  whose index and key it carries (`acknowledges`). A dispatcher's indexes start
  at the time it started, in microseconds (`first_idx`), so a late acknowledge
  from before a restart or a move of the dispatcher names no newer message.
  Delivered but unacknowledged messages stay with the
  dispatcher, marked `handled_by` and with the stream they went to (`stream`).
- **A registration names a stream:** `/q/{name}/c/{client}` holds
  `<node>;g=<generation>`, `;draining` added for a stream that receives no more
  (`route::Registration`). Each connection of a client to a node is a stream with
  its own generation; generations grow with time, so a client that moved or
  connected again has the newer one. `;s=<incarnation>` says when the writing
  node's process started: a stream of that node with an older generation is
  from before a restart, dead, and what it held goes back
  (`Registration::outlives`). A bare `<node>` - an older node's - is generation
  0; an older node routes nothing to a value with fields.
- **What a closed stream was handed is handed out again** (`release_stream`):
  on its own node at once, and on every dispatcher by the stream's tombstone
  (`<node>;g=<generation>;closed`, never stored), whatever the client registered
  since - read or unread in that stream, it is the line's again. A tombstone is
  taken only from the node it names, and removes that stream's registration or
  an older one, never a newer; that node's registration of that stream arriving
  after it - the two crossed - is not stored. A delivery to another node names
  its stream (the put's `lease`), and the host hands it to that stream only.
- **A delivery is marked before it is sent,** and the mark taken back on a
  refusal: a stream can close, and its release run, before the send returns. A
  delivery to another node that gets no answer may have been carried out: it
  stays marked for that stream, never handed out on a guess. The host gives its
  consumer 2 s (`REMOTE_DISPATCH_TIMEOUT`), well under the dispatcher's 5 s for
  the call, and drops a slow one in a task the call's end does not cut off.
- **A stream that stalls or is cancelled is not gone:** it is registered as
  draining, gets nothing new, and keeps what it was handed until it closes -
  watched without holding it open. Never on a timeout: a slow consumer is not a
  gone one, and a job is not run twice.
- **The dispatch loop's own check** (`release_gone`): a message held for a
  stream here that is neither receiving nor draining goes back, and one held for
  a stream on another node goes back once the registry no longer places the
  client on that node.
- **A consumer dropped as slow is told:** a `canceled` frame with the reason
  (ETCD-E052), and its watch is taken as a cancel takes it, so it can watch again
  under the same id.
- **A closed stream proves a consumer gone,** so connections are probed:
  `serve()` sets HTTP/2 and TCP keepalive (30 s, `KEEPALIVE`). An embedder that
  mounts the services on its own server (`add_all_services`) sets keepalive
  there, or a consumer whose host died or was cut off holds what it was handed
  until its node is down past the down timeout.
- **So does its node down past the down timeout** (`queue_down_timeout`, 300 s;
  0 never). Each node asks every peer its `status` every 20 s, and notes from any
  call whether it answered (`EtcdCluster::probe`, `down_for`); the dispatch loop
  withdraws, on its behalf, the stream of a consumer whose node has not answered
  for longer, by a tombstone marked `;gone` that any node takes, and hands what
  it held to the next consumer - at least once: a node only cut off may run it
  too. A node back within the time keeps them. Nothing is routed to a node that
  does not answer now. A node the named one answers does not take its `;gone`
  (it was cut off from the writer, not from it); and a peer that answers again
  after it was down is a join both ways (`probe_peers`): each writes its
  registry, and announces itself so the other writes its own - a consumer
  withdrawn as gone meanwhile is registered again.
- **The dispatcher follows a lone consumer.** With exactly one consumer, on
  another node, online and in place for 30 s (`PIN_STABLE`), and nothing
  outstanding or waiting, the dispatcher moves to that node.
- **A wake-up is only a wake-up.** The channel value is not matched to a message:
  each wake drains everything undelivered, oldest first. While a queue holds
  messages the loop also looks again on its own, 1 s after a wake-up and up to
  30 s apart after that (`REDISPATCH`).
- **The registry is written again when a peer joins** - admitted (also at start,
  by `connect`), promoted, or announcing itself with `member_add`
  (`EtcdNode::republish_registry`): each node writes each stream it holds as it
  is, never over a newer registration of another node (the client moved there);
  withdraws, by tombstone, a registration naming it that it no longer holds - one
  a restart copied back from a peer included, and, for a peer it connects late,
  one that peer still holds (`withdraw_stale_at`); writes again the tombstones of
  streams closed in the last 10 minutes (`QUEUE_IDLE`); and writes the dispatcher
  record where it names that node and a consumer is there. A node announces
  itself to a peer it connects late as it does at its start, so that peer writes
  its registry here too. A registration write is read back and written again if
  a stream changed meanwhile (`sync_registration`); a stream of this node it no
  longer holds gets its tombstone before it is written over; a value it holds
  already goes to the peers only, so a watch on it here hears nothing.
- **A peer held Spare gets no writes - the registry neither;** promoted
  (`member_promote`), it gets this node's whole current part of the registry:
  its registrations written again, and what the peer still places on this node
  and this node no longer holds withdrawn there (`withdraw_stale_at`) - however
  long it was Spare. Each node does so for its own part, the only part it can
  vouch for.
- **A rolling upgrade routes between versions.** A node answers `status` with
  `rust etcds/<version> registry/2`; an older node answers `rust`
  (`peer::registry_of`). It is asked at each connect and each probe - the
  cheapest signal every version gives. While any member reads the older
  registry, a node writes a registration as the bare `<node>` and withdraws it by
  a delete (`legacy_registry`); it reads an older node's bare `<node>` as
  generation 0, start unknown - never older than a stream it knows, so the end
  of the upgrade hands out nothing whose consumer lives. Meanwhile what a closed
  stream held on another node's dispatcher goes back only once its client has no
  stream left on that node; a stream closed then is remembered, and its
  tombstone written once no older member is left - when the next probe writes
  the registry in full.
- **A key watcher that does not take a notification in 2 s is cancelled**
  (`NOTIFY_TIMEOUT`, ETCD-E085), as a slow queue consumer is: its watch taken
  off, told why once its stream has room. Writes do not wait on it, nor on the
  `already exists` answer to a watch id used twice, sent with no lock held and
  as bounded. A slow consumer or watcher is told once, however often its stall
  is seen.

## ClickHouse queues

Feature `clickhouse`, details in [clickhouse.md](clickhouse.md). A queue named
`ch:{table}` or `clickhouse:{table}` has no consumer: at each wake-up its
dispatcher sends every message it holds to ClickHouse as one
`INSERT INTO db.table FORMAT JSONEachRow` over HTTP (`src/clickhouse.rs`, hyper's
client, 30 s timeout), and drops what landed. A failure keeps everything for the
next wake-up. With no `clickhouse_url` the messages just stay queued.

## In YT Cluster

YT Cluster's server, ytserv, embeds this crate (as `etcd`, with the `tracer` and
`clickhouse` features in its default build) and is its main user. What it sets,
and what it builds on top - **keep this section in step with ytserv**: a change
there to how it uses etcd is a change here.

- **One port.** The etcd services join ytserv's own gRPC server
  (`add_all_services`) on the node's port, 7421 by default - there is no 2379 and
  no HTTP gateway. A node serves them only with `serve_etcd` on (node level).
- **Peers from the cluster.** ytserv reconfigures etcd (`EtcdMgmtEvent::Config`)
  with every node of the cluster in `initial_advertise_peer_urls` and its own URL
  in `listen_client_urls`, and puts the node roster into the KV for discovery.
- **The language.** ytserv gives etcd its own language at its start (`set_lang`):
  the system locale's, `en` without one - one language for the node's messages.
- **The secret mirror.** With `skv_secrets` on, `/secret/` is `local_prefix`,
  `local_only_prefix` and `readonly_prefix` at once: each node's own copy of the
  secret store, served to a local caller only and written by ytserv alone.
- **ClickHouse queues.** ytserv passes `clickhouse_url` only on a node with
  `serve_clickhouse` on, so a `ch:` queue inserts only when its dispatcher's node
  has it. The same names mean different things on the two paths:
  - an empty `clickhouse_db` is `default` for a `ch:` queue, but the PostgreSQL
    schema name for ytserv's own on-commit sink (`on_commit_clickhouse`);
  - `clickhouse_batch_rows`, `clickhouse_batch_ms` and `clickhouse_queue`, and the
    sink's retry (1 s doubling to 60 s), are the sink's alone: a `ch:` queue has no
    batch size, no timer and no limit;
  - turning `serve_clickhouse` off does not stop a `ch:` queue until the node
    restarts (an empty URL in a reconfigure keeps the old one).
- **etcd into Postgres, `etcd_upsert`** (cluster level; ytserv's
  `state/etcd_upsert.rs`, nothing in this crate). Entries
  `<event>=<schema.table>:<column>[,<column>]`, separated by `;` or new lines: the
  messages of a queue, or the puts under a key prefix, are JSON objects (or arrays
  of them) upserted into the table through `jsonb_to_recordset ... on conflict
  (<columns>) do update`, one transaction per value, on a connection of its own.
  - A queue: every node registers an in-process consumer (`new_watcher`), so the
    dispatcher hands a message to its own node's first.
  - **The acknowledgement follows the cluster transaction.** A value is upserted
    in one transaction whose COMMIT is a cluster commit
    (`yt_cluster.auto_complete`), and its message is acknowledged when that
    commits. A rollback that may pass - the database away, a cluster
    inconsistency such as `40001` - is not acknowledged: the node retries it, 1 s
    doubling to 60 s, since etcd will not hand the message out again. Today one
    case breaks the rule: a value Postgres refuses as it is (not JSON, SQLSTATE
    class 22 or 23) is rolled back, logged, counted rejected - and acknowledged.
  - **Planned, not built yet: failures part ways, and none is acknowledged by
    default.** A configuration or syntax error in a value (it can never go in as
    it is) goes to a dead-letter queue; a cluster inconsistency is retried.
    Without a dead-letter queue - the default - nothing that failed is
    acknowledged. A route that wants failed messages acknowledged must name a
    dead-letter queue: the message goes onto it first, and is acknowledged
    after.
  - A prefix: the leader watches it and applies the puts in their order; a delete
    is not applied, and puts during a change of leader may be missed.
  - Its state is in `yt_info('E')`. End to end: `topology/93b_etcd_upsert.sh` in
    yt-cluster-tests.
  - The forwarded-message gap below is the common case here: a message put
    through any node but the dispatcher's is acknowledged and applied, but stays
    in the dispatcher's memory.
- **rppd** (in ytserv-py) consumes queues for Python functions through the same
  in-process watch API.

## Known gaps

Read from the source; each is a place where the obvious assumption is wrong.

- **A consumer's node down for less than the down timeout keeps what it was
  handed,** and after it the messages may run twice (at least once).
- **A forwarded message cannot be acknowledged on the dispatcher's node.** A
  producer on another node sends the dispatcher its own key, `/q/{name}/p/{key}`.
  The dispatcher keeps that key (`Queue::put`, the `from_peer` branch of `key`)
  and a consumer on its own node receives it as is. That key has no index, so the
  consumer's delete removes nothing: the message is not delivered twice, but it
  stays in memory and the queue is never reapable. A consumer on another node is
  not affected - its delivery key is rebuilt with the index.
- **A peer demoted to Spare stays Spare** until `member_promote`: a write it
  failed demotes it, and nothing in etcd promotes it - that is the cluster
  membership's call, the embedder's. It gets no writes meanwhile; promoted, it
  misses nothing of the registry.
- **No messages survive a restart** of their dispatcher's node.
- **A ClickHouse queue retries only on the next put,** and one row ClickHouse
  refuses makes every later batch fail with it ([clickhouse.md](clickhouse.md)).

## Changing the crate

- **Unsafe code is forbidden** in the library and the binary
  (`#![forbid(unsafe_code)]` in `lib.rs` and `main.rs`). `cargo make audit` and
  `cargo make geiger` (`Makefile.toml`) run the dependency checks; CI runs both on
  every push and weekly (`.github/workflows/audit.yml`).
- **The protocol is generated** from `proto/` by `build.rs` (tonic-prost-build)
  into `src/etcdpb/`.
- **Locks:** never hold `peers.read()` across a network wait (the `[lockup]`
  comments), and never hold a `queues` guard while creating a queue - both have
  wedged a node before.
- **Registry writes go through `put_control` / `put_kv`,** never through
  `kv_put`: that is the path that decides whether a key is a message, and a
  registry write is not one.
- **Tests:** `cargo test` runs the unit tests in `src/` and the integration tests
  in `tests/`, which start a node in-process (`tests/testing.rs`) and talk to it
  with `etcd-client`. End to end, over the network: `topology/93_etcd.sh` and the
  pipeline tests in [yt-cluster-tests](https://github.com/DBinvent/yt-cluster-tests),
  with this repository's `yxetcd` example as the client
  (`cargo build --release --example yxetcd`).
- **Versions:** ytserv and rppd build against this checkout through
  `[patch.crates-io]` path patches. A patch applies only while its version meets
  the dependent's requirement (ytserv `^0.22.1`, rppd `^0.22.1`), so a `0.x` minor
  bump here goes with a matching change of those requirements - otherwise cargo
  warns that the patch was not used and builds the crates.io release instead.
- Log lines start with `[etcd] ` (`LP`); queue routing ones add `[q-route]`.
- **Messages go through `tr!("module.key", name = value)`** (`src/i18n.rs`), the
  binary's through `etcds::message`; each key in all six `locales/<language>.yml`,
  on the same line in each, grouped by module. An error's message starts with a
  new `ETCD-Ennn` code (E001-E009 cli, E010 cluster, E030 peer, E050 queue, E070 clickhouse, E080 srv, E100
  main, E104-E107 the bridge; the bridge's own log lines are English, bridge.md). The i18n tests fail on a key missing in a language, a placeholder that
  differs, a reused code, or a message nothing uses.
