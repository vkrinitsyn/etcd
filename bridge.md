# The bridge (feature `bridge`)

A bridge from an etcds queue to the systems JVM applications read:
a JMS queue over AMQP 1.0, an MQTT 5 topic, or a Kafka topic. It consumes
`/q/{queue}` like any etcd queue consumer, sends each message on, and
acknowledges it in etcd - deletes its delivery key - only after the other side
took it. It can also be the AMQP peer itself, so JMS clients connect to it with
no broker between, and it can send a key prefix's changes instead of a queue's
messages.

etcds runs it, built with the `bridge` feature, from the `bridge_*` settings -
flags, or keys of its config file. The bridge is a client of its own node, over
gRPC at the node's first client URL. The code is `src/bridge/`: `node.rs` the
settings, `mod.rs` the delivery, `listener.rs`, `mapping.rs`, `cloudevents.rs`
and `target/` the brokers. It was the separate crate etcd2jms until 2026-10-29.

```shell
cargo install etcds --features bridge
# to a broker (Artemis, Qpid, any AMQP 1.0 broker JMS reads through Qpid JMS)
etcds --bridge-queue jobs --bridge-to amqp://app:secret@artemis:5672 --bridge-dead-letter jobs-dlq
# to MQTT 5: topics jobs/{key}
etcds --bridge-queue jobs --bridge-to mqtt://mosquitto:1883 --bridge-address jobs
# to Kafka: the topic jobs, one record per message
etcds --bridge-queue jobs --bridge-to kafka://kafka1:9092,kafka2:9092 --bridge-cloudevents
# a key prefix's puts and deletes, a delete a tombstone
etcds --bridge-prefix /config/ --bridge-to kafka://kafka1:9092 --bridge-address config
# JMS clients connect here; a consumer of queue X consumes /q/X
etcds --bridge-listen 0.0.0.0:5672 --bridge-listen-user app --bridge-listen-password secret
```

| setting | default | what |
|---|---|---|
| `bridge_queue` | | the etcd queue to consume, one path segment |
| `bridge_prefix` | | instead of a queue, a key prefix to watch: below |
| `bridge_listen` | | be the AMQP peer on `host:port`, instead of a queue and `bridge_to` |
| `bridge_listen_user`, `bridge_listen_password` | | the user a client logs in as (SASL PLAIN); anonymous without |
| `bridge_to` | | the broker: `amqp://`, `amqps://`, `mqtt://`, `mqtts://`, `kafka://`, `kafkas://`, then `[user:password@]host:port` (Kafka: `host:port,host:port[?options]`, below) |
| `bridge_address` | the queue's name (`bridge_prefix /a/b/`: `a.b`) | the AMQP address (JMS queue), the MQTT topic prefix, or the Kafka topic |
| `bridge_consumer` | random | this consumer's UUID in the queue registry |
| `bridge_body` | `auto` | `auto`, `text` or `bytes`: below |
| `bridge_group_by_key` | off | AMQP: `JMSXGroupID` = the message key |
| `bridge_cloudevents` | off | add the CloudEvents attributes, in each protocol's binding |
| `bridge_ce_type` | `etcd.queue.message` | the CloudEvents `type` |
| `bridge_dead_letter` | | an etcd queue for messages nothing will take |
| `bridge_buffer` | 64 | deliveries held between etcd and the other side |
| `bridge_send_timeout` | 30 | seconds to wait: to connect, and for one message to be taken |

One of `bridge_queue`, `bridge_prefix` and `bridge_listen`; none, no bridge.
Settings that make no bridge stop etcds before the node starts (exit code 14,
ETCD-E104; ETCD-E105 when built without the feature). The signal that stops the
node stops the bridge; what it had not acknowledged goes back to etcd's line. A
bridge that ends on its own failed for good and stops the node (exit code 15,
ETCD-E106); one not stopped within `bridge_send_timeout` + 10 s of the signal
exits 16 (ETCD-E107). The node is reached over plain `http://` at its first
`listen_client_urls`. A setting the mode does not use - `bridge_address` with
`bridge_listen`, `bridge_listen_user` with a queue - is refused, not ignored.

In a program of its own: `bridge::run` (a queue or a prefix) and
`listener::listen`, each with a `bridge::Config` and a future that stops it.

## Delivery

The same for every target:

- **Acknowledged after the other side took it** - AMQP `accepted`, MQTT
  PUBACK, Kafka's offset with `acks=all`, a JMS client's acknowledge on the
  listener. A delete that fails is tried three times; after that the message
  comes back, and is sent again, when the stream closes.
