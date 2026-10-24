# ClickHouse queues

A queue whose name starts with `ch:` or `clickhouse:` is consumed by the etcd
server itself: each message is a JSON object, and the server inserts it into a
ClickHouse table over ClickHouse's HTTP interface, as `JSONEachRow`. There is no
consumer process, no watcher and no database in between - a producer with any
etcd v3 client is the whole pipeline.

Built only with the cargo feature `clickhouse` (off by default):

```toml
etcds = { version = "0.21", features = ["clickhouse"] }
```

or, for the standalone server, `cargo install etcds --features clickhouse`.

The feature adds hyper's HTTP/1 client, which tonic already compiles in.

## In short

```shell
# on the ClickHouse side, once
curl -d 'CREATE TABLE default.events (id Int32, source String, ts DateTime DEFAULT now())
         ENGINE = MergeTree ORDER BY id' http://127.0.0.1:8123/

# the etcd node: where to insert
etcds --clickhouse-url http://127.0.0.1:8123 --clickhouse-db default --cluster <uuid> ...
```

```python
import etcd3, json
c = etcd3.client(host="127.0.0.1", port=2379)
c.put("/q/ch:events/producer/1", json.dumps({"id": 1, "source": "etcd"}))
```

The row is in `default.events` once the insert returns - nothing acknowledges it
and nothing watches the queue.

## The queue name is the target

| queue | inserts into |
|---|---|
| `/q/ch:events/...` | `` `<clickhouse_db>`.`events` `` |
| `/queue/clickhouse:events/...` | the same |
| `/q/ch:analytics.events/...` | `` `analytics`.`events` ``, whatever `clickhouse_db` says |

The table part is trimmed and quoted with backticks (an embedded backtick is
doubled). `ch:` alone, with no table, is an ordinary queue.

Produce with the usual producer key, `/q/{name}/p/{key}` or
`/queue/{name}/producer/{key}`; `{key}` is any one path segment, a message id for
your own logs. The value is one JSON object whose fields are the table's columns,
the way `JSONEachRow` reads them: a missing column takes its default, and a
field the table has no column for is skipped or refused as the ClickHouse
setting `input_format_skip_unknown_fields` says for the user the URL names.

## Configuration

| setting | meaning |
|---|---|
| `clickhouse_url` | the HTTP endpoint: `http://host:port`, or `host` alone for port 8123. `http://user:password@host:port` sends HTTP Basic auth. Only the host and port are used: a path or query in the URL is ignored, and the connection is plain HTTP |
| `clickhouse_db` | the database for a queue name with no `db.` part. Empty means `default` |

Both are read at every insert, so a reconfigure (`EtcdMgmtEvent::Config`) takes
effect at the next batch. Set them on **every node that may dispatch the queue**
(see below). An embedder reconfiguring with an empty `clickhouse_url` keeps the
previous endpoint: the queues stay on until the node restarts.

With `clickhouse_url` empty, a `ch:` queue does not drop anything: its messages
stay queued and each wake-up logs `queue /q/ch:... targets ClickHouse but
clickhouse_url is not set` at warning.

## How a batch goes out

The queue's dispatcher holds the messages (as for any queue, [queue.md](queue.md)).
On each wake-up - a put, a delete, a watcher registration - it sends **every
message it holds** as one request:

```
POST /?query=INSERT%20INTO%20`db`.`table`%20FORMAT%20JSONEachRow
Host: host:port
Authorization: Basic ...        (when the URL carried userinfo)

{"id":1,"source":"etcd"}
{"id":2,"source":"etcd"}
```

- **Success (2xx):** the messages of that batch are removed; a message put while
  the request was in flight stays for the next batch.
- **Failure** (connection refused, no answer within 30 s, or a non-2xx status):
  nothing is removed, and the warning names the table, the row count and
  ClickHouse's own message (it puts the reason in the body, which the log line
  carries).
- One fresh connection per batch; no keep-alive.

## Delivery: what to rely on

- **Order:** oldest first within a batch. Rows from different nodes' producers
  reach ClickHouse in the order the dispatcher received them.
- **At least once while the dispatcher lives.** A batch that ClickHouse wrote but
  whose answer was lost (the 30 s timeout) is sent again, so rows can be
  duplicated. So can a put made while the dispatcher is unreachable: it is then
  broadcast, every node keeps a copy, and every node with `clickhouse_url` set
  inserts it. A message id in the row and a `ReplacingMergeTree` ordered by it
  make duplicates harmless.
- **Memory only.** Queued rows live in the dispatcher's memory. A restart of that
  node loses them, and so does the dispatcher dying before it indexed a message a
  producer on another node sent it.
- **No retry timer.** A failed batch is tried again on the next wake-up, which is
  the next put on the queue. If producers stop while ClickHouse is down, the rows
  wait until something is put again.
- **One bad row blocks the queue.** ClickHouse refuses the whole `JSONEachRow`
  request when one line does not parse or does not fit the table, and a refused
  batch keeps every message - so every later batch carries the same bad line and
  fails the same way. Validate before you put, or let ClickHouse skip bad lines
  (`input_format_allow_errors_num` / `input_format_allow_errors_ratio` for the
  user the URL names).
- **An empty value is never sent and never removed:** do not put one on a `ch:`
  queue.
- **No size limit.** The batch is everything queued, however much: a long outage
  makes one large request when ClickHouse comes back.

## Which node inserts

Only the dispatcher keeps a queue's messages; other nodes forward a producer's put
to it and keep nothing. A `ch:` queue has no consumer to pull the dispatcher
towards it, so the dispatcher is **the node of the first producer** that found the
queue unclaimed, recorded in `/q/{name}` (`EtcdNode::queue_dispatcher` reads it).
It moves only when that node goes away (re-election on the next put elsewhere).
That node needs ClickHouse configured and reachable; the others need it only to
take over.

## Tests

- Unit tests in `src/clickhouse.rs`: the queue name forms, the target table and
  database, the URL forms and the Basic auth header.
- End to end: `pipeline/etcd_ch_queue_test.py` in
  [yt-cluster-tests](https://github.com/DBinvent/yt-cluster-tests) puts 200
  messages through a YT Cluster node with a ClickHouse beside it and counts them in
  the table, and topology `94` covers ClickHouse in the seven-node cluster.
