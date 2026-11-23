# agent.md - etcds

A guide for coding agents (and people) who use this crate or change it. Each
statement below was checked against the source in `src/` for version 0.21.4.
The client side of a queue is in [queue.md](queue.md), the ClickHouse queues in
[clickhouse.md](clickhouse.md), the bridge to JMS, MQTT, Kafka and HTTP(S) endpoints in
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
| KV | put, range (one key, or a range or prefix, with limit and count only), delete (one key or a range), txn; `range_stream` is the same range in one chunk; `compact` is a no-op |
| Watch | create (one key or a range), cancel, progress. A put is a PUT event; a delete - by key, range or txn - a DELETE event with the key and no value, except under `/q/` and `/queue/` (the registry is the server's). No start revision, no filters, no fragments. Every frame carries the response header (cluster, member, term; revision 0) - etcd's Go client reads it unchecked, and etcdctl crashed on a frame with none [watch-header]. A stream whose requests end is served on while its watches hold it (half-closed, as etcd does); its key watches are taken off once no one reads it either [watch-gone] |
| Cluster | `member_add` (how peers join); the rest is minimal |
| Maintenance | `status` (a reachability probe, open to any caller; `leader` filled, raised alarms in `errors`), `alarm` (kept on this node: GET, ACTIVATE, DEACTIVATE), `hash`, `hash_kv`, `snapshot`; `defragment` and `move_leader` answer and do nothing; `downgrade` answers the version (VALIDATE, CANCEL) and refuses ENABLE - nothing on disk to downgrade |
| Lease | grant (the minimum TTL 5 s), revoke, keep-alive, time-to-live (with its keys), leases - on the leader ("Leader, leases and locks") |
| Lock | lock, unlock - on the leader |
| Auth | feature `auth` with `auth_db_url`: signon's accounts ("Auth"); otherwise `unimplemented` |
| Approvals | no service of their own: puts and deletes on `/approval/` and on a gated queue's producer key (feature `approvals`, "Approvals") |
| Election | registered, answers `unimplemented` |
| Sequences | no service of their own: a queue pair (`etcds-seq`, a reply queue of the asker's), "System records and sequences". etcds serves etcd's v3 API only: `.github/proto-compare.sh` fails on any difference from etcd's files and on any proto or generated package of etcds' own |

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
| `bridge_queue`, `bridge_prefix`, `bridge_listen`, `bridge_to`, ... | | feature `bridge` ([bridge.md](bridge.md), `src/bridge/`): the node's queue or key prefix to a broker (JMS over AMQP 1.0, MQTT 5, Kafka) or an HTTP(S) endpoint (one POST per message), or JMS clients served as an AMQP peer; Started once the node serves, over its first client URL; stopped with it. A `bridge_prefix` holding a key the node serves to no remote caller (`local_only_prefix`, its exceptions aside), or a `bridge_queue` it serves to no remote consumer, is refused at start, exit code 14 - the bridge would carry it out of the node; a listener's link to such a queue is refused as it attaches. `amqps://`, `mqtts://` and `kafkas://` take `?ca=<PEM file>`, the only CAs then trusted (else the public roots); `amqps://` is TLS from its first byte (bridge.md "TLS and logins"). An embedder running `etcds::bridge::run` itself may set `Config::node` to its `EtcdNode`: watched and written in-process, no client listener to dial (bridge.md "Embedding"). Settings that make no bridge: exit code 14 (E104, E105 without the feature); a bridge that fails for good stops the node: exit code 15 (E106); one not stopped within `bridge_send_timeout` + 10 s of the signal: exit code 16 (E107). The binary's only: an embedder calling `EtcdNode::init` gets no bridge from these settings |
| `leader` | | [coord] the leader's node uuid, set by an embedder with a leader of its own; a reconfigure moves it. Empty: the lowest node id among this node and the peers it writes to (not Spare) |
| `lock_ttl` | 60 | [sys] the global lock timeout, seconds: the lease a lock asked with none holds (the hex after its key's last `/`), and what an embedder keeps its locks' leases at. 0 (an embedder's `..Default::default()`) is 60 |
| `peer_token` | | [auth] a secret every node shares, on each call one node makes to another; a call with `x-peer` and without it is refused. Without auth, unchecked while empty - a client can then pass for a peer, as before |
| `auth_db_url`, `auth` | | [auth] feature `auth`: signon's PostgreSQL database, and authentication on from the start. `auth_db_url` without `peer_token` is refused at start (ETCD-E173), `auth` without `auth_db_url` too (E172), either without the feature (E174) |
| `queue_down_timeout` | 300 | seconds a queue consumer's node may be down before its consumers count as gone (`Queues`); 0 never. An `Option`: unset is 300, also from `Default` |
| `log_level` | `info` | `error`, `warning`, `info`, `debug` or `trace`. Anything else - `warn` included - logs critical messages only |
| `max_txn_ops`, `max_request_bytes` | 128, 1.5 MiB | stored, not enforced |

Every other field is etcd's, accepted for compatibility and unused.

## Leader, leases and locks

[coord] One node decides leases and locks for the cluster (`coord.rs`, `srv/lease.rs`,
`srv/lock.rs`); the KV stays a broadcast.

- **The leader** is `leader` when set, else the lowest node id among this node and the
  peers it writes to (not Spare). Nodes that see the same peers agree on it; without an
  embedder's leader, a network split may have one on each side. `status` says it.
- **A call on another node is sent on to the leader** (header `x-etcds-fwd`, with the
  client's token). A node that is not the leader any more answers `unavailable` (ETCD-E165),
  as does one whose leader it holds no peer for (E166): the client asks again.
- **Leases.** Granted, kept alive and expired on the leader alone, one clock; the id is
  random and positive unless asked for. A grant and a revoke are replicated to every peer
  (`x-etcds-rep`) before they answer, so a put with the lease right after works on any node,
  and a new leader holds them all - it gives each a full TTL as it takes over, as etcd's
  does. A put with a lease no node granted is refused (`not_found`, E167); `ignore_lease`
  keeps the key's own. A revoked or expired lease's keys are deleted through the ordinary
  delete.
- **Locks.** Queued per name on the leader, first come first served. The holder's key is
  `name/<lease hex>`, etcd's layout, put with the lease when the lock is granted, its
  system record written before the answer - a new leader takes the holders over from there. `Unlock`
  or the lease ending frees it; a waiter whose lease ends is told (`not_found`), one whose
  client goes leaves the queue.
- **Kept as system records** ("System records and sequences"): memory only unless the
  embedder sets a durable store - then a cluster that stops entirely keeps its leases and
  lock holders.

## System records and sequences

[sys] What the leader decides is kept as system records (`sys.rs`), so a leader switch
carries it: `lease/<id>`, `lock/<name hex>` (the holder's key and lease), `seq/<name>`
(the cache size and the next number to hand out).

- **Written before the answer.** A lease granted or revoked, a lock granted or freed, a
  sequence's block handed out: the record first, then the answer. A store that fails the
  write fails the call (`unavailable`, ETCD-E199) - nothing is handed out unrecorded.
- **Where.** Each node holds them in a map, each record with a version (the leader's
  counter) and a delete kept as a tombstone; a node keeps the newest version it is sent,
  so a late put of an older one is no put. The leader replicates each record to every
  peer as a peer's KV put on `/.etcds/sys/<record>` (marked `x-etcds-rep`), and a node
  joining or taking over reads them with a peer's range over that prefix: memory only.
- **A majority, or no answer.** Without an embedder's store, a record counts once a
  majority of the nodes hold it - else the call fails (`unavailable`, E205) - and a new
  leader takes the newest version of each record among a majority that answers it, or
  does not take over yet (E207, tried again each tick). So a number handed out or a lock
  granted is never lost by a leader switch. Two nodes need both: one down, sequences,
  leases and locks wait for it. The majority is of every node this one holds or is
  configured with (`initial_advertise_peer_urls`), not of the peers that answer.
- **A ballot per takeover.** A takeover goes under a ballot of its own - a round above
  every one seen, then the node id - sent with its range (`x-etcds-ballot`). Each node
  that answers promises it and from then on refuses a record of a lower ballot, naming
  the promised one; records order by ballot, then version. A leader superseded while its
  write was on the way is refused by the majority the new one read, so it cannot answer
  for a block the new leader hands out again: it answers `unavailable` (E213), its lock
  waiters and its queue consumer end, and it takes over again on its next call. With an
  embedder's store, the store keeps the writers' order (YT Cluster's: its Boss).
- **An embedder's store** (`set_coord_store`, the `CoordStore` trait: load, put, delete) -
  YT Cluster's, committed through the cluster: the leader writes there first, a new leader
  loads from there, and the records survive the whole cluster stopping. The records a node
  holds when it is set are written there first.
- **No client's.** Outside the KV: in no range, watch or snapshot; a call on `/.etcds/sys/`
  answer a peer only.
- **A new leader** loads the records: each lease a full TTL, each lock held by its record's
  holder while that lease lives, each sequence going on from its mark.
- **Sequences, by a queue pair.** A definition (`seq_define(SeqDefine { .. })`: `create`,
  `start`, `cache`; `at_least` - past what a local sequence gave; `drop`, its mark kept and
  answered - again for one dropped already - so one made again under its name starts above it) and a block (`seq_take(name, count)`: the
  count asked, or the cache size) are decided on the leader: on the leader itself in
  process, from another node by a request on queue `etcds-seq` (`etcds-seqdef` for a
  definition) answered on the asker's reply queue, `etcds-seq-node-<node id hex>`. The leader's
  consumer of the request queues starts on demand - woken by a peer's put of
  `/.etcds/sys/!seq-wake` before a request - so a node that uses no sequence keeps no queue
  of them. No answer within 5 s: `unavailable` (E211), ask again; a request the leader took
  late is a gap. `seq_next(name)` serves the node's block and takes the next when it runs
  out. Cache 1: each number from the leader, in order across the cluster; cache N: unique,
  in order within a node. A restart or a leader switch loses a block's unused numbers - a
  gap, never a repeat.
- **Any etcd client** takes numbers the same way: a put on `/q/etcds-seq/p/<id>` of
  `{"op":"next","name":"<seq>","count":<n or 0>,"reply":"etcds-seq-<its name>","req":"<id>"}`, and a
  consumer's watch on its reply queue gets `{"req":"<id>","ok":true,"block":{"first":..,"count":..}}`
  (or `"ok":false` with `code` and `msg`). [auth] Taking numbers needs write on
  `/q/etcds-seq/`; a definition is an embedder's or a peer's only - a client's put on
  `etcds-seqdef` is refused.
- **The sequences' own queues are no client's** but for a request's put: no consumer of
  `etcds-seq`, `etcds-seqdef` or a node's `etcds-seq-node-*` (a watch there is canceled at
  once), no put of a definition or of an answer on a node's reply queue, no delete in
  them. Each queue takes its own op: a `define` put on `etcds-seq` is answered refused
  (E212). The leader answers on `etcds-seq-<name>` queues only. A peer's call there is
  proved as on `/.etcds/sys/`: the `peer_token`, or without one a peer's address.
- **Locks.** A lock asked with no lease gets one of `lock_ttl`, kept alive while its call
  waits; the same lease locking the same name again is the one acquisition, answered at
  once (etcd's rule). An unlock removes the holder's record first: not removed, nothing is
  freed and the caller is told.
- **A record's put without a `peer_token`** is taken only from the address of a peer the
  node holds; with one, the token is the proof.

## Auth

[auth] Feature `auth`, with `auth_db_url` (`auth.rs`, `srv/auth.rs`, `guard.rs`): signon's
accounts, roles and permissions, through signon's `api` and `permissions` modules.

- **Accounts are signon's,** an etcd user an account named by its address. `Authenticate`
  is signon's sign-in - its limits and SSO rules hold - and its token a signon session
  (`via` `etcd`). Accounts are made, deleted and given passwords in signon: `UserAdd`,
  `UserDelete`, `UserChangePassword` answer `failed_precondition` (E186);
  `signon::api::add_account` makes one from a tool.
- **Roles and permissions** are signon's roles and `role_permissions` in the key space
  `etcd`; the role `root` (made at start) may do anything, and is needed for every
  management call and to turn authentication on or off. `AuthEnable` needs an account
  with `root` (E181); it and `AuthDisable` are sent to every peer.
- **The checks** (`guard.rs`): a range read or written needs a permission covering it,
  adjacent grants together; a txn each range it compares, reads or writes; a watch reads
  what it watches (refused: created and cancelled at once); leases need a signed-in
  account, a lock write on its key. A token's account and permissions are cached 5 s
  per node: a revoked role or session stops working within that.
- **Peers.** A call with `x-peer` must carry `peer_token` (`x-etcds-peer-token`) - always,
  with `auth_db_url`, which needs it set: `x-peer` alone is a header any client can send.
  Each node's calls to another carry it.

## Approvals

[approve] Feature `approvals` (`approve.rs`; needs `auth` and signon's `labac`). What a client
does is in [approval.md](approval.md). What a change has to keep in mind:

- **The rules are plain functions** (`approve::vote`, `Action`, `Definition`, `target`,
  `produced`, `reaches`), tested in `approve_test.rs`; the node's side (`approve_put`,
  `approve_delete`, `approve_txn`) only finds the account, asks labac and writes the records.
- **Three hooks in `srv/kv.rs`**: `put` asks `approve_put(.., true)` before the key's own
  permission (a definition or a vote: root's and labac's to allow) and `approve_put(.., false)`
  after it, before `put_impl` (a message put to a gated queue); `delete_range` asks
  `approve_delete`; `txn` asks `approve_txn`. A peer's own put (`x-peer` without `x-etcds-fwd`:
  a state key replicated, a released message on its way) passes all of them.
- **The leader decides** (`route_to_leader`, the call sent on with the client's token, as a
  lock's): the account is read from signon on the leader, at each vote, not from the 5 s cache.
- **System records** (`sys.rs`): `apv/def/<queue>` the definition, `apv/msg/<queue>/<key hex>`
  the waiting message (its producer key and value in hex, its votes). The state key
  `/approval/<queue>/<key>` is an ordinary replicated key written after the record: it shows,
  it decides nothing.
- **A release** is `put_impl` of the producer's own put on the leader, then the record's
  delete: at least once.
- **The account's groups** are its labac subject's (`signon::labac::subject`) and its signon
  roles; its level is `Acl::allowed(subject, alias, queue, action)` on signon's approval scale
  (`labac::APPROVAL_ACTIONS`, signon 1.1).

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
| `set_coord_store(store)`, `seq_define(SeqDefine)`, `seq_take(name, count)`, `seq_next(name)`, `seq_forget_block(name)` | [sys] the embedder's durable store of the system records (one for the cluster, the leader writes it first), and the cluster sequences in-process; `seq_forget_block` drops this node's block, its next number from the leader |
| `set_sys_keep(keep, prefixes)` | [sys] a per-node keep (`SysKeep`: load, keep): the records it kept read back first, then each record of `prefixes` this node takes written there before it counts or acknowledges it - after the whole cluster stopped, a takeover reads a majority of the copies. YT Cluster keeps the sequences' (`seq/`) in its config table |
| `embed_lease_grant(ttl)`, `embed_lease_keep(id)`, `embed_lease_revoke(id)`, `embed_lock(name, lease, try_only)`, `embed_unlock(key)`, `lock_holder_key(name, lease)`, `lock_ttl()` | [embed] leases and locks as this node's own calls, sent on to the leader with the `peer_token`, none of a client's checks. `try_only`: granted at once or `None` - refused while the lock is held or waited for (the header `x-etcds-try` on a Lock call, `aborted`, E214). The same lease locking a name again is the one acquisition: an embedder whose callers share a lease keeps them apart itself |
| `set_leader(uuid)` | [embed] the leader the embedder names - YT Cluster's Boss - over the configured `leader`, kept across a reconfigure; `""` the configured one again |
| `member_id()`, `config().await` | [coord] this node's member id, and the settings it runs with now - for a `Config` that changes one of them (a reconfigure carries a whole config) |
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
`clickhouse` features in its default build, and `bridge` in ytserv-all) and is its
main user. What it sets,
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
- **A peer etcd stopped sending to is brought back** (ytserv's
  `state/etcd_peers.rs`, nothing in this crate). Every 5 s each node reads
  `member_list`, and promotes each peer not online whose node answers its pings:
  this crate leaves that to the embedder (see "A peer demoted to Spare stays
  Spare"). What the peer missed stays missed. `yt_info('E') etcd_peers` counts
  them, each said in a warning. End to end: `topology/93_etcd.sh` freezes a node
  for one put.
- **S3 tickets** (ytserv's `state/s3.rs`, nothing in this crate). With the row `tickets` =
  `on` in module `B`, ytserv puts a JSON ticket per blob on `/q/s3:<db.schema.table>/p/`,
  on the node that uploaded it: `received`, then `complete`. A plain queue: memory
  only and at least once, as every queue here, so a missing ticket proves nothing,
  and one nobody reads stays in memory.
- **sge's checks** (ytserv's `state/sge_worker.rs`). Beside a roll's end, a `yt:plan`
  or `yt:snapshot` row's outcome goes on the `sge_notify_queue` queue as well.
- **etcd into Postgres, module `F` rows `feed.<event>`** (cluster level; ytserv's
  `state/etcd_upsert.rs`, nothing in this crate). A row each, its value
  `<schema.table>:<column>[,<column>]`: the
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
- **A rolled migration's end** (ytserv's `state/sge_worker.rs`): the worker puts one
  JSON message on the `sge_notify_queue` queue with `kv_put` to `/q/<queue>/p/<key>`,
  and the bridge can send it on to Kafka or a CI over HTTPS. Its fields are in
  yt-cluster's `sge_worker.md` §5, and a change of `kv_put` or the `/q/` keys reaches it.
- **The bridge** (ytserv-all; ytserv's `state/bridge.rs`). The `bridge_*` settings
  are node level and named as this crate's, with two differences:
  - **the broker's password is no setting:** `bridge_to_secret` (the password of
    `bridge_to`'s user) and `bridge_listen_password_secret` name a secret of
    ytserv's store, opened in-process when ytserv builds the `Config` - the config
    rows that replicate hold the name only;
  - an empty `bridge_consumer` is a UUID of the node's own, the same at every
    start, not a new one.

  The bridge runs on the node in-process (`Config::node`), with the node's policy,
  so a prefix under `/secret/` is refused (`served_out`). It is built through
  `bridge::node::settings` and `bridge::node::run`, by one task of its own that a
  configuration only hands the settings to (a stop waits on the broker). It is
  built again when a setting it reads changes or its secret's value does (a put, a
  rotation), and tried again every 10 s when it does not start. A listener beyond a
  loopback address needs a login (it has no TLS), and a `bridge_consumer` given is
  each node's own row. Its state is in `yt_info('E')` `bridge`; end to end:
  `topology/36b_skv_bridge.sh` in yt-cluster-tests.
- **rppd** (in ytserv-all) consumes queues for Python functions through the same
  in-process watch API.
- **The schema map** (ytserv's `state/labac_sync.rs`, nothing in this crate; owner 2026-11-21). With
  `schema_map` on (cluster level), every 30 s each node puts each participating database's tables
  into the plain KV with `kv_put`: `schema/cluster:<cluster>/<db>/<schema>/<table>` is the
  leader's, `.../<table>/<node uuid>` a node's whose table differs (deleted with `kv_delete` once it
  no longer does). The value is schema_guard's table structure as JSON, keys in order, with
  labac's declaration beside it (`security`: `schemaPath`, `alias`, `softDelete`, `rls`, and - since
  2026-11-22 - a table with levels has `schemaPath` as the map of its named paths, `dml` and each level's by its name - no `dml` and an empty `alias` for a table of levels alone); a key is put only when its value changed (`kv_prefix` reads
  them back). ytproxy reads the map. Its state is in `yt_info('E')` `labac`.

### Leader, leases, locks and sequences - adopted (arch1_22.md part 2)

ytserv builds etcds 0.25 from the sibling checkout (the workspace's `[patch.crates-io]`,
until it is published); rppd asks ^0.25.0 with it.

- **The leader is YT's Boss:** `state/etcd_peers.rs` calls `set_leader(<Boss uuid>)` each
  5 s tick (none while an election runs: the one before goes on leading). The Boss's
  switch takes the locks and the sequences over with one load of the records; a
  superseded etcds leader is fenced by the takeover's ballot.
- **SQL locks and sequences** (`state/ycoord.rs`): the extension's `yt_advisory_lock`,
  `yt_try_advisory_lock`, `yt_advisory_unlock`, `yt_advisory_xact_lock`, `yt_nextval`,
  `yt_create_sequence` and `yt_cluster_sequences` ask ytserv by `yt_info` prefixes over the
  unix socket (`lk+:` `lk-:` `lkc:` `lk*:` `sq:` `sq+:` `sq=:` `sq?:`). One lease per
  PostgreSQL (`lock_ttl`), kept alive while ytserv reaches it and its postmaster start time
  stays; the backends take turns on a name in ytserv, the head asking the leader
  (`embed_lock`). Locks as `/.yt/advisory/<db>/<key>`, sequences as `<db>.<schema>.<name>`.
  `<fs_dir>/ycoord.json` carries the lease and its holders across a ytserv restart.
- **The sequences' records are kept** in each node's own `yt_config` rows, module `Q`
  (`set_sys_keep`, prefix `seq/`): the cluster stops whole only with PostgreSQL, and its
  sequences come back with it. Leases and lock holders are not kept: they end with
  PostgreSQL.

### Approvals - not adopted yet

ytserv does not build etcds with `approvals`: it needs `auth` adopted first (below), and
signon's labac tables in the cluster's database.

### Auth - not adopted yet

Adopting it (owner, 2026-11-21) means, in ytserv and rppd:

- **`peer_token`**: one secret for the cluster, from its secret store, the same on every
  node - before any node turns auth on.
- **`auth_db_url`**: the cluster's database with signon's tables, synced like any other
  table, so a token signed in on one node is known on every node. signon writes a session's
  `last_seen_at` at most once a minute (`signon::api::TOUCH_EVERY`), not per call.
- **ytserv's own etcd calls** (`state/etcd_peers.rs`: `member_list`, `member_promote`) are
  in-process trait calls with no metadata: with auth on they are refused unless sent as a
  peer's - `x-peer` and the token - or made before auth is on.
- **rppd** embeds etcds the same way (`etcd-provided` in ytserv-all): it gets the same
  settings from ytserv.

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
- **[coord] Without `peer_token`, a client can pass for a peer:** `x-peer` alone is
  a header, and a peer's lease replica or forward is then taken from anyone. Set it on
  a network a client shares with the nodes.
- **[coord] Without an embedder's `leader`, nothing fences two leaders:** a split may
  have one each side, each granting leases and locks; healed, both lists stand.
- **[coord] Alarms are each node's own,** not raised cluster-wide.
- **[approve] A leader lost between a message's release and its record's delete** leaves the
  message waiting with its count complete: its next approval releases it again (at least once).
- **[approve] Nothing bounds how many messages wait**, nor their size: each is a system record.
- **[auth] A revoked role or session works on for up to 5 s** on a node that checked
  it just before (the cache), and the user and role calls change signon's database,
  not each node's cache - another node's ends within those seconds.

## Changing the crate

- **Unsafe code is forbidden** in the library and the binary
  (`#![forbid(unsafe_code)]` in `lib.rs` and `main.rs`). `cargo make audit` and
  `cargo make geiger` (`Makefile.toml`) run the dependency checks; CI runs both on
  every push and weekly (`.github/workflows/audit.yml`).
- **The protocol is generated** from `proto/` by `build.rs` (tonic-prost-build), only with
  `ETCDS_GEN_PROTO=1 cargo build` (protoc 3.12 or newer), into `src/etcdpb/`, committed and not
  edited by hand: a normal build runs no protoc for it. `proto/etcd/` is the official repo's
  tree, unchanged; what it imports from elsewhere is in `proto/third_party/`.
- **`proto/etcd/` is compared with etcd's own** by `.github/workflows/proto.yml`
  (`.github/proto-compare.sh [ref]`): the official repo's proto files at its newest release,
  byte for byte, any difference a failed run. To follow a release: copy its files in, and
  generate `src/etcdpb` again.
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
  main, E099 the generated gRPC clients, E104-E107 and E110 on the bridge). The i18n tests fail on a key missing in a language, a placeholder that
  differs, a reused code, or a message nothing uses.