- **In order, one at a time.** Messages go out in the order etcd delivered
  them; the next waits for the one before.
- **Nothing that failed is acknowledged.** The other side down, the connection
  lost, a refusal for now, or no answer within the send timeout: the message is
  sent again, 1 s doubling to 60 s apart. etcd hands a message out once per
  stream, so the bridge holds it until it lands.
- **A message nothing will take** - refused as malformed, invalid or too large,
  or a value that is not UTF-8 with `bridge_body text` - goes to the dead-letter
  queue, `/q/{dead-letter}/p/{key}`, and is acknowledged after that. Without
  `bridge_dead_letter` it is retried like any other failure, and holds the line
  behind it. The dead-letter queue is an etcd queue: memory only, like the rest.
- **At least once.** A message taken whose delete did not reach etcd, or one
  sent on a timeout that the other side in fact took, is sent again. Every send
  of one delivery carries the same message id (below), so a consumer can drop a
  resend after a timeout; a message etcd hands out again, after the bridge's
  stream closed, gets a new one.
- **The etcd stream is the hold.** Unacknowledged messages stay with the
  bridge's watch stream; a new stream (the old one failed, or the bridge
  restarted) gets them from etcd again, so what the bridge still held from the
  old stream is dropped, not sent. While the bridge cannot take more, etcd drops
  it as a slow consumer (ETCD-E052) and it watches again once there is room.
- **Every wait has a deadline.** The etcd connection is probed by HTTP/2
  keepalive (10 s) and each etcd call has 10 s; the AMQP connection has a 60 s
  idle timeout and MQTT a 30 s keep alive; connecting, sending and closing have
  the send timeout.

What counts as refused for good, per target:

| target | for good | retried |
|---|---|---|
| AMQP | rejected `decode-error`, `invalid-field`, `not-implemented`, `message-size-exceeded` | other rejections (`resource-limit-exceeded`, none given), `released`, `modified` |
| MQTT | PUBACK `topic name invalid`, `payload format invalid`; over the broker's maximum packet size | other PUBACK reasons (`quota exceeded`, `not authorized`, ...) |
| Kafka | `MESSAGE_TOO_LARGE`, `CORRUPT_MESSAGE`, `INVALID_RECORD`, `RECORD_LIST_TOO_LARGE` | other broker errors |

## A key prefix

With `bridge_prefix /config/` the bridge watches the keys under `/config/` instead of
a queue, and sends each change, in order, to any target:

- `etcd_key` is the key after the prefix, `etcd_delivery_key` the whole key;
  `etcd_prefix` and `etcd_event` (`put`, `delete`) take `etcd_queue`'s place.
- A put carries the value. A delete carries none: an empty AMQP `data` body,
  an empty MQTT payload, and on Kafka a **tombstone** - the key with no value,
  in the key's partition - so a compacted topic keeps each key's last value.
- With CloudEvents, `source` is the prefix, `type` gets `.put` or `.delete`
  (`etcd.kv.put`, `etcd.kv.delete` by default), and `etcdprefix`, `etcdevent`
  take `etcdqueue`'s place.

A watch is not a queue: nothing is acknowledged - the keys stay - and nothing
is replayed. A change while the bridge is down, or while etcd has dropped its
watch as slow (2 s, ETCD-E085) is missed; a send that fails is retried as above
while the bridge runs. Keys under `/q/` are queue traffic and refused as a
prefix.

## AMQP 1.0 / JMS

Per the AMQP JMS Mapping (OASIS amqp-bindmap-jms v1.0):

| etcd | AMQP | JMS |
|---|---|---|
| the value as text | `amqp-value` string, `x-opt-jms-msg-type` 5 | `TextMessage` |
| the value as bytes | a `data` section, `application/octet-stream`, type 3 | `BytesMessage` |
| one per delivery | `message-id` `ID:<uuid>` | `JMSMessageID` |
| the message key `{key}`, with `bridge_group_by_key` | `group-id` | `JMSXGroupID` |
| the bridge's clock | `creation-time` | `JMSTimestamp` |
| | `header.durable` | `JMSDeliveryMode` PERSISTENT |

`bridge_body auto` makes a UTF-8 value a `TextMessage` and anything else a
`BytesMessage`, so one queue can carry both: a listener there takes `Message`.
Set `text` or `bytes` when the listener expects one type.

`bridge_group_by_key` is off by default: a broker pins each group to one consumer,
and Artemis keeps a map of every group it has seen (`group-buckets`, unlimited
by default), which grows when keys are unique.

