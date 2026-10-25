# agent.md - etcds

A guide for coding agents (and people) who use this crate or change it. Each
statement below was checked against the source in `src/` for version 0.21.1.
The client side of a queue is in [queue.md](queue.md), the ClickHouse queues in
[clickhouse.md](clickhouse.md), and the routing between nodes in
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
| Watch | create (one key or a range), cancel, progress. No start revision, no filters, no fragments |
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
cargo install etcds                          # or --features clickhouse
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
exception: an empty one keeps the endpoint already set.

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
  whose index it carries. Delivered but unacknowledged messages stay with the
  dispatcher, marked `handled_by`.
- **The dispatcher follows a lone consumer.** With exactly one consumer, on
  another node, online and in place for 30 s (`PIN_STABLE`), and nothing
  outstanding, the dispatcher moves to that node.
- **A wake-up is only a wake-up.** The channel value is not matched to a message:
  each wake drains everything undelivered, oldest first.

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

- **No redelivery.** A message handed to a consumer that dies before
  acknowledging it is never handed out again (`queue.rs`, TODO "dead queue").
- **A forwarded message cannot be acknowledged on the dispatcher's node.** A
  producer on another node sends the dispatcher its own key, `/q/{name}/p/{key}`.
  The dispatcher keeps that key (`Queue::put`, the `from_peer` branch of `key`)
  and a consumer on its own node receives it as is. That key has no index, so the
  consumer's delete removes nothing: the message is not delivered twice, but it
  stays in memory and the queue is never reapable. A consumer on another node is
  not affected - its delivery key is rebuilt with the index.
- **The registry is replicated once.** The dispatcher record and the consumer
  entries are broadcast when written and loaded at a node's start; a node that was
  unreachable at that moment does not know them until they are written again.
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
  the dependent's requirement (ytserv `^0.21.1`, rppd `^0.21`), so a `0.x` minor
  bump here goes with a matching change of those requirements - otherwise cargo
  warns that the patch was not used and builds the crates.io release instead.
- Log lines start with `[etcd] ` (`LP`); queue routing ones add `[q-route]`.
- **Messages go through `tr!("module.key", name = value)`** (`src/i18n.rs`), the
  binary's through `etcds::message`; each key in all six `locales/<language>.yml`,
  on the same line in each, grouped by module. An error's message starts with a
  new `ETCD-Ennn` code (E001-E009 cli, E010 cluster, E030 peer, E050 queue, E070 clickhouse, E080 srv, E100
  main). The i18n tests fail on a key missing in a language, a placeholder that
  differs, a reused code, or a message nothing uses.
