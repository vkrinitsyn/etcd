# ETCD based QUEUE

## Using a queue

A queue is used with any etcd v3 client and three calls - watch, put, delete. It
needs no setup: the first put or watch creates it. `{name}` is one path segment,
and `/q/` and `/queue/` are two spellings of the same namespace: `/q/jobs` and
`/queue/jobs` are the same queue. Use one spelling for a queue everywhere,
because its registry key is spelled the way its first user spelled it.

| step | call | key | value |
|---|---|---|---|
| consume | **watch** one key, no range | `/q/{name}/c/{uuid}` or `/queue/{name}/consumer/{uuid}` | |
| produce | **put** | `/q/{name}/p/{key}` or `/queue/{name}/producer/{key}` | the message |
| receive | a put event on the watch | a delivery key, below | the message |
| acknowledge | **delete** | the key the event carried | |

```rust
use etcd_client::{Client, EventType};

let mut c = Client::connect(["localhost:2379"], None).await?;
let me = uuid::Uuid::new_v4();
let mut events = c.watch(format!("/q/jobs/c/{}", me), None).await?;

c.put("/q/jobs/p/job-1", "payload", None).await?;

while let Some(resp) = events.message().await? {
    for ev in resp.events() {
        if ev.event_type() != EventType::Put { continue }
        let Some(kv) = ev.kv() else { continue };
        handle(kv.value());
        c.delete(kv.key(), None).await?;   // the acknowledge
    }
}
```

The rules a client can rely on (`src/queue.rs`):

- **One consumer per message, oldest first.** Messages are handed out in the order
  the dispatcher received them, each to one consumer. With several consumers the
  message goes to the first one that accepts it - there is no balancing between
  them.
- **Messages wait for a consumer.** A message put before any consumer exists is
  held and goes to the first one that registers.
- **The `{uuid}` is the consumer.** A watch key without a valid UUID there takes
  the connection's id instead. Watch the exact key: a range watch is an ordinary
  key watch and receives no messages.
- **`{key}` is one segment.** Anything after a further `/` is dropped.
- **Acknowledge with the key you received.** A delivery key carries the message's
  index - `/q/{name}/{idx}/{key}`, or `/q/{name}/c/{uuid}/{idx}/{key}` when the
  message was handed over from another node - and the delete removes that one
  message. Treat the key as opaque. A known gap: a message a producer on another
  node sent to the dispatcher reaches a consumer on the dispatcher's own node under
  its producer key, `/q/{name}/p/{key}`, which has no index - the delete is
  accepted, the message is not delivered again, but it stays in the dispatcher's
  memory and the queue is never reaped ([agent.md](agent.md), known gaps).
- **A slow consumer is dropped.** A consumer that does not take a message within
  5 s gets nothing more (its registration says `draining`) and a `canceled` frame
  with the reason (ETCD-E052): watch again to receive. One whose stream has
  closed loses its registration. The message goes to the next consumer or waits.
- **A gone consumer's messages go to the next one.** A message delivered and never
  acknowledged is handed out again when the stream it went to closes - whatever
  the client did since: a client that connects again, to this node or another, is
  a new stream, and what the old one held, read or unread, goes back to the line.
  One that stalls or cancels its watch gets nothing new, and keeps what it holds
  until its stream closes; watching again on that same stream, it keeps it on.
  Never on a timeout. So a message can reach a second consumer after the first
  did part of the work: acknowledge after the work, and make the work safe to
  find unfinished.
- **While a consumer's node is down,** what it was handed waits, for the down
  timeout (`queue_down_timeout`, 5 minutes; 0 never): a node back within it keeps
  it. Past it the consumer counts as gone, its registration is withdrawn on its
  behalf, and what it was handed goes to the next consumer - **at least once**
  for those messages: a node that was only cut off, not dead, may run them too.
  Make such work safe to run twice. A node that restarts withdraws, at start and
  from each peer it connects to later, what it no longer hosts, and a stream from
  before its restart is known dead wherever its client registers again.
- **Acknowledge on commit, not on failure.** The acknowledgement is the queue's
  side of the consumer's commit: delete the key when the work's transaction has
  committed (in YT Cluster, the cluster commit). On a rollback do not
  acknowledge: keep the message and try again, since etcd will not hand it out a
  second time. A consumer that must give up on a message (one that can never succeed as
  it is) puts it on a dead-letter queue of its own first, and only then
  acknowledges it: a failed message is never acknowledged without one.
- **Memory only.** Messages live in the dispatcher's memory: a restart of that
  node loses them, and so does the dispatcher dying before it indexed a message
  from a producer on another node (below).