Application properties: `etcd_queue`, `etcd_key`, `etcd_index` (a long, when the
delivery key carries the dispatcher's index) and `etcd_delivery_key`. The link's
target has the `queue` capability, so a broker that creates addresses on demand
makes it anycast.

```java
@JmsListener(destination = "jobs")
void onJob(TextMessage m) throws JMSException {
    String key = m.getStringProperty("etcd_key");
    ...
}
```

### The listener

With `bridge_listen` the bridge is the AMQP peer: a JMS client connects to it as to
a broker (`amqp://host:5672` in Qpid JMS, SASL PLAIN with `bridge_listen_user`, else
ANONYMOUS). Each consumer link on queue `X` is an etcd consumer of `/q/X` of its
own, the message shape is the one above, and a message is acknowledged in etcd
when the JMS client acknowledged it - in `AUTO_ACKNOWLEDGE` once `onMessage`
returned, in `CLIENT_ACKNOWLEDGE` at `acknowledge()`. A consumer that closes,
or whose connection drops, ends its etcd consumer, and what it held goes back
to the line. Producers are refused (`amqp:not-implemented`): put to etcd. No
TLS on the listener.

## MQTT 5

Each message is published QoS 1, not retained, to `{address}/{key}`; `%`, `+`,
`#` and `/` in the key are written `%25`, `%2B`, `%23`, `%2F`, so the key is one
topic level and never a wildcard. The payload is the value; a text value has
payload format indicator 1. Content type `text/plain; charset=utf-8` or
`application/octet-stream`; user properties `etcd_queue`, `etcd_key`,
`etcd_index`, `etcd_delivery_key`, `message_id`.

MQTT is not a queue: a PUBACK `no matching subscribers` is taken, and a
message nobody subscribed to is gone. Subscribe with a persistent session
(clean start off, a session expiry) before the bridge publishes, or use a
broker that keeps messages per topic (Artemis does, for its queues).

## Kafka

Each message is one record on the topic `bridge_address`, produced with `acks=all`,
no compression. The record key is the message key, and the partition is the one
Kafka's default partitioner gives that key (murmur2), so a Java producer and the
bridge put a key in the same partition, and a key's messages stay in order. The
timestamp is the bridge's clock; headers `content-type`, `etcd_queue`,
`etcd_key`, `etcd_index`, `etcd_delivery_key`, `message_id`.

The topic must exist - connecting reads its partitions, and the bridge waits,
connecting again, while it is not found; it never asks the broker to create
it. The partitions are read again every `metadata_max_age_ms` (5 minutes, as
Kafka's producers) and on every connect, and a key goes where Kafka's producers
put it for the count. A moved leader or a vanished partition is retried by the
client for two thirds of `bridge_send_timeout`; past that the bridge connects again
and reads the partitions again. Under a client quota, rskafka sends a throttled
produce again: one more duplicate, inside the same at-least-once rule.

A failed connect is logged with the broker's reason - an untrusted
certificate, an unknown host - each time the bridge tries again.

```text
kafka://host:9092,host2:9092                         PLAINTEXT
kafkas://host:9093                                   SSL, the public roots trusted
kafkas://host:9093?ca=/etc/kafka/ca.pem              SSL, a private CA
```

| option | values | default |
|---|---|---|
| `ca` | a PEM file of the CAs to trust (`kafkas://` only) | the public (webpki) roots |
| `metadata_max_age_ms` | how often the partitions are read again | `300000` |

No login: the bridge does not authenticate to Kafka (no SASL), and a user in
the URL is refused. Brokers are `host:port` - a name or IPv4: an IPv6 literal
is refused, as the client cannot reach one. An unknown option, `ca` on
`kafka://` and a CA file with no certificate are refused at start. Not
supported: a client certificate (mTLS), compression, and the idempotent
producer - a resend after a timeout can be a second record (at least once, as
everywhere here; `message_id` is the same on both).

## CloudEvents

`bridge_cloudevents` makes each message a CloudEvent 1.0 in binary content mode,
the value its data:

| attribute | value |
|---|---|
| `specversion` | `1.0` |
| `id` | the message id's UUID |
| `source` | `/q/{queue}` |
| `type` | `bridge_ce_type` |
| `subject` | the message key |
| `time` | when the bridge sent it, RFC 3339 |
| `datacontenttype` | `text/plain; charset=utf-8` or `application/octet-stream` |
| `etcdqueue`, `etcdkey`, `etcddeliverykey`, `etcdindex` | extensions |

each written as its protocol binding says:

| target | attributes | `datacontenttype` |
|---|---|---|
| AMQP | application properties `cloudEvents_{name}` (the binding's spelling for JMS) | `content-type` |
| MQTT 5 | user properties `{name}` | content type |
| Kafka | headers `ce_{name}` | header `content-type` |

On AMQP the `etcd_*` properties stay; on MQTT and Kafka the extensions replace
them. CloudEvents SDKs (Java, Python, Spring Cloud Function) read these as
events.

## Embedding

`etcds::bridge::run(config, logger, shutdown)` runs the bridge in another
tokio program until `shutdown` completes, `etcds::bridge::listener::listen` the
listener; `Config::validate` says what they refuse. `etcds::bridge::mapping`,
`etcds::bridge::cloudevents` and `etcds::bridge::broker::{mqtt::topic,
kafka::partition_of}` are the shapes alone. An embedder of `EtcdNode` gets no
bridge from the `bridge_*` settings: those are the binary's.

## Tests

`cargo test --features bridge` runs the unit tests and, in-process with an
etcds node:

- `tests/bridge_delivery.rs`, with a minimal AMQP broker: delivery in order
  with the acknowledgement; a malformed message kept, or dead-lettered; a full
  address and a release retried, with one message id; a send with no outcome
  sent again with the same id; a broker down past etcd's slow-consumer limit,
  with the watch made again and every message delivered once, in order; a
  configuration refused; a key prefix's puts and deletes sent in order, the
  keys left in place;
- `tests/bridge_failures.rs`: brokers and etcd going away, cut and refusing;
- `tests/bridge_listener.rs` and `tests/bridge_listener_outcomes.rs`, with an
  AMQP client: a client's accept acknowledges; a client that leaves, holding a
  message or idle, gives it back at once; a producer refused; release, reject,
  no credit, a wrong login;
- `tests/bridge.rs`: the binary with `bridge_*` flags and a config file - a
  queue, a prefix and the listener, stopped by SIGTERM and SIGINT - and the
  settings it refuses (exit code 14; without the feature, E105).

`tests/bridge_live.rs` runs against real brokers - `cargo test --features
bridge --test bridge_live -- --ignored`, with `ETCDS_BRIDGE_LIVE_AMQP`,
`ETCDS_BRIDGE_LIVE_MQTT` (comma separated) and `ETCDS_BRIDGE_LIVE_KAFKA` - and
reads back with clients of its own: MQTT topics, payloads and properties, plain
and CloudEvents; Kafka records, headers and each key's partition, and a
prefix's delete as a tombstone (Kafka is a smoke test only, PLAINTEXT).
`live/jms-listener` is a Qpid JMS consumer, run as a container, that prints
what a JMS application reads:

```shell
docker build -t e2j-jms-listener live/jms-listener
docker run --rm e2j-jms-listener amqp://host:5672 user password queue count idle-seconds
```

Run so far against ActiveMQ Artemis 2.42 (AMQP and MQTT 5), Eclipse Mosquitto
2.0 and Apache Kafka 3.9 (KRaft, PLAINTEXT); the listener with Qpid JMS 2.7.

## Built, and not

- **CloudEvents** - by hand (no SDK): binary content mode in the AMQP
  (`cloudEvents_` properties, the JMS spelling), MQTT 5 (user properties) and
  Kafka (`ce_` headers) bindings, with `etcd*` extensions. Not built:
  structured mode (the event as JSON), and protobuf.
- **AMQP 1.0 / JMS** - `fe2o3-amqp`: a client to a broker, and the listener.
  Not built: TLS on the listener.
- **MQTT 5** - `rumqttc`, QoS 1.
- **Kafka** - `rskafka` 0.6.0 (pure Rust). Under a client quota it sends a
  throttled produce again - a duplicate, within at least once; the fix is on
  the rskafka fork's `fix-throttled-produce` branch, for upstream. No login
  (SASL), on purpose. Not built: mTLS, compression, the idempotent producer,
  batching. Not possible: an offset from the etcd revision or index - Kafka
  gives the offset; the index is the `etcd_index` header.
- **Log lines are English only**, not through `tr!` like the node's messages.

Consumers named in the first notes - Spring `@JmsListener`, Spring Integration
MQTT, Spring Kafka, `python-qpid-proton`, `paho-mqtt`, `confluent-kafka`,
CloudEvents SDKs - read these shapes; only Qpid JMS was run.