- **A key watcher must keep reading.** One that does not take a notification
  within 2 s is cancelled - a `canceled` frame with the reason (ETCD-E085) - as a
  slow consumer is: watch again to receive. Writes never wait on it.
- **A rolling upgrade keeps routing.** While any node runs an older version, the
  nodes write the registry in the older form (the bare node id), which every
  version reads; until the last one is upgraded, what a closed stream held goes
  back once its client has no stream left on that node, rather than at once.
- **The server writes the registry.** `/q/{name}` (the dispatcher) and
  `/q/{name}/c/{uuid}` (a consumer's node and stream: `<node>;g=<generation>`,
  `;s=` the node's start, `;draining` added for one that receives no more;
  `;closed` a tombstone, `;gone` one written for a node down too long) are
  ordinary replicated keys the server maintains, and writes again whenever a
  peer joins. Read them if you like; a put there overwrites the routing. A peer
  held Spare - it failed a write - gets no writes, the registry neither: it gets
  the registry again when it is promoted, so it does not dispatch on what it
  missed.

Request/reply pairs, and how few network hops a message takes between nodes, are
in [queue-p2p-route.md](queue-p2p-route.md). A queue named `ch:<table>` writes its
messages into ClickHouse instead of handing them to a consumer:
[clickhouse.md](clickhouse.md).

## Shortly

Everything from:
- /queue/{q_name}/producer/*

will be moved to: 
- /queue/{q_name}/{idx}/{key}

then copied to:
- /queue/{q_name}/consumer/{client_id}/{idx}/{key}

The pipeline looks like this:

producer -> producer node -> dispatcher node -> consumer node -> client
- all might be on same node


## In detail

### Create a queue

On a first incoming event, the node become a queue leader (dispatcher), if no queue definition set, or existing node not available.

- key: /q/{q_name} OR /queue/{q_name}
- value: queue coordinator node id

The key must be a string starting with /q/ OR /queue/ 
This KV distributed to the whole cluster. So, this will take time to get more than half nodes confirm the new value set.

> [!NOTE]
> - Change the queue dispatcher, if current one not handling messages recently
> - loaded queue definition from DB
> - get a CLI command to create a queue


> [!IMPORTANT]
Delivery guarantee for producer implemented on connection to dispatcher queue.
There is a chance that producer if not connected to dispatcher node might 
get lost message if dispatcher node get dies suddenly before indexed message and broadcast it. 


the node as queue dispatcher put rsvr on a queue, to
 - handle /q/{q_name}/p/ create event to dispatch
 - handle /q/{q_name}/c/{client_id}/{idx}/ drops
 - handle /q/{q_name}/c/{client_id}/ creates a new consumer


### Create a consumer
client as etcd consumer creates key local key and start etcd watcher for client assigned
/queue/{q_name}/consumer/{client_id}/
/q/{q_name}/c/{client_id}/
 value is a clients host node, 
 the key will drop, if client wont work for a while
 if client erase or update value, dispatcher will automatically overwrite to current client host
 the copy set to a queue coordinator node
 client can only delete from this path


### Send a messages by producer

producer as etcd client put msg (a value) to node it's connected, 
	then the node copy to a coordinator aka dispatcher node:
/queue/{q_name}/producer/{key}
/q/{q_name}/p/{key}
/q/{q_name}/input/{client_id}/{key}
/q/{q_name}/i/{client_id}/{key}
	0-1 network synch calls, no locks

> [!NOTE] 
TODO: Create multiple producers as a sender stream by
/queue/{q_name}/producer@{node_id} with value: connection url,
dispatcher will create a watcher stream into the peer 


### Place into the ordered queue 
 
The dispatcher node move to ordered queue:
/queue/{q_name}/{idx}/{key}
operation depends on reliability configuration: more than half nodes or at least few nodes or no set
	variable network calls, might or might not wait to response, no locks

### Distribute to a consumer related node

The dispatcher command to consumer node to copy to notify client:
Client etcd watcher get notifyed of a new event, in a key value:
/queue/{q_name}/consumer/{client_id}/{idx}/{key}:
/q/{q_name}/c/{client_id}/{idx}/{key}:
	no network call if no response on [2] required OR same node
	otherwise no network data call, only command

### Handle the event and acknowledge it

Once client(_id) get etcd watch notification from .../c/{client_id}/{idx}/{key}, 
	1. client load message from KV value, 
	2. processe it
	3. deleted from his queue, 
	 - The client_id can only delete from path /queue/{q_name}/consumer/{client_id}/
	Then dispatcher remove from indexed queue: /queue/{q_name}/{idx}/{key}

	deferred variable network calls, no locks

