use crate::cluster::{ClientId, EtcdClientNode, EtcdNode, WatcherConsumer, WatcherId};
use crate::etcdpb::etcdserverpb::{PutRequest, PutResponse, WatchCancelRequest, WatchCreateRequest, WatchResponse};
use crate::{KvEvent, LP};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use slog::{debug, info, trace, warn, Logger};
use tokio::sync::mpsc::{Receiver, Sender, WeakSender};
use tokio::sync::{mpsc, oneshot, RwLock};
use tokio::time::Instant;
use tonic::{Response, Status};
use uuid::Uuid;
use crate::etcdpb::mvccpb::{Event, KeyValue};
use crate::peer::{BroadcastRequest};

/// queue dispatcher
/// Everything from:
/// - /queue/{q_name}/producer/*
///
/// will be moved to: 
/// - /queue/{q_name}/{idx}/{key}
/// 
/// then copied to:
/// - /queue/{q_name}/consumer/{client_id}/{idx}/{key}
/// 
/// The pipeline looks like this:
/// 
/// producer -> producer node -> dispatcher node -> consumer node -> client
/// - all might be on same node
#[derive(Clone)]
pub struct Queue {
    /// readonly name /queue/{q_name} or /q/{q_name}
    pub(crate) fq_name: String,

    pub(crate) etcd: Arc<RwLock<EtcdNode>>,

    /// [q-route] Where this queue's traffic goes: here, one named peer, or
    /// nowhere known yet.
    ///
    /// Replaces an `Option<EtcdPeerNodeType>` in which `None` meant BOTH "I am
    /// the dispatcher" and "nobody has been elected". Nothing ever wrote it, so
    /// the second reading always won and every produced message was broadcast
    /// to every peer - `2(N-1)` RPCs per round trip. See
    /// `queue-p2p-route.md` and [`crate::route::Dispatch`].
    dispatch: Arc<RwLock<crate::route::Dispatch>>,

    /// The record as the registry holds it, for the Phase 3 fields.
    record: Arc<RwLock<Option<crate::route::DispatchRecord>>>,

    /// [q-route Phase 3] The consumer host this queue has been watching, and
    /// since when. Hysteresis: a consumer that reconnects to a different node
    /// would otherwise drag the dispatcher with it on every flap and churn the
    /// registry cluster-wide.
    pin_seen: Arc<RwLock<Option<(crate::cluster::NodeId, Instant)>>>,

    /// When this queue last did anything, for the idle reaper.
    idle_since: Arc<RwLock<Option<Instant>>>,

    /// store messages /queue/{q_name}/{idx}/{key}
    queue: Arc<RwLock<VecDeque<QueueMsg>>>,

    /// store messages /queue/{q_name}/{idx}/{key}
    idx: Arc<AtomicU64>,

    // TODO currently handled message:
    // dispatched: Arc<RwLock<(ClientId, WatcherId, u64)>>

    /// store deliveries /queue/{q_name}/consumer/{client_id}/{idx}/{key}
    clients: Arc<RwLock<HashMap<ClientId, crate::cluster::EtcdClientType>>>,

    /// [q-dead] Per watcher, what ends its closed-stream task (`make_consumer`)
    /// when the watcher is dropped here or watched again: removing the entry
    /// drops the sender, and the task, with its clone of the watch's sender,
    /// ends. Without it a cancelled watch's stream stayed open for the client,
    /// whose reader waits for that stream to end.
    closers: Arc<RwLock<HashMap<(ClientId, WatcherId), oneshot::Sender<()>>>>,

    /// [q-dead] Streams here that receive no more - their consumer stalled, or
    /// cancelled - and are still open: they keep what they were handed until
    /// they close (`drain`). By client and stream generation, with the stream,
    /// held weak: a watch made again on it makes it receiving again. And the
    /// token of the one `drain` task watching it.
    draining: Arc<RwLock<HashMap<(ClientId, u64), (WeakSender<Result<WatchResponse, Status>>, u64)>>>,

    /// [q-dead] Streams here that closed lately, for `QUEUE_IDLE`: their
    /// tombstones are written again when a peer joins, for one that was not
    /// receiving when they were first written.
    closed: Arc<RwLock<VecDeque<(ClientId, u64, Instant)>>>,

    /// `/q/ch:<table>/` and `/queue/clickhouse:<table>/`: the queue is its own
    /// consumer and writes each message's JSON value straight into this
    /// ClickHouse table, with no watcher on the other end.
    #[cfg(feature = "clickhouse")]
    ch_table: Option<String>,

    sender: Sender<MsgNotifyType>,
}

type MsgNotifyType = u64;

/// How long a consumer gets to accept one dispatched message before it is
/// treated as dead. Bounded on purpose: an unbounded `send().await` on a watcher
/// nobody is draining stalls every other consumer of the same queue.
const DISPATCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// [q-dead] How long a consumer on this node gets to take a message another
/// node's dispatcher sent here. Well under `PEER_RPC_TIMEOUT`: the dispatcher
/// gives up on the call at that, the call is dropped with everything it was
/// doing, and a consumer that stalled was neither dropped nor told - and was
/// offered the next message, and the next, a wait each.
const REMOTE_DISPATCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// [q-dead] How soon a dispatcher holding messages looks again on its own,
/// with no put, ack or watcher to wake it.
///
/// A wake-up was the only thing that ran the dispatch loop, so a message no
/// consumer could take was held until the next one - and a consumer known
/// only later (its registration reached this node after the put), or a peer
/// that came Online only later, never got it on a quiet queue. A look that
/// finds nothing to do doubles the wait, up to [`REDISPATCH_MAX`]; a wake-up
/// or a delivery starts it again here.
const REDISPATCH: std::time::Duration = std::time::Duration::from_secs(1);

/// [q-dead] The longest wait between two looks of a dispatcher holding
/// messages: what a held message costs a queue nobody consumes.
const REDISPATCH_MAX: std::time::Duration = std::time::Duration::from_secs(30);

/// [q-route Phase 3] How long a queue's single consumer must stay on one node
/// before the dispatcher follows it there.
///
/// Hysteresis, and it is not tuning. Moving a dispatcher rewrites a replicated
/// key that every node reads, so a consumer flapping between two nodes would
/// churn the registry for the whole cluster while saving one hop for itself.
/// Thirty seconds is long against a reconnect and short against a deployment.
const PIN_STABLE: std::time::Duration = std::time::Duration::from_secs(30);

/// How long an empty, consumerless queue is kept before it is reaped.
///
/// `queue-p2p-route.md` lists teardown as an open question - *"nothing removes
/// a queue from `EtcdNode::queues` - no TTL, no reaper"*. The registries made
/// it pressing rather than merely untidy: a leaked queue now also leaks
/// `/q/{name}` and `/q/{name}/c/{client}`, which are **replicated**, so one
/// node's leak becomes every node's memory.
///
/// Ten minutes is long against a queue that is merely idle between bursts and
/// short against a lifetime. The guards below, not this number, are what make
/// it safe.
pub(crate) const QUEUE_IDLE: std::time::Duration = std::time::Duration::from_secs(600);

/// client that produce queue input  
#[derive(Clone, Debug)]
pub struct QueueNameKey {
    /// copy of original key as string
    input: String,
    /// /q/ or /queue/
    prefix: String,
    /// the name after prefix
    pub(crate) queue_name: String,
    /// key after /{idx}/, or /producer/
    tail: String,
    /// if received and indexed msg, then must be from dispatcher to keep a copy until delivered
    idx: Option<u64>,
    /// if key is consumer, then on delete needs to delete an indexed line (AKS)
    consumer: bool,
    /// if key is producer, then on put needs move to indexed line
    #[allow(dead_code)]
    producer: bool,
    /// if key the queue but neither consumer nor producer, then no extra work
    queue: bool,
    /// if key is consumer, then key should contain a client uuid
    client_id: Option<Uuid>,
}

/// [q-idx] The first index of a queue's dispatcher: the time it started, in
/// microseconds, so indexes do not repeat across restarts and moves of the
/// dispatcher. The unit tests here start at 1, to read their keys;
/// `tests/queue.rs` sees the clock.
fn first_idx() -> u64 {
    #[cfg(test)]
    return test::FIRST_IDX.with(|f| f.get());
    #[cfg(not(test))]
    clock_idx()
}

fn clock_idx() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64).unwrap_or(1).max(1)
}

/// [q-idx] Whether an acknowledge of `idx` with the key `tail` is for `m`:
/// the index and the key's last segment both. Indexes start at 1 again when a
/// dispatcher restarts or moves, so a late acknowledge of an older message
/// named the new message with its index, and removed it undelivered.
fn acknowledges(m: &QueueMsg, idx: u64, tail: &str) -> bool {
    m.idx == idx && m.key.rsplit('/').next() == Some(tail)
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct QueueMsg {
    /// message index (uuid?)
    idx: u64,
    /// send as vec<u8> but must be a string literal 
    key: String,
    value: Vec<u8>,
    created: Instant,
    /// set by dispatcher when forwarded to client
    handled_by: Option<ClientId>,
    /// [q-dead] the stream it was handed to: the node hosting it and its
    /// generation. Released when THAT stream closes, whatever the client
    /// registered since. `None` with `handled_by` set: marked before streams
    /// had generations.
    stream: Option<(crate::cluster::NodeId, u64)>,
}

/// The oldest message no consumer has been handed yet.
///
/// `put` pushes to the FRONT, so FIFO order means walking from the back. Note
/// this deliberately ignores which idx the wake-up carried: a notification is
/// only "something changed", and matching it against the head is what used to
/// stall the queue at one message.
fn next_undelivered(q: &VecDeque<QueueMsg>) -> Option<u64> {
    q.iter().rev().find(|m| m.handled_by.is_none()).map(|m| m.idx)
}

impl From<&QueueMsg> for KeyValue {
    fn from(value: &QueueMsg) -> Self {
        KeyValue {
            key: value.key.clone().into_bytes(),
            value: value.value.clone(), .. Default::default()
        }
    }
}

impl QueueNameKey {
    const P1: &'static str = "p";
    const P: &'static str = "producer";
    const I1: &'static str = "i";
    const I: &'static str = "input";
    const C1: &'static str = "c";
    const C: &'static str = "consumer";
    const Q1: &'static str = "q";
    const Q: &'static str = "queue";

    /// store deliveries /queue/{q_name}
    #[inline]
    pub fn new(value: String) -> Self {
        let names: Vec<&str> = value.split("/").collect();
        let queue = names.len() > 2 && names.get(1)
            .map(|v| v.trim().len() > 0 && *v == Self::Q1 || *v == Self::Q).unwrap_or(false);
        let consumer = queue && names.get(3)
            .map(|v| v.trim().len() > 0 && (*v== Self::C1 || *v == Self::C)).unwrap_or(false);
        let producer = queue && names.get(3)
            .map(|v| v.trim().len() > 0 && (*v== Self::P1 || *v == Self::P
                || *v== Self::I1 || *v == Self::I) ).unwrap_or(false);
        let client_id = if consumer {
            Uuid::parse_str(names.get(4).unwrap_or(&"")).ok()
        } else {
            None
        };

        QueueNameKey {
            prefix:  names.get(1).map(|v| v.to_string()).unwrap_or("".to_string()),
            queue_name: names.get(2).map(|v| v.to_string()).unwrap_or("".to_string()),
            tail: names.get(if consumer { 6 } else { 4 }).map(|v| v.to_string()).unwrap_or("".to_string()),
            idx: names.get(if consumer { 5 } else { 3 }).unwrap_or(&"").parse::<u64>().ok(),
            consumer,
            producer,
            queue,
            client_id,
            input: value,
        }
    }
    pub fn is_queue(&self) -> bool {
        self.queue
    }

    pub fn name(&self) -> String {
        format!("/{}/{}", self.prefix, self.queue_name)
    }
}


impl Queue {

    pub(crate) async fn make_consumer(&mut self, consumer_key: QueueNameKey, client_id: Uuid, watcher_id: i64, sender: &Sender<Result<WatchResponse, Status>>) {
        let key = consumer_key.input.clone().into_bytes();
        // a consumer that registers after messages were produced must still get
        // them: kick the dispatcher once this watcher is in place (below)
        let notify = self.sender.clone();
        let client = sender.clone();
        // [q-dead] the stream's generation: the one this channel has already -
        // another watch of this client on it, or a draining stream watched
        // again - or a new one for a new stream
        let generation = self.gen_for(&client_id, sender).await;
        // [q-dead] a watch made again under its id on another channel: the old
        // stream receives no more, and may still be open
        let mut replaced = None;
        let mut clients = self.clients.write().await;
        match clients.get_mut(&client_id) {
            None => {
                let mut watchers = HashMap::new();
                watchers.insert(watcher_id, WatcherConsumer { key, client, generation } );
                clients.insert(client_id,  Arc::new(RwLock::new(EtcdClientNode { client_id, watchers } )));
            }
            Some(c) => {
                let mut watchers = c.write().await;
                match watchers.watchers.get_mut(&watcher_id) {
                    None => {
                        watchers.watchers.insert(watcher_id, WatcherConsumer { key, client, generation });
                    }
                    Some(w) => {
                        if w.generation != generation {
                            replaced = Some((w.generation, w.client.downgrade()));
                        }
                        w.key = key;
                        w.client = client;
                        w.generation = generation;
                    }
                }
                if let Some((old, _)) = &replaced {
                    if watchers.watchers.values().any(|w| w.generation == *old) {
                        replaced = None; // the old stream still has a watch here
                    }
                }
            }
        }
        // receiving again, if it was draining - under the clients' lock, as
        // `drop_watcher` puts it there
        self.draining.write().await.remove(&(client_id, generation));
        let token = next_gen();
        if let Some((old, ch)) = &replaced {
            self.draining.write().await.insert((client_id, *old), (ch.clone(), token));
        }
        drop(clients);
        self.touched().await;
        if let Some((old, ch)) = replaced {
            self.drain(&client_id, old, ch, token);
        }

        // [q-dead] Drop this watcher when its stream closes, with or without a
        // message to send it. The dispatch loop found a closed stream only by
        // sending into it, so a consumer that went away after its last message
        // stayed registered, and that message - handed to it, never
        // acknowledged - was held for it for good.
        {
            let (queue, ch) = (self.clone(), sender.clone());
            let (closer, dropped) = oneshot::channel::<()>();
            // a watch made again under this id ends the task of the one before
            self.closers.write().await.insert((client_id, watcher_id), closer);
            tokio::spawn(async move {
                tokio::select! {
                    _ = ch.closed() => queue.drop_closed(&client_id, watcher_id, &ch).await,
                    _ = dropped => {} // dropped here already, or watched again
                }
            });
        }

        // [q-route Phase 2] Record which node hosts this consumer, so the
        // dispatcher can route to it instead of broadcasting and hoping.
        // Replicated like any key, so every node can resolve it.
        //
        // BEFORE the claim below, and the order matters: the claim asks whether
        // the current dispatcher hosts a consumer, and it has to be able to see
        // this one.
        self.sync_registration(&client_id, false).await;

        // [q-route] **This node now holds a consumer, so it should dispatch.**
        //
        // A dispatcher at neither end costs two hops - producer node ->
        // dispatcher -> consumer node. At the consumer it costs one, for EVERY
        // producer at once. So a consumer's node claims the queue immediately
        // and takes it over from a producer's node; it does not wait for the
        // Phase 3 hysteresis, which exists for RELOCATING an established
        // dispatcher, not for placing one.
        let log = self.etcd.read().await.log.clone();
        self.elect(crate::route::Claim::Consumer, &log).await;

        let _ = notify.send(0).await;
    }

    /// [q-dead] The generation of the stream `ch` is for `client`: the one a
    /// watch of the client already has on it, or a draining stream's that it
    /// is, or a new one.
    async fn gen_for(&self, client: &ClientId, ch: &Sender<Result<WatchResponse, Status>>) -> u64 {
        if let Some(c) = self.clients.read().await.get(client).cloned() {
            if let Some(w) = c.read().await.watchers.values().find(|w| w.client.same_channel(ch)) {
                return w.generation;
            }
        }
        for ((cid, generation), (weak, _)) in self.draining.read().await.iter() {
            if cid == client && weak.upgrade().map_or(false, |s| s.same_channel(ch)) {
                return *generation;
            }
        }
        next_gen()
    }

    /// Write every pending message into ClickHouse as one JSONEachRow batch and
    /// drop the ones that made it. Messages stay queued on failure, so a
    /// ClickHouse that is down delays delivery rather than losing it.
    #[cfg(feature = "clickhouse")]
    async fn drain_to_clickhouse(&self, table: &str, log: &Logger) {
        let (url, db) = {
            let cfg = self.etcd.read().await.cfg.clone();
            let cfg = cfg.read().await;
            (cfg.clickhouse_url.clone(), cfg.clickhouse_db.clone())
        };
        let Some(sink) = crate::clickhouse::ChSink::new(&url, &db) else {
            warn!(log, "{}{}", LP, tr!("queue.ch_no_url", queue = self.fq_name));
            return;
        };

        // oldest first, and remember what we are sending so a concurrent put is
        // not dropped along with the batch
        let batch: Vec<(u64, String)> = {
            let q = self.queue.read().await;
            q.iter().rev()
                .map(|m| (m.idx, String::from_utf8_lossy(&m.value).trim().to_string()))
                .filter(|(_, v)| !v.is_empty())
                .collect()
        };
        if batch.is_empty() {
            return;
        }
        let rows: Vec<String> = batch.iter().map(|(_, v)| v.clone()).collect();
        match sink.insert(table, &rows, log).await {
            Ok(()) => {
                let sent: std::collections::HashSet<u64> = batch.iter().map(|(i, _)| *i).collect();
                let left = {
                    let mut q = self.queue.write().await;
                    q.retain(|m| !sent.contains(&m.idx));
                    q.len()
                };
                debug!(log, "{}{}", LP, tr!("queue.ch_sent", table = table, rows = rows.len(), queue = self.fq_name, left = left));
            }
            Err(e) => {
                // left queued on purpose: the next put or retry delivers them
                warn!(log, "{}{}", LP, tr!("queue.ch_failed", table = table, rows = rows.len(), err = e));
            }
        }
    }

    /// Forget one watcher, and the client with it once it has none left, so the
    /// dispatcher stops handing messages to something that cannot take them.
    ///
    /// [q-dead] `gone`: its stream is closed, so what was handed to that stream
    /// goes to the next consumer (`stream_closed`). Otherwise - it stalled, or
    /// cancelled the watch - its stream is open and it may still be at work on
    /// what it has: it only stops receiving, and keeps that until the stream
    /// closes (`drain`). Handing it to another would run the job twice. Either
    /// way only once no other watch of the client is on that stream.
    ///
    /// The bookkeeping is done here, the writes that follow in a task: they
    /// wait on the network and on the watch-notify task, and this is called
    /// from the dispatch loop and from a peer's delivery.
    ///
    /// [q-dead] With `on`, only while the watcher is still on that stream: a
    /// send that waited may find it watched again under its id on another.
    pub(crate) async fn drop_watcher(&self, client_id: &ClientId, watcher_id: WatcherId, gone: bool,
        on: Option<&Sender<Result<WatchResponse, Status>>>)
    {
        let mut clients = self.clients.write().await;
        let Some(c) = clients.get(client_id).cloned() else { return };
        let (w, still_on_stream, empty) = {
            let mut c = c.write().await;
            if let (Some(ch), Some(w)) = (on, c.watchers.get(&watcher_id)) {
                if !w.client.same_channel(ch) {
                    return;
                }
            }
            // [q-dead] its closed-stream task ends with it: a stream left with
            // only a cancelled watch must be able to end
            self.closers.write().await.remove(&(*client_id, watcher_id));
            let Some(w) = c.watchers.remove(&watcher_id) else { return };
            let still = c.watchers.values().any(|o| o.generation == w.generation);
            (w, still, c.watchers.is_empty())
        };
        if empty {
            clients.remove(client_id);
        }
        if still_on_stream {
            return;
        }
        // [q-dead] draining before it leaves the clients, under their lock: a
        // look of the dispatch loop between the two would find the stream in
        // neither, take it for gone, and hand out what it holds
        let token = next_gen();
        if !gone {
            self.draining.write().await.insert((*client_id, w.generation), (w.client.downgrade(), token));
        }
        drop(clients);
        if gone {
            let (queue, cid, generation) = (self.clone(), *client_id, w.generation);
            tokio::spawn(async move { queue.stream_closed(&cid, generation).await });
        } else {
            self.drain(client_id, w.generation, w.client.downgrade(), token);
        }
    }

    /// [q-dead] A consumer's stream closed. What was handed to it and is not
    /// acknowledged goes to the next consumer - here, and on every other node
    /// by the stream's tombstone, which they apply whatever the client
    /// registered since: a client that moved, or connected again, left that
    /// stream behind, and what sat in it unread is nobody's but the line's.
    /// Then the client's registration follows what is left of it here.
    pub(crate) async fn stream_closed(&self, client: &ClientId, generation: u64) {
        self.draining.write().await.remove(&(*client, generation));
        let etcd = self.etcd.read().await.clone();
        let me = etcd.peers.read().await.me();
        self.release_stream(client, me, generation, &etcd.log).await;
        // [q-dead] With an older member here there is no tombstone: the
        // registration is deleted once the client has no stream left here
        // (`sync_registration`), and a dispatcher then hands back what any of
        // its streams here held.
        // remembered either way: back in the full form, its tombstone is written
        // with the others (`republish`)
        self.closed_now(client, generation).await;
        if !etcd.peers.read().await.legacy_registry().await {
            let key = crate::route::consumer_key(&self.fq_name, client);
            let t = crate::route::Registration::new(me, generation, crate::route::StreamState::Closed);
            if let Err(e) = put_control(&etcd, &key, &t.render()).await {
                warn!(etcd.log, "{}[q-route] {}", LP, tr!("queue.withdraw_failed", key = key, err = e));
            }
        }
        self.sync_registration(client, false).await;
    }

    /// [q-dead] Remember a stream closed, for its tombstone to be written
    /// again on the next joins (`republish`).
    async fn closed_now(&self, client: &ClientId, generation: u64) {
        let mut closed = self.closed.write().await;
        closed.retain(|(_, _, t)| t.elapsed() < QUEUE_IDLE);
        closed.push_back((*client, generation, Instant::now()));
    }

    /// [q-dead] A stream that receives no more and is still open.
    ///
    /// Registered as draining (`StreamState::Draining`) when the client has no
    /// receiving stream here, so no dispatcher hands it anything new, while
    /// each keeps what it was handed; and watched until it closes, which makes
    /// it gone. Watched through a weak sender: a strong one would itself hold
    /// open the stream of a client that cancelled and waits for it to end. A
    /// watch made again on it makes it receiving again (`make_consumer`).
    ///
    /// All in a task of its own: the registration write waits on the network
    /// and on the watch-notify task.
    fn drain(&self, client: &ClientId, generation: u64, ch: WeakSender<Result<WatchResponse, Status>>, token: u64) {
        let (queue, cid) = (self.clone(), *client);
        tokio::spawn(async move {
            let log = queue.etcd.read().await.log.clone();
            info!(log, "{}{}", LP, tr!("queue.draining", consumer = cid, queue = queue.fq_name));
            queue.sync_registration(&cid, false).await;
            // soon at first - a cancelled stream mostly ends at once - then
            // further apart, up to REDISPATCH_MAX, for one left open
            let mut wait = REDISPATCH;
            loop {
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(REDISPATCH_MAX);
                // receiving again, closed already, or watched by a newer task
                if queue.draining.read().await.get(&(cid, generation)).map(|(_, t)| *t) != Some(token) {
                    return;
                }
                // no sender left - the stream ended here - or no reader there
                if ch.upgrade().map_or(true, |s| s.is_closed()) {
                    info!(log, "{}{}", LP, tr!("queue.drained", consumer = cid, queue = queue.fq_name));
                    queue.stream_closed(&cid, generation).await;
                    return;
                }
            }
        });
    }

    /// [q-dead] Tell a consumer dropped as slow that its watch is over - etcd's
    /// cancel from the server, with the reason - and take the watch as a cancel
    /// does (`EtcdNode::forget_watch`), so it can watch again under the same
    /// id. Until it does its stream is draining: it keeps what it was handed.
    ///
    /// All in a task of its own. The frame waits for room in a full stream, or
    /// for it to close; and the watch-notify task can hold the watchers - for
    /// a key watch on that same full stream - for as long.
    fn cancel_stalled(&self, client: &ClientId, watcher: WatcherId, ch: Sender<Result<WatchResponse, Status>>) {
        let (queue, cid) = (self.clone(), *client);
        tokio::spawn(async move {
            let etcd = queue.etcd.read().await.clone();
            // told once: a stall seen twice - the loop and a peer's delivery -
            // takes the watch once, and a second frame would have the client
            // watch twice
            if etcd.forget_watch(&cid, watcher, Some(&ch)).await.is_none() {
                return;
            }
            let reason = tr!("queue.stalled", timeout = format!("{:?}", DISPATCH_TIMEOUT));
            let _ = ch.send(Ok(WatchResponse { watch_id: watcher, canceled: true, cancel_reason: reason,
                ..Default::default() })).await;
        });
    }

    /// [q-dead] `client`'s registration as this node has it now: its newest
    /// receiving stream, else its newest draining one, else none.
    async fn registration_here(&self, client: &ClientId, me: crate::cluster::NodeId) -> Option<crate::route::Registration> {
        use crate::route::{Registration, StreamState};
        if let Some(c) = self.clients.read().await.get(client).cloned() {
            if let Some(generation) = c.read().await.watchers.values().map(|w| w.generation).max() {
                return Some(Registration::new(me, generation, StreamState::Receiving).since(incarnation()));
            }
        }
        self.draining.read().await.keys().filter(|(cid, _)| cid == client).map(|(_, g)| *g).max()
            .map(|generation| Registration::new(me, generation, StreamState::Draining).since(incarnation()))
    }

    /// [q-dead] Write `client`'s registration as this node has it now
    /// (`registration_here`), when the stored one differs - or always with
    /// `force`, for a peer that just joined. None left here: a registration
    /// naming this node is withdrawn by its tombstone.
    ///
    /// Never over a newer registration of another node: the client moved
    /// there. And read again after the write, and written again if a watch was
    /// made, dropped or closed meanwhile - a check, then a write, could
    /// otherwise put back what was just undone. Returns how many writes.
    ///
    /// A stored registration naming this node for a stream it does not hold -
    /// from before a restart, say, under a client id that came back - gets its
    /// tombstone first: written over, what a dispatcher holds for that stream
    /// would look alive for good. Forced and unchanged, the value only goes to
    /// the peers: a watch on it here would take it for news.
    pub(crate) async fn sync_registration(&self, client: &ClientId, force: bool) -> usize {
        use crate::route::{Registration, StreamState};
        enum Write { Put(String), PeersOnly(String), Delete }
        let etcd = self.etcd.read().await.clone();
        let me = etcd.peers.read().await.me();
        let key = crate::route::consumer_key(&self.fq_name, client);
        let mut writes = 0;
        let mut force = force;
        for _ in 0..4 {
            // [q-dead] an older member here reads the bare node id only: written
            // so, with no generation to tell streams apart, and withdrawn by a
            // delete rather than a tombstone it would keep as a value
            let legacy = etcd.peers.read().await.legacy_registry().await;
            let want = self.registration_here(client, me).await
                .map(|w| if legacy { Registration::new(w.node, 0, StreamState::Receiving) } else { w });
            let render = |r: &Registration| if legacy { r.render_legacy() } else { r.render() };
            let stored = etcd.kv_get(key.as_bytes()).await
                .and_then(|v| Registration::parse(&String::from_utf8_lossy(&v)));
            if let Some(s) = stored {
                if s.node != me && want.map_or(true, |w| s.generation > w.generation) {
                    return writes; // the client is there now
                }
            }
            let stale = stored.filter(|s| s.node == me && want.map_or(true, |w| w.generation != s.generation));
            let write = match (want, stored) {
                _ if !legacy && stale.map_or(false, |s| s.generation > 0) => {
                    let s = stale.unwrap();
                    if self.stream_here(client, s.generation).await {
                        // held here after all - a draining stream behind a
                        // receiving one: the newer one is written over it
                        match want { Some(w) => Write::Put(render(&w)), None => return writes }
                    } else {
                        self.closed_now(client, s.generation).await;
                        Write::Put(Registration::new(me, s.generation, StreamState::Closed).render())
                    }
                }
                (Some(w), s) if s != Some(w) => Write::Put(render(&w)),
                (Some(w), _) if force => Write::PeersOnly(render(&w)),
                (None, Some(s)) if s.node == me && legacy => Write::Delete,
                (None, Some(s)) if s.node == me =>
                    Write::Put(Registration::new(me, s.generation, StreamState::Closed).render()),
                _ => return writes,
            };
            let done = match write {
                Write::Put(v) => put_control(&etcd, &key, &v).await,
                Write::PeersOnly(v) => {
                    let r = broadcast_control(&etcd, &key, &v).await;
                    if r.is_ok() {
                        return writes + 1;
                    }
                    r
                }
                Write::Delete => etcd.kv_delete(key.clone().into_bytes()).await,
            };
            if let Err(e) = done {
                warn!(etcd.log, "{}[q-route] {}", LP, tr!("queue.publish_failed", key = key, err = e));
                return writes;
            }
            writes += 1;
            force = false;
        }
        writes
    }

    /// [q-dead] A consumer's stream closed: drop that watcher - if it is still
    /// the one on that stream, since a watch made again under the same id is a
    /// new one.
    async fn drop_closed(&self, client: &ClientId, watcher: WatcherId, ch: &Sender<Result<WatchResponse, Status>>) {
        let same = match self.clients.read().await.get(client) {
            None => false,
            Some(c) => c.read().await.watchers.get(&watcher).map_or(false, |w| w.client.same_channel(ch)),
        };
        if same {
            let etcd = self.etcd.read().await.clone();
            info!(etcd.log, "{}{}", LP, tr!("queue.consumer_closed", consumer = client, watch = watcher));
            self.drop_watcher(client, watcher, true, Some(ch)).await;
            // [q-dead] and the watch itself, as a cancel takes it: a client that
            // connects again under the same ids is a new stream, not a watch
            // that already exists
            etcd.forget_watch(client, watcher, Some(ch)).await;
        }
    }

    /// Wake the dispatch loop: something changed that may let it deliver.
    pub(crate) fn wake(&self) {
        let _ = self.sender.try_send(0);
    }

    /// [q-dead] Hand out again what matches `which` and is not acknowledged.
    /// Only for a stream that is gone - closed - and never for one that is
    /// merely slow, so a job is not run twice (at-most-once,
    /// `queue-p2p-route.md`). Returns how many.
    async fn release_where<F: Fn(&QueueMsg) -> bool>(&self, which: F, client: &ClientId, log: &Logger) -> usize {
        let n = {
            let mut q = self.queue.write().await;
            let mut n = 0;
            for m in q.iter_mut().filter(|m| m.handled_by.is_some() && which(m)) {
                m.handled_by = None;
                m.stream = None;
                n += 1;
            }
            n
        };
        if n > 0 {
            info!(log, "{}{}", LP, tr!("queue.redeliver", count = n, queue = self.fq_name, consumer = client));
            self.wake();
        }
        n
    }

    /// [q-dead] Mark message `idx` handed to `client`'s `stream`.
    async fn mark(&self, idx: u64, client: ClientId, stream: (crate::cluster::NodeId, u64)) {
        if let Some(m) = self.queue.write().await.iter_mut().find(|m| m.idx == idx) {
            m.handled_by = Some(client);
            m.stream = Some(stream);
        }
    }

    /// [q-dead] Take that mark back after a send that failed - only if it is
    /// still this one: the stream's release may have taken it already.
    async fn unmark(&self, idx: u64, client: ClientId, stream: (crate::cluster::NodeId, u64)) {
        if let Some(m) = self.queue.write().await.iter_mut().find(|m| m.idx == idx) {
            if m.handled_by == Some(client) && m.stream == Some(stream) {
                m.handled_by = None;
                m.stream = None;
            }
        }
    }

    /// [q-dead] What was handed to `client`'s streams on `node` from before
    /// `since`, that node's incarnation now: they died with the process.
    pub(crate) async fn release_older(&self, client: &ClientId, node: crate::cluster::NodeId, since: u64, log: &Logger) -> usize {
        // generation 0, marked while an older member was here: unknown, not older
        self.release_where(|m| m.handled_by.as_ref() == Some(client)
            && m.stream.map_or(false, |(n, g)| n == node && g > 0 && g < since), client, log).await
    }

    /// [q-dead] What was handed to `client`'s stream `generation` on `node`.
    pub(crate) async fn release_stream(&self, client: &ClientId, node: crate::cluster::NodeId, generation: u64, log: &Logger) -> usize {
        self.release_where(|m| m.handled_by.as_ref() == Some(client) && m.stream == Some((node, generation)), client, log).await
    }

    /// [q-dead] Has `node` been down - not answering this node - for longer than
    /// the queue's down timeout (`queue_down_timeout`)?
    async fn down_past(&self, etcd: &EtcdNode, node: crate::cluster::NodeId) -> bool {
        let Some(limit) = etcd.cfg.read().await.down_timeout() else { return false };
        etcd.peers.read().await.down_for(node).await.map_or(false, |d| d >= limit)
    }

    /// [q-dead] Is `client`'s stream `generation` on this node alive - receiving, or
    /// draining?
    async fn stream_here(&self, client: &ClientId, generation: u64) -> bool {
        if let Some(c) = self.clients.read().await.get(client).cloned() {
            if c.read().await.watchers.values().any(|w| w.generation == generation) {
                return true;
            }
        }
        self.draining.read().await.contains_key(&(*client, generation))
    }

    /// [q-dead] The dispatch loop's own check, behind the events: a message
    /// held for a stream that is gone goes back.
    ///
    /// * a stream here: alive while receiving or draining;
    /// * a stream on another node: alive while the registry places the client
    ///   on that node - its node writes the stream's tombstone when it closes.
    ///   Placed on another node, or nowhere, the client left it.
    ///
    /// It catches a stream dropped between its delivery and the mark of it,
    /// and a tombstone or withdrawal this node missed while the client is now
    /// elsewhere. While the registry still places the client on a node that
    /// is down, what it holds waits for that node: back, it withdraws what it
    /// no longer hosts (`EtcdNode::republish_registry`).
    async fn release_gone(&self, log: &Logger) -> usize {
        use crate::route::{Registration, StreamState};
        let held: HashSet<(ClientId, Option<(crate::cluster::NodeId, u64)>)> = self.queue.read().await.iter()
            .filter_map(|m| m.handled_by.map(|c| (c, m.stream))).collect();
        if held.is_empty() {
            return 0;
        }
        let etcd = self.etcd.read().await.clone();
        let me = etcd.peers.read().await.me();
        let mut n = 0;
        for (cid, stream) in held {
            let reg = etcd.kv_get(crate::route::consumer_key(&self.fq_name, &cid).as_bytes()).await
                .and_then(|v| Registration::parse(&String::from_utf8_lossy(&v)))
                .filter(|r| r.state != StreamState::Closed);
            let alive = match stream {
                Some((node, generation)) if node == me => self.stream_here(&cid, generation).await,
                // [q-dead] ...and from this incarnation of it: one from before
                // a restart has nobody left to write its tombstone
                Some((node, generation)) => reg.map_or(false, |r| r.node == node && !r.outlives(node, generation)),
                // marked before streams had generations: alive here, or
                // registered on another node
                None => self.clients.read().await.contains_key(&cid)
                    || self.draining.read().await.keys().any(|(c, _)| *c == cid)
                    || reg.map_or(false, |r| r.node != me),
            };
            if !alive {
                n += self.release_where(|m| m.handled_by == Some(cid) && m.stream == stream, &cid, log).await;
                continue;
            }
            // [q-dead] alive on a node down past the down timeout: gone, as its
            // node cannot say - withdrawn on its behalf, and what it held to the
            // next consumer (at least once: a node only slow may run it too)
            if let Some((node, generation)) = stream.filter(|(node, _)| *node != me) {
                if self.down_past(&etcd, node).await {
                    info!(log, "{}[q-route] {}", LP, tr!("queue.node_down", node = node, consumer = cid, queue = self.fq_name));
                    let key = crate::route::consumer_key(&self.fq_name, &cid);
                    withdraw_gone(&etcd, &key, node, generation).await;
                    n += self.release_stream(&cid, node, generation, log).await;
                }
            }
        }
        n
    }

    pub(crate) async fn new(etcd: &EtcdNode, qn: &QueueNameKey) -> Self {
        let (sender, rsvr) = mpsc::channel(100);
        Queue {
            fq_name: format!("/{}/{}", qn.prefix, qn.queue_name),
            etcd: Arc::new(RwLock::new(etcd.clone())),
            dispatch: Arc::new(RwLock::new(crate::route::Dispatch::Unknown)),
            record: Arc::new(RwLock::new(None)),
            pin_seen: Arc::new(RwLock::new(None)),
            idle_since: Arc::new(RwLock::new(Some(Instant::now()))),
            queue: Arc::new(Default::default()),
            // [q-idx] from this dispatcher's start, not 1: indexes restarting
            // at 1 after a restart or a move of the dispatcher let a late
            // acknowledge of an older message name a new one
            idx: Arc::new(AtomicU64::new(first_idx())),
            // delivery: Arc::new(Default::default()),
            clients: Arc::new(Default::default()),
            closers: Arc::new(Default::default()),
            draining: Arc::new(Default::default()),
            closed: Arc::new(Default::default()),
            #[cfg(feature = "clickhouse")]
            ch_table: crate::clickhouse::queue_target(&qn.queue_name).map(|t| t.to_string()),
            sender
        }.run(rsvr).await
    }

    pub(crate) async fn get(&self, qn: &QueueNameKey) -> Option<KeyValue> {
        if let Some(idx) = qn.idx {
            for i in self.queue.read().await.iter().rev() {
                if i.idx == idx {
                    return Some(i.into());
                }
            }
        }
        None
    }

    /// EtcdNode required to notify consumers
    pub(crate) async fn put(&self, qn: QueueNameKey, x: PutRequest, from_peer: &Option<String>, _log: &Logger) -> Result<Response<PutResponse>, Status> {
        // [q-route Phase 2] A DELIVERY arriving from the dispatcher
        // (`/q/{name}/c/{cid}/{idx}/{key}`) is not a new message: it is this
        // node being told to hand one to a consumer it hosts. Enqueuing it
        // would put a second copy in a second queue, and the ack - which
        // deletes by idx on the DISPATCHER - would never reach it.
        if from_peer.is_some() && qn.consumer {
            if let Some(cid) = qn.client_id {
                let kv = KeyValue { key: qn.input.clone().into_bytes(), value: x.value.clone(),
                    ..Default::default() };
                // [q-dead] the stream it is for rides in `lease` (see the
                // dispatch loop), as a delivery has no other field to spare
                //
                // In a task of its own, awaited: the dispatcher drops this call
                // when it gives up on it, and what was begun - a slow consumer
                // dropped and told - is finished all the same.
                let (queue, log, generation) = (self.clone(), _log.clone(), x.lease.max(0) as u64);
                let ok = tokio::spawn(async move { queue.deliver_local(&cid, generation, kv, &log).await })
                    .await.unwrap_or(false);
                if !ok {
                    // The registry said we host this consumer and we do not.
                    // Refusing is what lets the dispatcher try another one
                    // rather than count it delivered.
                    return Err(Status::not_found(tr!("queue.no_consumer_here", consumer = cid, queue = self.fq_name)));
                }
                return Ok(Response::new(PutResponse::default()));
            }
        }

        self.touched().await;
        let dispatch = self.dispatch().await;
        // [q-idx] lazily: a put that carries its index takes none from the
        // counter, so a node that only sees messages moves no index on
        let idx = qn.idx.unwrap_or_else(|| self.idx.fetch_add(1, Ordering::Relaxed));
        let msg = QueueMsg {
            idx,
            key: if from_peer.is_some() { qn.input.clone() }else{format!("{}/{}/{}", self.fq_name, idx, qn.tail)},
            value: x.value.clone(),
            created: Instant::now(),
            handled_by: None,
            stream: None,
        };

        // **Only the dispatcher keeps the message.** A producer's node forwards
        // and forgets.
        //
        // Keeping a copy everywhere was harmless while the dispatch loop could
        // only see LOCAL consumers - a producer-only node had no candidates and
        // fell straight out. Once the consumer registry let every node see
        // every consumer, that same copy made every node a dispatcher: the
        // producer's node and the real dispatcher would each deliver, and the
        // consumer would get the message TWICE. It would also have leaked, since
        // the ack deletes by idx on the dispatcher only.
        if matches!(dispatch, crate::route::Dispatch::Remote(_)) && from_peer.is_none() {
            trace!(_log, "{}[q-route] {}", LP, tr!("queue.forwarding", idx = idx, queue = self.fq_name));
        } else {
            let depth = { let mut q = self.queue.write().await; q.push_front(msg); q.len() };
            let notified = self.sender.try_send(idx);
            debug!(_log, "{}{}", LP, tr!("queue.queued", idx = idx, depth = depth,
                notify = format!("{:?}", notified.as_ref().map(|_| "ok").map_err(|e| e.to_string())), cap = self.sender.capacity()));
        }

        // [q-route] Replicate the message only as far as it has to go.
        //
        //   Local   this node dispatches: nothing leaves it (Phase 5). A
        //           co-located producer/dispatcher/consumer is the single-node
        //           and dev case, and it must never touch the network.
        //   Remote  one unicast hop to the dispatcher.
        //   Unknown no registry answer: broadcast, as before, and SAY SO. That
        //           path used to be the default for every message on every
        //           cluster; it is now the failure mode, and a log line is what
        //           makes the difference visible.
        //
        // A message arriving FROM a peer is already where it belongs and is
        // never re-sent, which is what stops a claim race becoming a loop.
        if from_peer.is_none() {
            let r = PutRequest {
                key: qn.input.into_bytes(),
                value: x.value,
                lease: 0,
                prev_kv: false,
                ignore_value: true,
                ignore_lease: true,
            };
            let etcd = self.etcd.read().await.clone();
            let zone = {
                let policy = etcd.policy.read().await;
                if policy.is_zone_scoped(&r.key) {
                    Some(etcd.peers.read().await.my_zone().to_string())
                } else { None }
            };
            match dispatch {
                crate::route::Dispatch::Local => {}
                crate::route::Dispatch::Remote(node) => {
                    // [lockup] never under `peers.read()`: see `peer::BroadcastPlan`
                    let sent = etcd
                        .unicast(BroadcastRequest::Kv(KvEvent::Put(r.clone())), node,
                            zone.as_deref()).await;
                    if !sent {
                        // Unreachable, not Online, or excluded by the zone
                        // test. Broadcasting rather than dropping is the safe
                        // direction, and re-electing on the next touch is what
                        // stops it being permanent.
                        warn!(_log, "{}[q-route] {}", LP, tr!("queue.dispatcher_unreachable", node = node, queue = self.fq_name));
                        *self.dispatch.write().await = crate::route::Dispatch::Unknown;
                        let _ = etcd
                            .broadcast_scoped(BroadcastRequest::Kv(KvEvent::Put(r)),
                                zone.as_deref()).await?;
                    }
                }
                crate::route::Dispatch::Unknown => {
                    debug!(_log, "{}[q-route] {}", LP, tr!("queue.no_dispatcher", queue = self.fq_name));
                    let _ = etcd
                        .broadcast_scoped(BroadcastRequest::Kv(KvEvent::Put(r)),
                            zone.as_deref()).await?;
                }
            }
        }

        Ok(Response::new(PutResponse::default()))
    }


    /// start a thread to dispatch a queue messages
    async fn run(self, mut rsvr: Receiver<MsgNotifyType>) -> Self {
        let log = self.etcd.read().await.log.clone();
        let queue = self.clone();
        tokio::spawn(async move {
            // The received value is only a wake-up. It used to be matched against
            // the head of the queue (`if x.idx > 0 && r != x.idx { continue }`),
            // which lined up only while exactly one message was outstanding:
            // `put` notifies with the idx it just pushed to the FRONT, while the
            // dispatcher reads the OLDEST from the back, so the second producer
            // put stalled the queue permanently. It also threw away the kick that
            // `delete` sends after an acknowledge, since 0 never equals a real
            // idx - so even draining the queue never restarted delivery.
            #[cfg(feature = "clickhouse")]
            let ch_table = queue.ch_table.clone();

            // [q-dead] While the queue holds anything, the wait for a wake-up is
            // bounded and the loop looks again on its own (`REDISPATCH`).
            let mut look_again: Option<std::time::Duration> = None;
            loop {
                let woke = match look_again {
                    None => match rsvr.recv().await {
                        Some(_) => true,
                        None => break,
                    },
                    Some(wait) => match tokio::time::timeout(wait, rsvr.recv()).await {
                        Ok(Some(_)) => true,
                        Ok(None) => break,
                        Err(_) => false,
                    },
                };

                // A ch: queue consumes itself: take everything pending, write it
                // as one JSONEachRow batch, and drop the messages that landed.
                // Nothing else in this loop applies - there is no watcher to pick.
                #[cfg(feature = "clickhouse")]
                if let Some(table) = &ch_table {
                    queue.drain_to_clickhouse(table, &log).await;
                    continue;
                }

                // [q-dead] what a gone consumer was handed goes back first, so
                // the pass below hands it out again, oldest first as ever
                let mut progress = queue.release_gone(&log).await > 0;

                // drain every message no consumer has been handed yet, oldest
                // first (newest is at the front, so walk from the back)
                loop {
                    let next = {
                        let q = queue.queue.read().await;
                        next_undelivered(&q).and_then(|idx| q.iter()
                            .find(|m| m.idx == idx)
                            .map(|m| (idx, m.key.clone(), KeyValue::from(m), q.len())))
                    };
                    let Some((idx, key, kv, depth)) = next else { break };

                    // TODO queue: implement picking best consumer strategy
                    let candidates: Vec<(ClientId, WatcherId, u64, Sender<Result<WatchResponse, Status>>)> = {
                        let clients = queue.clients.read().await;
                        let mut v = Vec::new();
                        for (cid, c) in clients.iter() {
                            for (wid, w) in c.read().await.watchers.iter() {
                                v.push((*cid, *wid, w.generation, w.client.clone()));
                            }
                        }
                        v
                    };
                    // [q-route Phase 2] Consumers on other nodes, from the
                    // registry. Tried only AFTER every local one: a local
                    // delivery is a channel send and a remote one is a network
                    // round trip, so preferring local is the direct-dispatch
                    // short circuit in the one place it matters.
                    //
                    // **Only the dispatcher fans out.** Without this test every
                    // node holding a copy would deliver to the same consumer -
                    // the registry tells them all where it is. One dispatcher
                    // is what makes `handled_by` mean anything.
                    //
                    // In the `Unknown` fallback nobody fans out, which is
                    // exactly the pre-registry behaviour: a broadcast put a
                    // copy on every node, and only the node actually hosting
                    // the consumer delivered it.
                    let remote = if queue.dispatch().await.is_local() {
                        queue.remote_consumers().await
                    } else {
                        Vec::new()
                    };

                    if candidates.is_empty() && remote.is_empty() {
                        debug!(log, "{}{}", LP, tr!("queue.no_consumer_yet", key = key, depth = depth));
                        break; // the next put, ack or watcher registration wakes us
                    }

                    // Try each consumer in turn, and never block on one of them:
                    // a watcher whose stream is gone or whose channel nobody is
                    // draining would otherwise wedge the whole queue for good,
                    // since `iter().next()` keeps handing back the same dead
                    // entry. Anything that fails or stalls is dropped here and
                    // the next producer put or watcher registration re-adds a
                    // live one.
                    let mut delivered = false;
                    let me = queue.etcd.read().await.peers.read().await.me();
                    for (cid, wid, generation, client) in candidates {
                        let resp = WatchResponse {
                            header: None,
                            watch_id: wid,
                            created: false,
                            canceled: false,
                            compact_revision: 0,
                            cancel_reason: "".to_string(),
                            fragment: false,
                            events: vec![ Event {
                                r#type: 0, // put
                                kv: Some(kv.clone()),
                                prev_kv: None,
                            }],
                        };
                        // [q-dead] marked before it is sent: the stream can close,
                        // and its release run, before the send returns
                        queue.mark(idx, cid, (me, generation)).await;
                        match tokio::time::timeout(DISPATCH_TIMEOUT, client.send(Ok(resp))).await {
                            Ok(Ok(())) => {
                                debug!(log, "{}{}", LP, tr!("queue.dispatched", key = key, depth = depth));
                                delivered = true;
                                break;
                            }
                            Ok(Err(_)) => {
                                queue.unmark(idx, cid, (me, generation)).await;
                                warn!(log, "{}{}", LP, tr!("queue.watcher_gone", consumer = cid, watch = wid));
                                queue.drop_watcher(&cid, wid, true, Some(&client)).await;
                            }
                            Err(_) => {
                                queue.unmark(idx, cid, (me, generation)).await;
                                // [q-dead] slow, not gone: it keeps what it has
                                warn!(log, "{}{}", LP, tr!("queue.watcher_slow", consumer = cid, watch = wid, key = key,
                                    timeout = format!("{:?}", DISPATCH_TIMEOUT)));
                                queue.drop_watcher(&cid, wid, false, Some(&client)).await;
                                queue.cancel_stalled(&cid, wid, client);
                            }
                        }
                    }
                    // [q-route Phase 2] Nothing local took it; offer it to a
                    // consumer on another node. One unicast, to the node the
                    // registry names - not a broadcast to everyone in the hope
                    // that whoever hosts it notices.
                    if !delivered {
                        for (cid, node, generation) in remote {
                            let etcd = queue.etcd.read().await.clone();
                            let dkey = format!("{}/c/{}/{}/{}",
                                queue.fq_name, cid, idx, key.rsplit('/').next().unwrap_or(""));
                            let zone = {
                                let policy = etcd.policy.read().await;
                                if policy.is_zone_scoped(dkey.as_bytes()) {
                                    Some(etcd.peers.read().await.my_zone().to_string())
                                } else { None }
                            };
                            // [q-dead] `lease` names the stream it is for: the
                            // host hands it to that one, whose tombstone then
                            // releases it here
                            let req = PutRequest {
                                key: dkey.into_bytes(),
                                value: kv.value.clone(),
                                lease: generation.min(i64::MAX as u64) as i64, prev_kv: false, ignore_value: true, ignore_lease: true,
                            };
                            queue.mark(idx, cid, (node, generation)).await;
                            // [lockup] never under `peers.read()`
                            match etcd.unicast_outcome(BroadcastRequest::Kv(KvEvent::Put(req)), node, zone.as_deref()).await {
                                crate::peer::Sent::Taken => {
                                    debug!(log, "{}[q-route] {}", LP, tr!("queue.dispatched_remote", key = key, consumer = cid, node = node));
                                    delivered = true;
                                    break;
                                }
                                // [q-dead] Taken or not, nobody here knows: count it
                                // handed to that stream. Handed out again on a guess
                                // it could run twice; held, it goes back when that
                                // stream closes.
                                crate::peer::Sent::NoAnswer => {
                                    warn!(log, "{}[q-route] {}", LP, tr!("queue.remote_no_answer", node = node, key = key, consumer = cid));
                                    delivered = true;
                                    break;
                                }
                                crate::peer::Sent::Refused | crate::peer::Sent::Unreachable =>
                                    queue.unmark(idx, cid, (node, generation)).await,
                            }
                            // [q-dead] said once per event: a look of the loop's
                            // own repeats a refusal already logged
                            if woke {
                                warn!(log, "{}[q-route] {}", LP, tr!("queue.remote_refused", node = node, key = key, consumer = cid));
                            } else {
                                debug!(log, "{}[q-route] {}", LP, tr!("queue.remote_refused", node = node, key = key, consumer = cid));
                            }
                        }
                    }

                    if !delivered {
                        // no consumer took it; leave it queued for the next one
                        break;
                    }
                    // Marked above, so the next wake moves on instead of
                    // re-sending the same message; the consumer's acknowledge
                    // (delete) is what drops it for good.
                    //
                    // [q-dead] Not for good if its stream closes: that stream's
                    // close or tombstone hands it out again (`release_stream`),
                    // and `release_gone` above catches the rest. Not on a
                    // timeout - a slow consumer is not a gone one, and handing
                    // its job to another would run it twice.
                    progress = true;
                }

                // [q-route Phase 3] With the queue drained, consider moving the
                // dispatcher onto its consumer's node. Here rather than on a
                // timer because this is the one moment the guard "nothing
                // outstanding" is most likely to hold, and because the
                // dispatcher is the only node that can evaluate it.
                queue.pin_to_consumer(&log).await;

                // [q-dead] Look again soon after a wake-up or a delivery, later
                // and later after a look that found nothing to do, and not at
                // all while nothing is held.
                look_again = if queue.queue.read().await.is_empty() {
                    None
                } else if woke || progress {
                    Some(REDISPATCH)
                } else {
                    Some(look_again.map_or(REDISPATCH, |w| (w * 2).min(REDISPATCH_MAX)))
                };
            }
        });
        self
    }


    // TODO queue: - implement queue acknowledge auth, allow only logged in clientId to cleanup only his dispatcher message
    pub(crate) async fn delete(&self, request: &QueueNameKey, _from_peer: &Option<String>, log: &Logger) {
        // [q-dead] A consumer's registration deleted - by its host, the delete
        // guard in `delete_impl` sees to that; an older node withdraws so
        // rather than by a tombstone. What was handed to the client's streams
        // on that node goes to the next consumer; a stream here that is still
        // alive keeps its own.
        if let Some(cid) = crate::route::consumer_of(&request.input) {
            let etcd = self.etcd.read().await.clone();
            let me = etcd.peers.read().await.me();
            let host = _from_peer.as_deref().and_then(|p| p.trim().parse::<crate::cluster::NodeId>().ok()).unwrap_or(me);
            let live: Vec<u64> = {
                let mut live: Vec<u64> = self.draining.read().await.keys().filter(|(c, _)| *c == cid).map(|(_, g)| *g).collect();
                if let Some(c) = self.clients.read().await.get(&cid).cloned() {
                    live.extend(c.read().await.watchers.values().map(|w| w.generation));
                }
                live
            };
            self.release_where(|m| m.handled_by == Some(cid) && match m.stream {
                Some((n, g)) => n == host && !(n == me && live.contains(&g)),
                None => !(host == me && !live.is_empty()),
            }, &cid, log).await;
        }
        let q = if let Some(idx) = request.idx {
            let mut q = self.queue.write().await;
            // Remove ONLY the acknowledged message. `> idx` treated every ack as
            // cumulative, but a consumer acknowledges one specific key, and they
            // come back out of order (a burst acks e.g. 2,3,1,8,9,7,...). Acking
            // a high idx therefore silently discarded every lower message still
            // waiting to be dispatched - a steady, load-dependent message loss.
            // [q-idx] and only the message with that key: an index alone may
            // be one an older dispatcher gave out
            q.retain(|v| !acknowledges(v, idx, &request.tail)); // TODO optimize for big queue size
            q.len() as i64
        } else {-1};
        debug!(log, "{}{}", LP, tr!("queue.ack", key = request.input, idx = format!("{:?}", request.idx), size = q, cap = self.sender.capacity()));
        let _ = self.sender.try_send(0);
    }

    // ─── [q-route Phase 2] the consumer -> host registry ───────────────────

    /// Consumers this queue has anywhere in the cluster, and where they live.
    ///
    /// Local ones are excluded: the dispatch loop already has their channels
    /// and delivering to them costs nothing, so including them here would only
    /// offer the network as an alternative to a local send.
    ///
    /// [q-dead] With the stream each is registered for: a delivery names it,
    /// so the host hands it to that stream and its tombstone releases it. Only
    /// receiving streams - a draining one takes nothing new.
    async fn remote_consumers(&self) -> Vec<(ClientId, crate::cluster::NodeId, u64)> {
        use crate::route::{Registration, StreamState};
        let etcd = self.etcd.read().await.clone();
        let me = etcd.peers.read().await.me();
        let local: Vec<ClientId> = self.clients.read().await.keys().copied().collect();
        let prefix = format!("{}/c/", self.fq_name);
        let mut out = Vec::new();
        for (k, v) in etcd.kv_prefix(prefix.as_bytes()).await {
            let key = String::from_utf8_lossy(&k).to_string();
            let Some(cid) = crate::route::consumer_of(&key) else { continue };
            if local.contains(&cid) {
                continue;
            }
            let Some(r) = Registration::parse(&String::from_utf8_lossy(&v))
                .filter(|r| r.state == StreamState::Receiving) else { continue };
            let (node, generation) = (r.node, r.generation);
            // [q-dead] a node not answering now gets nothing: a call there would
            // wait the call's timeout, and count it handed on no answer
            if etcd.peers.read().await.down_for(node).await.is_some() {
                continue;
            }
            if node == me {
                // Our own registration, left behind by a watcher that went away
                // without withdrawing. Routing to ourselves over the network
                // would be a loop.
                continue;
            }
            out.push((cid, node, generation));
        }
        out.sort_by_key(|(_, n, _)| *n);
        out
    }

    /// Does `node` host any consumer of this queue?
    ///
    /// The test that makes the dispatcher an **endpoint**: a node that hosts no
    /// consumer is a third party, and routing through it costs an extra hop to
    /// every producer.
    async fn node_hosts_consumer(&self, node: crate::cluster::NodeId) -> bool {
        let etcd = self.etcd.read().await.clone();
        let me = etcd.peers.read().await.me();
        if node == me && !self.clients.read().await.is_empty() {
            return true;
        }
        let prefix = format!("{}/c/", self.fq_name);
        for (k, v) in etcd.kv_prefix(prefix.as_bytes()).await {
            if crate::route::consumer_of(&String::from_utf8_lossy(&k)).is_none() {
                continue;
            }
            // [q-dead] a receiving consumer there - a draining one takes nothing
            if crate::route::Registration::parse(&String::from_utf8_lossy(&v))
                .map_or(false, |r| r.node == node && r.state == crate::route::StreamState::Receiving) {
                return true;
            }
        }
        false
    }

    /// Deliver one message to a locally-hosted consumer's watchers.
    ///
    /// The receiving end of a remote dispatch: the dispatcher unicasts the
    /// delivery key to the node the registry names, and that node's `put` lands
    /// here instead of enqueuing a second copy.
    ///
    /// [q-dead] To the stream `generation` the dispatcher handed it to - that stream's
    /// tombstone is what releases it there - or to any with 0, from an older
    /// node. That stream gone, it is refused.
    async fn deliver_local(&self, client: &ClientId, generation: u64, kv: KeyValue, log: &Logger) -> bool {
        let watchers: Vec<(WatcherId, Sender<Result<WatchResponse, Status>>)> = {
            let clients = self.clients.read().await;
            match clients.get(client) {
                None => Vec::new(),
                Some(c) => c.read().await.watchers.iter()
                    .filter(|(_, w)| generation == 0 || w.generation == generation)
                    .map(|(wid, w)| (*wid, w.client.clone())).collect(),
            }
        };
        for (wid, ch) in watchers {
            let resp = WatchResponse {
                header: None,
                watch_id: wid,
                created: false,
                canceled: false,
                compact_revision: 0,
                cancel_reason: "".to_string(),
                fragment: false,
                events: vec![Event { r#type: 0, kv: Some(kv.clone()), prev_kv: None }],
            };
            match tokio::time::timeout(REMOTE_DISPATCH_TIMEOUT, ch.send(Ok(resp))).await {
                Ok(Ok(())) => return true,
                sent => {
                    warn!(log, "{}[q-route] {}", LP, tr!("queue.local_refused", consumer = client, watch = wid));
                    // [q-dead] a closed stream is gone; a slow one keeps what it has
                    let gone = matches!(sent, Ok(Err(_)));
                    self.drop_watcher(client, wid, gone, Some(&ch)).await;
                    if !gone {
                        self.cancel_stalled(client, wid, ch);
                    }
                }
            }
        }
        false
    }

    /// [q-route] Where this queue's traffic goes right now.
    pub(crate) async fn dispatch(&self) -> crate::route::Dispatch {
        *self.dispatch.read().await
    }

    /// [q-route] Resolve the dispatcher, electing this node if there is none or
    /// the recorded one is not usable.
    ///
    /// Called on every first touch of a queue - `get_or_create_queue` and
    /// `create_watcher` - and cheap enough to call again: the common path is a
    /// vault read and a comparison.
    ///
    /// **Last-writer-wins is acceptable here and the reason is worth stating:**
    /// two nodes claiming at once cost an extra hop for the loser's traffic
    /// until the registry converges, not a lost or duplicated message. Delivery
    /// ownership is `handled_by`, which this does not touch. Paying for a
    /// consensus round to save one hop would be the wrong trade.
    pub(crate) async fn elect(&self, why: crate::route::Claim, log: &Logger)
        -> crate::route::Dispatch
    {
        use crate::route::{Claim, Dispatch, DispatchRecord, dispatcher_key};

        let etcd = self.etcd.read().await.clone();
        let me = etcd.peers.read().await.me();
        let key = dispatcher_key(&self.fq_name);

        let found = etcd.kv_get(key.as_bytes()).await
            .and_then(|v| DispatchRecord::parse(&String::from_utf8_lossy(&v)));

        // Usable means: it is us, or it is a peer that is Online. An
        // `InProgress` peer receives writes but is still syncing, and a
        // dispatcher has to index and route rather than merely store.
        let usable = match &found {
            Some(r) if r.node == me => true,
            Some(r) => etcd.peers.read().await.is_online(r.node).await,
            None => false,
        };

        // **The dispatcher is an ENDPOINT, and a consumer endpoint wins.**
        //
        // A dispatcher placed at neither end costs two hops: producer node ->
        // dispatcher -> consumer node. Placed at the consumer's node it costs
        // ONE, for every producer at once - which is the whole point of p2p
        // routing, and why a consumer's node takes the queue over from a
        // producer's rather than waiting for the Phase 3 hysteresis.
        //
        // A producer claims only an unclaimed queue. Two consumers on two nodes
        // leave the first in place: there is no single right answer then, and
        // `pin_to_consumer` deliberately declines to pick one.
        let hosts_consumer = match &found {
            Some(r) if usable => self.node_hosts_consumer(r.node).await,
            _ => false,
        };
        let take_over = matches!(why, Claim::Consumer) && !hosts_consumer;

        if usable && !take_over {
            let d = Dispatch::of(found.as_ref(), me);
            *self.record.write().await = found;
            self.set_dispatch(d).await;
            return d;
        }
        if take_over {
            if let Some(r) = &found {
                info!(log, "{}[q-route] {}", LP, tr!("queue.take_over", key = key, from = r.node, to = me));
            }
        }

        // Claim it. A record naming a peer that has gone Spare is REPLACED, not
        // kept: `queue-p2p-route.md` is explicit that a pinned dispatcher on a
        // demoted peer must be re-elected rather than silently used.
        if let (Some(r), false) = (&found, take_over) {
            warn!(log, "{}[q-route] {}", LP, tr!("queue.reelect", key = key, node = r.node));
        }
        let claim = DispatchRecord {
            node: me,
            // Phase 3 fields survive a re-election: the LINK is a property of
            // the queue pair, not of whichever node happens to dispatch it.
            reply_to: found.as_ref().and_then(|r| r.reply_to.clone()),
            pinned: false,
        };
        if let Err(e) = put_control(&etcd, &key, &claim.render()).await {
            warn!(log, "{}[q-route] {}", LP, tr!("queue.claim_failed", key = key, err = e));
            *self.dispatch.write().await = Dispatch::Unknown;
            return Dispatch::Unknown;
        }
        // Re-read to confirm: another node may have claimed it in the same
        // instant, and adopting its answer is one comparison against a hop on
        // every subsequent message.
        let confirmed = etcd.kv_get(key.as_bytes()).await
            .and_then(|v| DispatchRecord::parse(&String::from_utf8_lossy(&v)))
            .unwrap_or(claim);
        let d = Dispatch::of(Some(&confirmed), me);
        debug!(log, "{}[q-route] {}", LP, tr!("queue.dispatcher", key = key, dispatch = format!("{:?}", d)));
        *self.record.write().await = Some(confirmed);
        self.set_dispatch(d).await;
        d
    }

    /// [q-idx] A node that becomes the dispatcher gives out indexes from now
    /// on, not from when its `Queue` was made: it may have been made long
    /// before, on a producer's or a consumer's node, and its counter be below
    /// the indexes the dispatcher before it gave out.
    async fn set_dispatch(&self, d: crate::route::Dispatch) {
        let mut cur = self.dispatch.write().await;
        if d.is_local() && !cur.is_local() {
            self.idx.fetch_max(first_idx(), Ordering::Relaxed);
        }
        *cur = d;
    }

    /// Is this queue finished with - empty, unwatched, and idle?
    ///
    /// Four guards, and each is a way the obvious version loses a message:
    ///
    /// * **No messages.** Including dispatched-but-unacked ones: the ack
    ///   deletes by idx *here*, so reaping would strand a consumer mid-work.
    /// * **No local consumers.** A watcher is a live client stream.
    /// * **No consumers anywhere.** The registry is cluster-wide, so a queue
    ///   whose only consumer sits on another node is in use even though nothing
    ///   is attached here.
    /// * **Idle for [`QUEUE_IDLE`].** A queue between bursts looks identical to
    ///   a finished one at any single instant.
    async fn is_reapable(&self) -> bool {
        if !self.queue.read().await.is_empty() {
            return false;
        }
        if !self.clients.read().await.is_empty() {
            return false;
        }
        if !self.remote_consumers().await.is_empty() {
            return false;
        }
        match *self.idle_since.read().await {
            Some(t) => t.elapsed() >= QUEUE_IDLE,
            None => false,
        }
    }

    /// Note that the queue has just been used, so the idle clock restarts.
    async fn touched(&self) {
        *self.idle_since.write().await = Some(Instant::now());
    }

    /// Remove this queue's registry keys.
    ///
    /// The dispatcher record only - consumer registrations are withdrawn by
    /// whichever node held them, and a node deleting another's would be
    /// asserting something it cannot know.
    async fn forget_registry(&self, log: &Logger) {
        let etcd = self.etcd.read().await.clone();
        let key = crate::route::dispatcher_key(&self.fq_name);
        if let Err(e) = etcd.kv_delete(key.clone().into_bytes()).await {
            warn!(log, "{}[q-route] {}", LP, tr!("queue.clear_failed", key = key, err = e));
        }
    }

    /// [q-route Phase 3] **Relocate** the dispatcher onto its consumer's node.
    ///
    /// Placement is not this function's job: a consumer's node claims the queue
    /// the moment it creates the watch (`Claim::Consumer` in `make_consumer`),
    /// so the dispatcher is already at an endpoint. This handles the case that
    /// placement cannot - a consumer that **moved**, leaving the dispatcher on
    /// a node that no longer hosts it, which is a third party again and costs
    /// every producer the extra hop.
    ///
    /// Three guards, and each exists for a failure rather than for neatness:
    ///
    /// * **One consumer only.** With several, there is no single right place,
    ///   and pinning to one of them would make the others worse. Affinity is an
    ///   optimization, never a constraint on where a consumer may live.
    /// * **Stable for [`PIN_STABLE`].** Without it a reconnecting consumer drags
    ///   the dispatcher with it and churns a replicated key cluster-wide.
    /// * **Nothing outstanding.** Moving the dispatcher while messages are
    ///   dispatched-but-unacked would strand them: `handled_by` and the queue
    ///   they live in are on the old node, and the ack deletes by idx there.
    ///   Nor while one waits for a consumer: the old node keeps it and, no
    ///   longer the dispatcher, hands it to no other node.
    ///
    /// Leader-only in effect - it does nothing unless this node is the current
    /// dispatcher, because the dispatcher is the one node that can know whether
    /// anything is outstanding.
    async fn pin_to_consumer(&self, log: &Logger) {
        use crate::route::{Dispatch, DispatchRecord, dispatcher_key};

        if !self.dispatch().await.is_local() {
            return;
        }
        let etcd = self.etcd.read().await.clone();
        let me = etcd.peers.read().await.me();

        // Exactly one consumer, anywhere.
        let local: Vec<ClientId> = self.clients.read().await.keys().copied().collect();
        let remote = self.remote_consumers().await;
        let target = match (local.len(), remote.len()) {
            (1, 0) => me,                 // already here: nothing to move
            (0, 1) => remote[0].1,
            _ => {
                *self.pin_seen.write().await = None;
                return;
            }
        };
        if target == me {
            *self.pin_seen.write().await = None;
            return;
        }
        if !etcd.peers.read().await.is_online(target).await {
            return;
        }

        // Stable for long enough?
        let stable = {
            let mut seen = self.pin_seen.write().await;
            match *seen {
                Some((n, since)) if n == target => since.elapsed() >= PIN_STABLE,
                _ => {
                    *seen = Some((target, Instant::now()));
                    false
                }
            }
        };
        if !stable {
            return;
        }

        // Nothing dispatched-but-unacked - and nothing waiting either. The loop
        // comes here after a pass that left a message no consumer took, and a
        // dispatcher moved away from it then leaves it on a node that no
        // longer hands anything to another: held for good.
        if !self.queue.read().await.is_empty() {
            return;
        }

        let mut r = self.record.read().await.clone()
            .unwrap_or_else(|| DispatchRecord::new(me));
        r.node = target;
        r.pinned = true;
        let key = dispatcher_key(&self.fq_name);
        if let Err(e) = put_control(&etcd, &key, &r.render()).await {
            warn!(log, "{}[q-route] {}", LP, tr!("queue.pin_failed", key = key, node = target, err = e));
            return;
        }
        info!(log, "{}[q-route] {}", LP, tr!("queue.pinned", key = key, node = target));
        *self.record.write().await = Some(r);
        *self.dispatch.write().await = Dispatch::Remote(target);
        *self.pin_seen.write().await = None;
    }

    /// [q-route Phase 3] Declare this queue the reply half of a linked pair.
    ///
    /// Sets `reply_to` on the record so the two dispatchers can be placed
    /// together. Idempotent; a no-op when the link is already recorded.
    pub(crate) async fn link_reply_to(&self, other: &str, log: &Logger) {
        use crate::route::{DispatchRecord, dispatcher_key};
        let current = self.record.read().await.clone();
        if current.as_ref().and_then(|r| r.reply_to.as_deref()) == Some(other) {
            return;
        }
        let etcd = self.etcd.read().await.clone();
        let me = etcd.peers.read().await.me();
        let mut r = current.unwrap_or_else(|| DispatchRecord::new(me));
        r.reply_to = Some(other.to_string());
        let key = dispatcher_key(&self.fq_name);
        if let Err(e) = put_control(&etcd, &key, &r.render()).await {
            warn!(log, "{}[q-route] {}", LP, tr!("queue.link_failed", key = key, other = other, err = e));
            return;
        }
        debug!(log, "{}[q-route] {}", LP, tr!("queue.linked", key = key, other = other));
        *self.record.write().await = Some(r);
    }

    /// [q-registry] Write this node's part of the queue's registry again, for
    /// peers that were not receiving when it was first written. Returns how
    /// many keys.
    ///
    /// * **Each client with a stream here** - receiving, or draining - as
    ///   `sync_registration` has it: from the streams this node holds, not from
    ///   the store; never over a newer registration of another node, where the
    ///   client moved; read again after the write. A registration naming this
    ///   node that it no longer holds is withdrawn by its tombstone - one a
    ///   restart pulled back from a peer, or one a peer missed the withdrawal of.
    /// * **The tombstones of streams closed lately** (`closed`), for a peer that
    ///   missed them: what it holds for those streams goes back.
    /// * **The dispatcher record**, `/q/{name}` - only while the store still
    ///   names this node, so a claim another node made since is not undone;
    ///   and only with a consumer here. A consumer's node is the endpoint that
    ///   should dispatch; a producer's claim written over it would cost every
    ///   other producer a hop.
    async fn republish(&self, log: &Logger) -> usize {
        use crate::route::{consumer_key, dispatcher_key, DispatchRecord, Registration, StreamState};
        let etcd = self.etcd.read().await.clone();
        let me = etcd.peers.read().await.me();
        let mut cids: HashSet<ClientId> = self.clients.read().await.keys().copied().collect();
        cids.extend(self.draining.read().await.keys().map(|(c, _)| *c));
        for (k, v) in etcd.kv_prefix(format!("{}/c/", self.fq_name).as_bytes()).await {
            let Some(cid) = crate::route::consumer_of(&String::from_utf8_lossy(&k)) else { continue };
            if Registration::parse(&String::from_utf8_lossy(&v)).map_or(false, |r| r.node == me) {
                cids.insert(cid);
            }
        }
        let mut n = 0;
        for cid in &cids {
            n += self.sync_registration(cid, true).await;
        }
        // [q-dead] none to an older member, which would keep them as values
        let legacy = etcd.peers.read().await.legacy_registry().await;
        let closed: Vec<(ClientId, u64)> = {
            let mut closed = self.closed.write().await;
            closed.retain(|(_, _, t)| t.elapsed() < QUEUE_IDLE);
            closed.iter().filter(|_| !legacy).map(|(c, g, _)| (*c, *g)).collect()
        };
        for (cid, generation) in closed {
            let key = consumer_key(&self.fq_name, &cid);
            match put_control(&etcd, &key, &Registration::new(me, generation, StreamState::Closed).render()).await {
                Ok(()) => n += 1,
                Err(e) => warn!(log, "{}[q-route] {}", LP, tr!("queue.republish_failed", key = key, err = e)),
            }
        }
        // read again, just before the write: a consumer gone or a claim made
        // meanwhile is not undone
        let key = dispatcher_key(&self.fq_name);
        let held = etcd.kv_get(key.as_bytes()).await.map(|v| String::from_utf8_lossy(&v).to_string());
        let here = !self.clients.read().await.is_empty();
        if let Some(v) = held.filter(|v| here && DispatchRecord::parse(v).map(|r| r.node) == Some(me)) {
            // [q-dead] the value it holds: to the peers only
            match broadcast_control(&etcd, &key, &v).await {
                Ok(()) => n += 1,
                Err(e) => warn!(log, "{}[q-route] {}", LP, tr!("queue.republish_failed", key = key, err = e)),
            }
        }
        n
    }

}

impl EtcdNode {
    /// Reap queues nothing is using any more.
    ///
    /// `queue-p2p-route.md` listed teardown as an open question. It became a
    /// real one when the registries landed: a leaked `Queue` used to cost this
    /// node some memory, and now also leaks `/q/{name}`, which is **replicated**
    /// - so one node's leak is every node's.
    ///
    /// Safe to call as often as an embedder likes; the work is one read lock
    /// per queue in the common case. Returns how many were reaped.
    pub async fn reap_idle_queues(&self) -> usize {
        let candidates: Vec<(String, Queue)> = {
            self.queues.read().await.iter().map(|(k, q)| (k.clone(), q.clone())).collect()
        };
        let mut reaped = Vec::new();
        for (name, q) in candidates {
            if q.is_reapable().await {
                reaped.push((name, q));
            }
        }
        if reaped.is_empty() {
            return 0;
        }
        // Two phases, and the split is not stylistic. `forget_registry` deletes
        // a replicated key, which goes through `delete_impl`, which takes
        // `queues.read()` - so awaiting it while holding `queues.write()` is a
        // deadlock. Drop the map lock first, then clear the registry.
        let mut dropped = Vec::new();
        {
            let mut map = self.queues.write().await;
            for (name, q) in reaped {
                // Re-check under the write lock: a producer or a watcher may
                // have arrived between the scan and here, and reaping then
                // would drop a queue somebody is holding.
                if !q.is_reapable().await {
                    continue;
                }
                map.remove(&name);
                dropped.push((name, q));
            }
        }
        for (name, q) in &dropped {
            q.forget_registry(&self.log).await;
            info!(self.log, "{}[q-route] {}", LP, tr!("queue.reaped", queue = name));
        }
        dropped.len()
    }

    /// [q-registry] Write this node's queue registry again: its consumers and
    /// the dispatcher records it holds for them (`Queue::republish`).
    ///
    /// The registry is ordinary replicated KV, broadcast when written - so a
    /// peer that was not receiving at that moment never learned it, and
    /// `announce_and_sync`, which copies a peer's keys, runs once at start. Run
    /// when a peer joins (`spawn_registry_republish`). Idempotent: the same
    /// values again. Returns how many keys.
    ///
    /// [q-dead] At start too: the peers `connect` admits are joins, and what
    /// `announce_and_sync` copied from them includes this node's registrations
    /// from before a restart, for consumers it no longer has. Each is withdrawn
    /// by its tombstone, and what a dispatcher holds for those streams goes
    /// back - as on each join, for one a peer still holds.
    pub(crate) async fn republish_registry(&self) -> usize {
        use crate::route::{Registration, StreamState};
        let queues: Vec<Queue> = self.queues.read().await.values().cloned().collect();
        let mut n = 0;
        // [q-dead] registrations naming this node under no queue it has
        let me = self.peers.read().await.me();
        let mut stale = Vec::new();
        for prefix in ["/q/", "/queue/"] {
            for (k, v) in self.kv_prefix(prefix.as_bytes()).await {
                let key = String::from_utf8_lossy(&k).to_string();
                if crate::route::consumer_of(&key).is_none() {
                    continue;
                }
                let Some(r) = Registration::parse(&String::from_utf8_lossy(&v)) else { continue };
                if r.node != me || r.state == StreamState::Closed {
                    continue;
                }
                let qn = QueueNameKey::new(key.clone());
                if !queues.iter().any(|q| q.fq_name == qn.name()) {
                    stale.push((key, r.generation));
                }
            }
        }
        for (key, generation) in stale {
            match withdraw_own(self, &key, me, generation).await {
                Ok(()) => n += 1,
                Err(e) => warn!(self.log, "{}[q-route] {}", LP, tr!("queue.republish_failed", key = key, err = e)),
            }
        }
        for q in queues {
            n += q.republish(&self.log).await;
        }
        if n > 0 {
            info!(self.log, "{}[q-route] {}", LP, tr!("queue.republished", count = n));
        }
        n
    }
}

/// [q-route] Write a registry key through the plain KV path.
///
/// Deliberately **not** `EtcdNode::kv_put`: that goes through `put_impl`, which
/// is the function deciding whether a key is a message - and `elect` is called
/// from inside it. Beyond the borrow-checker's objection to the cycle, a
/// registry write is by definition not queue traffic and should not be asking
/// that question at all.
async fn put_control(etcd: &EtcdNode, key: &str, value: &str) -> Result<(), Status> {
    put_control_kv(etcd, key, value).await
}

/// [q-dead] Withdraw `node`'s registration `key` of its stream `generation` on
/// its behalf - it has been down past the down timeout: a tombstone marked so
/// (`Registration::gone`), taken from any node. With an older member here, only
/// here: that member would keep it as a value.
async fn withdraw_gone(etcd: &EtcdNode, key: &str, node: crate::cluster::NodeId, generation: u64) {
    use crate::route::Registration;
    if etcd.peers.read().await.legacy_registry().await {
        let mut vault = etcd.vault.write().await;
        let ours = vault.get(key.as_bytes())
            .and_then(|kv| Registration::parse(&String::from_utf8_lossy(&kv.value)))
            .map_or(false, |s| s.removed_by(&Registration::gone(node, generation)));
        if ours {
            vault.remove(key.as_bytes());
        }
        return;
    }
    if let Err(e) = put_control(etcd, key, &Registration::gone(node, generation).render()).await {
        warn!(etcd.log, "{}[q-route] {}", LP, tr!("queue.withdraw_failed", key = key, err = e));
    }
}

/// [q-dead] Withdraw this node's registration `key`, of its stream
/// `generation`: by the stream's tombstone - or, with an older member here, by
/// a delete, as that member withdraws and reads it.
async fn withdraw_own(etcd: &EtcdNode, key: &str, me: crate::cluster::NodeId, generation: u64) -> Result<(), Status> {
    use crate::route::{Registration, StreamState};
    if etcd.peers.read().await.legacy_registry().await {
        etcd.kv_delete(key.as_bytes().to_vec()).await
    } else {
        put_control(etcd, key, &Registration::new(me, generation, StreamState::Closed).render()).await
    }
}

/// [q-dead] A registry value to the peers only, as `put_kv` would send it -
/// for one this node holds already: stored again here, a watch on it would
/// take it for news.
async fn broadcast_control(etcd: &EtcdNode, key: &str, value: &str) -> Result<(), Status> {
    let key = key.as_bytes().to_vec();
    if !etcd.policy.read().await.propagates(&key) {
        return Ok(());
    }
    let scope = if etcd.policy.read().await.is_zone_scoped(&key) {
        Some(etcd.peers.read().await.my_zone().to_string())
    } else { None };
    etcd.broadcast_scoped(BroadcastRequest::Kv(KvEvent::Put(PutRequest {
        key, value: value.as_bytes().to_vec(),
        lease: 0, prev_kv: false, ignore_value: true, ignore_lease: false,
    })), scope.as_deref()).await
}

/// Boxed, `Send` named: a tombstone written here is handled by `put_kv`, which
/// syncs the registration from a task, which writes here - a cycle the
/// compiler cannot prove `Send` through without this one named type.
fn put_control_kv<'a>(etcd: &'a EtcdNode, key: &'a str, value: &'a str)
    -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), Status>> + Send + 'a>>
{
    Box::pin(async move {
        etcd.put_kv(PutRequest {
            key: key.as_bytes().to_vec(),
            value: value.as_bytes().to_vec(),
            lease: 0, prev_kv: false, ignore_value: false, ignore_lease: false,
        }, &None).await.map(|_| ())
    })
}

impl EtcdNode {

    /// process queue message from /producer/ (no watcher notify)
    /// will create queue bucket if not exists
    /// The returned queue will call for put() local or remote
    ///
    /// if not a queue capable key, then return Err(())
    pub(crate) async fn get_or_create_queue(&self, r: &PutRequest) -> Result<(Queue, QueueNameKey), ()> {
        let key = String::from_utf8(r.key.clone()).map_err(|_|())?;

        // [q-route] A registry key is NOT a message. `/q/{name}` and
        // `/q/{name}/c/{client}` DESCRIBE the queue; enqueuing them would have
        // the registry fill the queue it exists to route - and before this test
        // existed, that is exactly what a put to either of them did. They fall
        // through to the ordinary replicated KV path, which is where every node
        // reads them from. `route::is_control` decides it structurally: a
        // delivery carries an idx and a tail, a registration carries neither.
        if crate::route::is_control(&key) {
            return Err(());
        }

        let qn = QueueNameKey::new(key);
        if qn.queue {
            // Scope the read guard: as the match scrutinee it lived for the whole
            // match, so the create arm deadlocked against its own `write()`. It
            // never showed because every queue used to be created by
            // create_watcher (which takes the write lock directly) - a queue that
            // nothing watches, like /q/ch:<table>/, is created by the first put
            // and hit it immediately, wedging the whole node.
            let existing = self.queues.read().await.get(&qn.queue_name).cloned();
            if let Some(q) = existing {
                return Ok((q, qn));
            }
            let q = Queue::new(&self, &qn).await;
            let mut queues = self.queues.write().await;
            // re-check: another put may have created it while we had no lock
            let q = queues.entry(qn.queue_name.clone()).or_insert(q).clone();
            return Ok((q, qn));
        }
        Err(())
    }

    // TODO queue: implement filters etc
    // TODO queue: get ClientID from auth token then must match a request if set
    /// 1. Create queue watcher, if it's a /q*: register queue consumer: then queue put will pick a consumer and:
    /// 2. Create regular key watcher: then regular kv put will notify
    pub(crate) async fn create_watcher(&self, r: WatchCreateRequest, cid: Uuid, sender: Sender<Result<WatchResponse, Status>>) -> ClientId {
        let qn = QueueNameKey::new(String::from_utf8_lossy(&r.key).to_string());
        let cid = qn.client_id.unwrap_or(cid);
        info!(self.log, "{}{}", LP, tr!("queue.watch_created", client = cid, watch = r.watch_id, key = &qn.input));
        // An exact consumer watch receives what the queue dispatches to it, and
        // nothing else. As an ordinary key observer too, it also received the
        // server's own registry write under that key - `/q/{name}/c/{uuid}` IS
        // the consumer registration key for the `/q/.../c/` spelling - so the
        // consumer got its host node id as a "message", and a client that
        // acknowledged it deleted its own registration.
        let key_observer = !(qn.consumer && r.range_end.is_empty());
        if key_observer {
            let mut o = self.observers.write().await;
            match o.get_mut(&r.key) {
                None => {
                    o.insert(r.key.clone(), vec![(cid, r.watch_id)]);
                }
                Some(v) => {
                    v.push((cid, r.watch_id));
                }
            }
        }
        // [range] `r.range_end` was dropped here, so a prefix watch registered
        // under its literal start key and never matched anything put under it.
        if !r.range_end.is_empty() {
            self.observer_ranges.write().await.insert(r.key.clone(), r.range_end.clone());
        }
        let mut duplicate = None;
        {
            let c = WatcherConsumer { key: r.key.clone(), client: sender.clone(), generation: 0 };
            let mut watcher = self.watchers.write().await;
            match watcher.get_mut(&cid) {
                None => {
                    let mut watcher_client_writer = EtcdClientNode {
                        client_id: cid,
                        watchers: HashMap::new(),
                    };
                    watcher_client_writer.watchers.insert(r.watch_id, c);
                    watcher.insert(cid, Arc::new(RwLock::new(watcher_client_writer)));
                }
                Some(watcher_client) => {
                    let mut watcher_client_writer = watcher_client.write().await;
                    match watcher_client_writer.watchers.get_mut(&r.watch_id) {
                        None => { let _ = watcher_client_writer.watchers.insert(r.watch_id, c); },
                        // as I understand the contract - the watcher must be uniq on the clients,
                        // therefore only one watcher ID per watching key
                        Some(w) => {
                            duplicate = Some(tr!("queue.watch_exists", watch = r.watch_id, key = String::from_utf8_lossy(&w.key)));
                        }
                    }
                }
            }
        }
        // [q-dead] said after the watchers' lock is released, and bounded: into a
        // full stream it waited, holding every watch made and every notification
        if let Some(msg) = duplicate {
            match tokio::time::timeout(crate::srv::watch::NOTIFY_TIMEOUT, sender.send(Err(Status::already_exists(msg)))).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => warn!(self.log, "{}{}", LP, tr!("queue.watch_send_failed", err = e)),
                Err(_) => warn!(self.log, "{}{}", LP, tr!("queue.watch_send_failed", err = "full")),
            }
        }

        if let Err(e) = sender.send(Ok(
            WatchResponse {
                watch_id: r.watch_id,
                created: true, .. Default::default()
            }
        )).await {
            warn!(self.log, "{}{}", LP, tr!("queue.watch_send_failed", err = e));
        }

        // [q-dead] A consumer joins its queue only after its `created` frame is
        // sent. Once it is in the queue's clients any pass of the dispatch loop
        // may hand it a waiting message, and one that came first was taken for
        // the watch's answer: an etcd client expects `created` there, and
        // embedded rppd reads the first frame as its registration and so lost
        // the message. Messages wait for a consumer more often now that a gone
        // consumer's go back to the line.
        if qn.consumer {
            let mut queue_map = self.queues.write().await;
            match queue_map.get_mut(&qn.queue_name) {
                None => { // no queue exists
                    let mut queue = Queue::new(&self, &qn).await;
                    let queue_name = qn.queue_name.clone();
                    let queue_consumer_key = queue.make_consumer(qn, cid, r.watch_id, &sender).await;
                    queue_map.insert(queue_name, queue);
                    queue_consumer_key
                }
                Some(queue) => queue.make_consumer(qn, cid, r.watch_id, &sender).await
            }
        }
        cid
    }

    /// [q-dead] Withdraw what `peer` places on this node and this node does not
    /// hold: its consumer registrations naming this node for a stream that is
    /// neither receiving nor draining here. For a peer connected late - after a
    /// restart that could not reach it, it still holds this node's old streams,
    /// and its dispatcher what it handed them. Returns how many.
    pub(crate) async fn withdraw_stale_at(&self, peer: crate::cluster::EtcdPeerNodeType) -> usize {
        use crate::route::{Registration, StreamState};
        use crate::etcdpb::etcdserverpb::RangeRequest;
        let me = self.peers.read().await.me();
        let mut kv = {
            let node = peer.lock().await;
            let c = node.kv_client.lock().await.clone();
            c
        };
        let mut found = Vec::new();
        for (from, to) in [(&b"/q/"[..], &b"/q0"[..]), (&b"/queue/"[..], &b"/queue0"[..])] {
            let req = RangeRequest { key: from.to_vec(), range_end: to.to_vec(), ..Default::default() };
            let Ok(Ok(resp)) = tokio::time::timeout(crate::peer::PEER_RPC_TIMEOUT, kv.range(req)).await
                else { continue };
            for kv in resp.into_inner().kvs {
                let key = String::from_utf8_lossy(&kv.key).to_string();
                let Some(cid) = crate::route::consumer_of(&key) else { continue };
                let Some(r) = Registration::parse(&String::from_utf8_lossy(&kv.value)) else { continue };
                if r.node == me && r.state != StreamState::Closed && r.generation > 0 {
                    found.push((key, cid, r.generation));
                }
            }
        }
        let mut n = 0;
        for (key, cid, generation) in found {
            let qn = QueueNameKey::new(key.clone());
            let q = self.queues.read().await.get(&qn.queue_name).cloned().filter(|q| q.fq_name == qn.name());
            let held = match &q {
                Some(q) => q.stream_here(&cid, generation).await,
                None => false,
            };
            if held {
                continue;
            }
            if let Some(q) = &q {
                q.closed_now(&cid, generation).await;
            }
            match withdraw_own(self, &key, me, generation).await {
                Ok(()) => n += 1,
                Err(e) => warn!(self.log, "{}[q-route] {}", LP, tr!("queue.republish_failed", key = key, err = e)),
            }
        }
        n
    }

    /// [q-dead] Take a watch off this node: the client's watcher, and the key
    /// observer and range it was. What a cancel does - and a consumer dropped as
    /// slow (`Queue::cancel_stalled`), so a watch made again under its id is
    /// one watch, not two. Returns it, as it was.
    ///
    /// [q-dead] With `on`, only while the watch is still on that stream.
    pub(crate) async fn forget_watch(&self, cid: &ClientId, watch_id: WatcherId,
        on: Option<&Sender<Result<WatchResponse, Status>>>) -> Option<WatcherConsumer>
    {
        // [q-dead] Taken out of the map, and every guard released, before any
        // queue work: the queue writes a draining consumer's registration, and
        // a put waits on the `watch_notify` task - which takes
        // `watchers.read()`. Held across it, a cancel stopped every put on the
        // node.
        let c = self.watchers.read().await.get(cid).cloned()?;
        trace!(self.log, "{}{}", LP, tr!("queue.cancel_request", client = cid, count = c.read().await.watchers.len()));
        let w = {
            let mut c = c.write().await;
            if let (Some(ch), Some(w)) = (on, c.watchers.get(&watch_id)) {
                if !w.client.same_channel(ch) {
                    return None;
                }
            }
            c.watchers.remove(&watch_id)?
        };
        let empty = {
            let mut obs = self.observers.write().await;
            if let Some(o) = obs.get_mut(&w.key) {
                o.retain(|(o_cid, o_wid)| !(o_cid == cid && o_wid == &watch_id));
                o.is_empty()
            } else { false }
        };
        // [range] ...and drop the range with the last watcher that used
        // it, or every cancelled prefix watch leaves a scan entry behind.
        if empty {
            self.observers.write().await.remove(&w.key);
            self.observer_ranges.write().await.remove(&w.key);
        }
        Some(w)
    }

    /// [q-dead] A key watcher that did not take a notification within
    /// `NOTIFY_TIMEOUT`: its watch is taken off as a cancel takes it - and the
    /// queue consumer it may also be dropped as slow - and it is told, with
    /// the reason, once its stream has room. It can watch again.
    pub(crate) async fn cancel_slow_watch(&self, cid: ClientId, wid: WatcherId, ch: Sender<Result<WatchResponse, Status>>) {
        let Some(w) = self.forget_watch(&cid, wid, Some(&ch)).await else { return };
        warn!(self.log, "{}{}", LP, tr!("srv.watch_slow_log", client = cid, watch = wid));
        let qn = QueueNameKey::new(String::from_utf8_lossy(&w.key).to_string());
        if qn.consumer {
            let q = self.queues.read().await.get(&qn.queue_name).cloned();
            if let Some(q) = q {
                q.drop_watcher(&cid, wid, false, Some(&ch)).await;
            }
        }
        let reason = tr!("srv.watch_slow", timeout = format!("{:?}", crate::srv::watch::NOTIFY_TIMEOUT));
        let _ = ch.send(Ok(WatchResponse { watch_id: wid, canceled: true, cancel_reason: reason, ..Default::default() })).await;
    }

    pub(crate) async fn remove_watcher(&self, r: WatchCancelRequest, cid: ClientId) {
        if let Some(w) = self.forget_watch(&cid, r.watch_id, None).await {
            debug!(self.log, "{}{}", LP, tr!("queue.cancelled", client = cid, watch = r.watch_id));

            // A cancelled consumer watch is no longer a consumer. Its stream
            // stays open, so without this the dispatcher kept handing it
            // messages - marked delivered, so no other consumer got them.
            let qn = QueueNameKey::new(String::from_utf8_lossy(&w.key).to_string());
            if qn.consumer {
                let q = self.queues.read().await.get(&qn.queue_name).cloned();
                if let Some(q) = q {
                    // [q-dead] its stream is open: it keeps what it has
                    q.drop_watcher(&cid, r.watch_id, false, None).await;
                }
            }

            if let Err(e) = w.client.send(Ok(
                WatchResponse { watch_id: r.watch_id, canceled: true, .. Default::default() }
            )).await {
                warn!(self.log, "{}{}", LP, tr!("queue.cancel_send_failed", err = e));
            }
        }
    }

}

fn now_micros() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64).unwrap_or(0)
}

/// [q-dead] When this process started, on the generations' clock: no stream
/// generation it gives is below it (`next_gen`), so a peer that sees it in a
/// registration knows any older stream of this node is dead
/// (`Registration::outlives`).
pub(crate) fn incarnation() -> u64 {
    static START: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *START.get_or_init(now_micros)
}

/// [q-dead] A new stream generation: larger than any this process gave, and
/// than the clock in microseconds - so a client's newer stream, on this node
/// or another, has the larger one, a restart included, as far as the nodes'
/// clocks agree.
fn next_gen() -> u64 {
    static LAST: AtomicU64 = AtomicU64::new(0);
    let start = incarnation();
    let mut last = LAST.load(Ordering::Relaxed);
    loop {
        let next = (last + 1).max(now_micros()).max(start);
        match LAST.compare_exchange(last, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return next,
            Err(seen) => last = seen,
        }
    }
}

#[cfg(test)]
pub mod test {
    use super::*;

    thread_local! {
        /// [q-idx] `first_idx` in the unit tests: 1, so their keys read
        /// `/q/name/1/k`; a test of the start sets it.
        pub(crate) static FIRST_IDX: std::cell::Cell<u64> = const { std::cell::Cell::new(1) };
    }
    #[test]
    pub fn test() {
        assert_eq!(QueueNameKey::new("/q/name/p/key".into()).queue_name, "name".to_string());
        assert!(QueueNameKey::new("/q/name/p/key".into()).queue);
        /*
        assert_eq!(Queue::get_producer_key(&("/q/name/i/key".split("/").collect())).unwrap(), "key".to_string());
        assert_eq!(Queue::get_producer_key(&("/queue/name/p/key".split("/").collect())).unwrap(), "key".to_string());
        assert_eq!(Queue::get_producer_key(&("/queue/name/i/key".split("/").collect())).unwrap(), "key".to_string());
        assert_eq!(Queue::get_producer_key(&("/queue/name/producer/key".split("/").collect())).unwrap(), "key".to_string());
        assert_eq!(Queue::get_producer_key(&("/q/name/producer/key".split("/").collect())).unwrap(), "key".to_string());
        assert_eq!(Queue::queue_name(&("/q/name/p/key".split("/").collect())).unwrap(), ("q".to_string(), "name".to_string()));
        assert_eq!(Queue::queue_name(&("/q/name/p/key".split("/").collect())).unwrap(), ("q".to_string(), "name".to_string()));
        assert!(Queue::is_consumer(&("/q/name/consumer/client".split("/").collect())));
        assert!(Queue::is_consumer(&("/q/name/c/client".split("/").collect())));

         */
    }

    fn msg(idx: u64) -> QueueMsg {
        QueueMsg { idx, key: format!("/q/name/{}/k", idx), value: vec![],
                   created: Instant::now(), handled_by: None, stream: None }
    }

    /// Regression: a second producer put used to stall the queue for good.
    /// `put` pushes to the front and notifies with the NEW idx, while dispatch
    /// reads the OLDEST from the back - so `if x.idx > 0 && r != x.idx` lined up
    /// only while exactly one message was outstanding.
    #[test]
    pub fn dispatch_walks_the_backlog_not_just_the_head() {
        let mut q: VecDeque<QueueMsg> = VecDeque::new();
        assert_eq!(next_undelivered(&q), None);

        q.push_front(msg(1));
        assert_eq!(next_undelivered(&q), Some(1));

        // message 1 is still queued (not yet acknowledged) when 2 arrives
        q.push_front(msg(2));
        assert_eq!(next_undelivered(&q), Some(1), "FIFO: the older message first");

        // hand 1 over, and the next wake must move on to 2 rather than re-send 1
        q.iter_mut().find(|m| m.idx == 1).unwrap().handled_by = Some(Uuid::nil());
        assert_eq!(next_undelivered(&q), Some(2));

        q.iter_mut().find(|m| m.idx == 2).unwrap().handled_by = Some(Uuid::nil());
        assert_eq!(next_undelivered(&q), None, "nothing left to hand over");
    }

    /// [q-idx] A late acknowledge from before a dispatcher restart names an
    /// index the new dispatcher may have given out again: it must not remove a
    /// message with another key.
    #[test]
    pub fn acknowledge_needs_index_and_key() {
        let m = QueueMsg { idx: 3, key: "/q/name/3/new".into(), value: vec![],
                           created: Instant::now(), handled_by: None, stream: None };
        assert!(acknowledges(&m, 3, "new"));
        assert!(!acknowledges(&m, 3, "old"), "same index, another message");
        assert!(!acknowledges(&m, 4, "new"));
        // a forwarded message keeps its producer key, the same last segment
        let p = QueueMsg { key: "/q/name/p/new".into(), ..m };
        assert!(acknowledges(&p, 3, "new"));

        let a = clock_idx();
        std::thread::sleep(std::time::Duration::from_millis(2));
        assert!(clock_idx() > a && a > 1_000_000_000_000_000, "a later start, a higher first index");
    }

    /// Regression: acknowledges come back out of order, so an ack must remove
    /// only its own message. Removing everything below it discarded messages
    /// that had not been dispatched yet.
    #[test]
    pub fn acknowledge_removes_only_its_own_message() {
        let mut q: VecDeque<QueueMsg> = VecDeque::new();
        for i in 1..=5 { q.push_front(msg(i)); }
        // consumer finishes #4 first and acknowledges it
        let idx = 4;
        q.retain(|v| !acknowledges(v, idx, "k"));
        assert_eq!(q.len(), 4);
        let left: Vec<u64> = q.iter().rev().map(|m| m.idx).collect();
        assert_eq!(left, vec![1, 2, 3, 5], "only #4 goes, the rest still owe delivery");
        // and they are all still deliverable
        assert_eq!(next_undelivered(&q), Some(1));
    }

    /// An acknowledge removes everything up to that idx and wakes the dispatcher
    /// with 0; that wake must still deliver, which the old guard refused since 0
    /// never equals a real idx.
    #[test]
    pub fn acknowledge_frees_the_next_message() {
        let mut q: VecDeque<QueueMsg> = VecDeque::new();
        q.push_front(msg(1));
        q.push_front(msg(2));
        q.iter_mut().for_each(|m| m.handled_by = Some(Uuid::nil()));
        assert_eq!(next_undelivered(&q), None);

        q.push_front(msg(3));
        // Queue::delete's retain on the acknowledge of idx 1, key `k`
        q.retain(|v| !acknowledges(v, 1, "k"));
        assert_eq!(q.len(), 2);
        assert_eq!(next_undelivered(&q), Some(3));
    }
}
#[cfg(test)]
pub(crate) mod node_tests {
    //! One node, a queue used the way queue.md describes it: watch a consumer
    //! key, put on the producer key, receive a put event, delete to acknowledge.
    //! In-process, through `new_watcher` - the same protocol as the gRPC Watch.
    use super::*;
    use crate::etcdpb::etcdserverpb::{watch_request, DeleteRangeRequest, RangeRequest, WatchRequest};
    use crate::etcdpb::etcdserverpb::kv_server::Kv as _;
    use std::time::Duration;

    pub(crate) async fn node(node_uuid: &str) -> EtcdNode {
        let mut cfg = crate::cli::EtcdConfig::with_defaults();
        cfg.cluster = "11111111-1111-5111-8111-111111111111".to_string();
        cfg.node = node_uuid.to_string();
        cfg.listen_client_urls = "127.0.0.1:0".to_string();
        EtcdNode::init(cfg, Logger::root(slog::Discard, slog::o!()),
            #[cfg(feature = "tracer")] None).await.unwrap()
    }

    /// An in-process watch stream: the request side, and the responses.
    pub(crate) struct Consumer {
        pub(crate) req: Sender<WatchRequest>,
        pub(crate) rx: Receiver<Result<WatchResponse, Status>>,
    }

    impl Consumer {
        /// Watch `key` as client `cid`, and read past the `created` frame.
        pub(crate) async fn watch(n: &EtcdNode, cid: &Uuid, key: &str, watch_id: i64) -> Consumer {
            let (req, req_rx) = mpsc::channel(10);
            let (tx, rx) = mpsc::channel(10);
            n.new_watcher(req_rx, tx, cid).await;
            let mut c = Consumer { req, rx };
            c.create(key, watch_id).await;
            c
        }

        pub(crate) async fn create(&mut self, key: &str, watch_id: i64) {
            self.req.send(WatchRequest { request_union: Some(watch_request::RequestUnion::CreateRequest(
                WatchCreateRequest { key: key.as_bytes().to_vec(), watch_id, ..Default::default() })) })
                .await.unwrap();
            let r = self.frame().await.expect("a created frame").unwrap();
            assert!(r.created, "{:?}", r);
        }

        pub(crate) async fn frame(&mut self) -> Option<Result<WatchResponse, Status>> {
            tokio::time::timeout(Duration::from_secs(5), self.rx.recv()).await.ok().flatten()
        }

        /// The next delivery: (key, value).
        pub(crate) async fn next(&mut self) -> (String, String) {
            let r = self.frame().await.expect("a delivery within 5 s").expect("not an error");
            let kv = r.events[0].kv.clone().unwrap();
            (String::from_utf8_lossy(&kv.key).to_string(), String::from_utf8_lossy(&kv.value).to_string())
        }

        /// The next delivery, waited for longer: one handed out again once a
        /// draining consumer's stream is seen closed (`Queue::drain`).
        pub(crate) async fn next_after_drain(&mut self) -> (String, String) {
            let r = tokio::time::timeout(Duration::from_secs(20), self.rx.recv()).await
                .expect("a delivery within 20 s").expect("open").expect("not an error");
            let kv = r.events[0].kv.clone().unwrap();
            (String::from_utf8_lossy(&kv.key).to_string(), String::from_utf8_lossy(&kv.value).to_string())
        }

        /// Nothing more arrives for a while.
        pub(crate) async fn quiet(&mut self) -> bool {
            tokio::time::timeout(Duration::from_millis(300), self.rx.recv()).await.is_err()
        }
    }

    pub(crate) async fn produce(n: &EtcdNode, key: &str, value: &str) {
        n.kv_put(key.as_bytes().to_vec(), value.as_bytes().to_vec()).await.unwrap();
    }

    /// Acknowledge, as a client does: delete the key the event carried.
    pub(crate) async fn ack(n: &EtcdNode, key: &str) {
        n.delete_range(tonic::Request::new(DeleteRangeRequest { key: key.as_bytes().to_vec(), ..Default::default() }))
            .await.unwrap();
    }

    /// [q-dead] A peer that is not there - a channel to a closed port - held
    /// as admitted, answering `status` with `version` when it did.
    pub(crate) async fn fake_peer(n: &EtcdNode, id: crate::cluster::NodeId, version: &str) {
        let ch = tonic::transport::Endpoint::from_static("http://127.0.0.1:9").connect_lazy();
        let mt = crate::etcdpb::etcdserverpb::maintenance_client::MaintenanceClient::new(ch.clone());
        n.peers.write().await.admit_fresh(vec![(id, "http://127.0.0.1:9".to_string(), ch, mt, version.to_string())]).await;
    }

    /// [q-dead] Set when that peer stopped answering.
    pub(crate) async fn down_since(n: &EtcdNode, id: crate::cluster::NodeId, since: Option<Instant>) {
        let p = n.peers.read().await.handle(id).await.unwrap();
        p.lock().await.down_since = since;
    }

    /// [q-dead] Set the registry format that peer reads.
    pub(crate) async fn reads_registry(n: &EtcdNode, id: crate::cluster::NodeId, registry: u32) {
        let p = n.peers.read().await.handle(id).await.unwrap();
        p.lock().await.registry = registry;
    }

    /// [q-dead] The registration stored under `key`.
    pub(crate) async fn registration(n: &EtcdNode, key: &str) -> Option<crate::route::Registration> {
        n.kv_get(key.as_bytes()).await.and_then(|v| crate::route::Registration::parse(&String::from_utf8_lossy(&v)))
    }

    pub(crate) async fn queue(n: &EtcdNode, name: &str) -> Queue {
        n.queues.read().await.get(name).cloned().expect("the queue exists")
    }

    /// [q-idx] A node becoming the dispatcher gives out indexes from that
    /// moment, though its queue was made long before - as a producer's or a
    /// consumer's node - with a counter below what the last dispatcher gave.
    #[tokio::test]
    async fn a_new_dispatcher_starts_its_indexes_from_now() {
        let n = node(A).await;
        produce(&n, "/q/moved/p/m", "1").await; // made with the counter at 1
        let q = queue(&n, "moved").await;
        *q.dispatch.write().await = crate::route::Dispatch::Unknown;
        test::FIRST_IDX.with(|f| f.set(5_000));
        q.set_dispatch(crate::route::Dispatch::Local).await;
        let after = q.idx.load(Ordering::Relaxed);
        assert!(after >= 5_000, "from the new start: {}", after);
        // staying the dispatcher moves nothing, and never down
        test::FIRST_IDX.with(|f| f.set(10));
        q.set_dispatch(crate::route::Dispatch::Local).await;
        q.set_dispatch(crate::route::Dispatch::Unknown).await;
        q.set_dispatch(crate::route::Dispatch::Local).await;
        assert_eq!(q.idx.load(Ordering::Relaxed), after);
        test::FIRST_IDX.with(|f| f.set(1));
    }

    /// The indexes the dispatcher still holds, oldest first.
    pub(crate) async fn held(q: &Queue) -> Vec<u64> {
        q.queue.read().await.iter().rev().map(|m| m.idx).collect()
    }

    pub(crate) async fn until<F, Fut>(f: F) -> bool where F: Fn() -> Fut, Fut: std::future::Future<Output = bool> {
        for _ in 0..150 {
            if f().await { return true; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    const A: &str = "22222222-2222-5222-8222-222222222222";

    /// A message put before any consumer exists waits, and goes to the first
    /// one that registers; the rest follow oldest first.
    #[tokio::test]
    async fn messages_wait_for_a_consumer_and_arrive_oldest_first() {
        let n = node(A).await;
        produce(&n, "/q/jobs/p/one", "1").await;
        produce(&n, "/q/jobs/p/two", "2").await;
        let cid = Uuid::new_v4();
        let mut c = Consumer::watch(&n, &cid, &format!("/q/jobs/c/{}", cid), 1).await;
        assert_eq!(c.next().await, ("/q/jobs/1/one".to_string(), "1".to_string()));
        assert_eq!(c.next().await, ("/q/jobs/2/two".to_string(), "2".to_string()));
        assert!(c.quiet().await, "each message once");

        // nothing is stored as a key: the producer key is queue traffic
        assert!(n.kv_get(b"/q/jobs/p/one").await.is_none());
        // the delivery key reads back the held message until it is acknowledged
        let got = n.range(tonic::Request::new(RangeRequest { key: b"/q/jobs/2/two".to_vec(), ..Default::default() }))
            .await.unwrap().into_inner();
        assert_eq!(got.kvs[0].value, b"2");
    }

    /// An acknowledge removes the one message whose index it carries - in any
    /// order - and a delivered, unacknowledged message is not handed out again.
    #[tokio::test]
    async fn an_ack_removes_exactly_its_message() {
        let n = node(A).await;
        let cid = Uuid::new_v4();
        let mut c = Consumer::watch(&n, &cid, &format!("/queue/work/consumer/{}", cid), 3).await;
        for i in 1..=3 {
            produce(&n, &format!("/queue/work/producer/m{}", i), &i.to_string()).await;
        }
        let keys: Vec<String> = vec![c.next().await.0, c.next().await.0, c.next().await.0];
        assert_eq!(keys, vec!["/queue/work/1/m1", "/queue/work/2/m2", "/queue/work/3/m3"]);
        let q = queue(&n, "work").await;
        assert_eq!(held(&q).await, vec![1, 2, 3], "delivered, held until acknowledged");

        ack(&n, &keys[1]).await;
        assert_eq!(held(&q).await, vec![1, 3], "only #2 goes");
        ack(&n, "/queue/work/nothing-here").await; // no index: removes nothing
        assert_eq!(held(&q).await, vec![1, 3]);
        assert!(c.quiet().await, "the ack's wake-up re-sends nothing");
        ack(&n, &keys[0]).await;
        ack(&n, &keys[2]).await;
        assert!(held(&q).await.is_empty());
    }

    /// The node hosting a consumer claims the queue: the dispatcher record and
    /// the consumer's registration both name it (route.rs).
    #[tokio::test]
    async fn a_consumer_registers_itself_and_claims_the_dispatcher() {
        let n = node(A).await;
        let cid = Uuid::new_v4();
        let _c = Consumer::watch(&n, &cid, &format!("/q/reg/c/{}", cid), 1).await;
        assert_eq!(n.queue_dispatcher("/q/reg").await, Some(n.node_id));
        assert_eq!(n.queue_dispatcher("/q/reg/p/anything").await, Some(n.node_id), "any key of the queue");
        let r = registration(&n, &format!("/q/reg/c/{}", cid)).await.unwrap();
        assert_eq!((r.node, r.state), (n.node_id, crate::route::StreamState::Receiving), "the consumer's host");
        assert!(queue(&n, "reg").await.dispatch().await.is_local());
    }

    /// A watch key without a valid UUID takes the connection's id instead.
    #[tokio::test]
    async fn a_consumer_key_without_a_uuid_uses_the_connection_id() {
        let n = node(A).await;
        let conn = Uuid::new_v4();
        let mut c = Consumer::watch(&n, &conn, "/q/anon/c/not-a-uuid", 1).await;
        let q = queue(&n, "anon").await;
        assert!(q.clients.read().await.contains_key(&conn));
        produce(&n, "/q/anon/p/k", "v").await;
        assert_eq!(c.next().await.1, "v");
    }

    /// A consumer whose stream has closed is dropped, its registration is
    /// withdrawn, and the message waits for the next consumer.
    #[tokio::test]
    async fn a_consumer_that_is_gone_is_dropped_and_the_message_waits() {
        let n = node(A).await;
        let dead = Uuid::new_v4();
        let gone = Consumer::watch(&n, &dead, &format!("/q/two/c/{}", dead), 1).await;
        drop(gone); // its receiving end closes

        produce(&n, "/q/two/p/m0", "v").await;
        let q = queue(&n, "two").await;
        assert!(until(|| async { !q.clients.read().await.contains_key(&dead) }).await);
        assert!(until(|| async { n.kv_get(format!("/q/two/c/{}", dead).as_bytes()).await.is_none() }).await,
            "the dead consumer's registration is withdrawn");
        assert!(q.queue.read().await[0].handled_by.is_none(), "not counted delivered");

        let live = Uuid::new_v4();
        let mut c = Consumer::watch(&n, &live, &format!("/q/two/c/{}", live), 1).await;
        assert_eq!(c.next().await.0, "/q/two/1/m0", "the next consumer gets it");
        assert!(n.kv_get(format!("/q/two/c/{}", live).as_bytes()).await.is_some());
    }

    /// A consumer that does not take a message within DISPATCH_TIMEOUT is
    /// dropped too: a stalled watcher must not wedge the queue.
    #[tokio::test]
    async fn a_stalled_consumer_is_dropped_after_the_dispatch_timeout() {
        let n = node(A).await;
        let cid = Uuid::new_v4();
        let (req, req_rx) = mpsc::channel(10);
        let (tx, rx) = mpsc::channel(1); // room for the created frame only
        n.new_watcher(req_rx, tx, &cid).await;
        req.send(WatchRequest { request_union: Some(watch_request::RequestUnion::CreateRequest(
            WatchCreateRequest { key: format!("/q/slow/c/{}", cid).into_bytes(), watch_id: 1, ..Default::default() })) })
            .await.unwrap();
        produce(&n, "/q/slow/p/k", "v").await;
        let q = queue(&n, "slow").await;
        tokio::time::sleep(DISPATCH_TIMEOUT + Duration::from_millis(500)).await;
        assert!(q.clients.read().await.is_empty(), "the stalled consumer was dropped");
        assert_eq!(held(&q).await, vec![1], "and the message waits for the next one");
        assert!(q.queue.read().await[0].handled_by.is_none(), "undelivered");
        drop(rx);
    }

    /// A second create on the same watch id of one client is refused.
    #[tokio::test]
    async fn the_same_watch_id_twice_is_refused() {
        let n = node(A).await;
        let cid = Uuid::new_v4();
        let mut c = Consumer::watch(&n, &cid, "/plain/key", 7).await;
        c.req.send(WatchRequest { request_union: Some(watch_request::RequestUnion::CreateRequest(
            WatchCreateRequest { key: b"/plain/other".to_vec(), watch_id: 7, ..Default::default() })) })
            .await.unwrap();
        let e = c.frame().await.unwrap().unwrap_err();
        assert_eq!(e.code(), tonic::Code::AlreadyExists);
        assert!(e.message().contains("/plain/key"), "names the key the id is on: {}", e.message());
    }

    /// Registry keys are not messages: a put to `/q/{name}` or
    /// `/q/{name}/c/{uuid}` is an ordinary stored key.
    #[tokio::test]
    async fn a_registry_key_is_stored_not_enqueued() {
        let n = node(A).await;
        produce(&n, "/q/reg2", "12345").await;
        produce(&n, &format!("/q/reg2/c/{}", Uuid::new_v4()), "12345").await;
        assert_eq!(n.kv_get(b"/q/reg2").await.as_deref(), Some(&b"12345"[..]));
        assert!(n.queues.read().await.get("reg2").is_none(), "no queue was created for them");
    }

    /// A record naming a node that is not an Online peer is replaced, not kept.
    #[tokio::test]
    async fn a_dispatcher_that_is_not_online_is_re_elected() {
        let n = node(A).await;
        produce(&n, "/q/stale", "987654321;reply_to=/q/stale-reply").await;
        produce(&n, "/q/stale/p/k", "v").await;
        assert_eq!(n.queue_dispatcher("/q/stale").await, Some(n.node_id));
        let rec = crate::route::DispatchRecord::parse(
            &String::from_utf8_lossy(&n.kv_get(b"/q/stale").await.unwrap())).unwrap();
        assert_eq!(rec.reply_to.as_deref(), Some("/q/stale-reply"), "the link survives a re-election");
        assert!(queue(&n, "stale").await.dispatch().await.is_local());
    }

    /// `queue_link` records `reply_to` on the request queue's record, once.
    #[tokio::test]
    async fn queue_link_records_the_reply_queue() {
        let n = node(A).await;
        n.queue_link("/q/rpc", "/q/rpc-reply").await; // no queue yet: nothing to link
        assert!(n.kv_get(b"/q/rpc").await.is_none());

        produce(&n, "/q/rpc/p/req", "v").await;
        n.queue_link("/q/rpc/p/req", "/q/rpc-reply").await;
        let rec = || async {
            crate::route::DispatchRecord::parse(&String::from_utf8_lossy(&n.kv_get(b"/q/rpc").await.unwrap())).unwrap()
        };
        assert_eq!(rec().await.reply_to.as_deref(), Some("/q/rpc-reply"));
        assert_eq!(rec().await.node, n.node_id, "the dispatcher stays where it is");
        n.queue_link("/q/rpc", "/q/rpc-reply").await; // idempotent
        assert_eq!(rec().await.reply_to.as_deref(), Some("/q/rpc-reply"));
    }

    /// The reaper: only a queue that is empty, unwatched, consumerless anywhere
    /// and idle for QUEUE_IDLE goes - and its dispatcher record with it.
    #[tokio::test]
    async fn only_an_idle_empty_unwatched_queue_is_reaped() {
        let n = node(A).await;
        produce(&n, "/q/idle/p/k", "v").await;
        let q = queue(&n, "idle").await;
        let long_ago = || Some(Instant::now().checked_sub(QUEUE_IDLE + Duration::from_secs(1)).unwrap());

        *q.idle_since.write().await = long_ago();
        assert_eq!(n.reap_idle_queues().await, 0, "it still holds a message");
        q.queue.write().await.clear();
        *q.idle_since.write().await = Some(Instant::now());
        assert_eq!(n.reap_idle_queues().await, 0, "empty, but used just now");

        // a consumer on another node keeps it, though nothing is attached here
        let elsewhere = format!("/q/idle/c/{}", Uuid::new_v4());
        produce(&n, &elsewhere, "987654321").await;
        *q.idle_since.write().await = long_ago();
        assert_eq!(n.reap_idle_queues().await, 0, "a remote consumer is a user");
        n.kv_delete(elsewhere.into_bytes()).await.unwrap();

        // a local consumer keeps it
        let cid = Uuid::new_v4();
        let c = Consumer::watch(&n, &cid, &format!("/q/idle/c/{}", cid), 1).await;
        *q.idle_since.write().await = long_ago();
        assert_eq!(n.reap_idle_queues().await, 0, "a local consumer is a user");
        q.drop_watcher(&cid, 1, true, None).await;
        drop(c);

        *q.idle_since.write().await = long_ago();
        assert!(n.kv_get(b"/q/idle").await.is_some());
        assert_eq!(n.reap_idle_queues().await, 1);
        assert!(n.queues.read().await.get("idle").is_none(), "the queue is gone");
        assert!(n.kv_get(b"/q/idle").await.is_none(), "and its dispatcher record");
    }

    /// A delivery from a peer for a consumer this node does not host is refused,
    /// so the dispatcher can try another rather than count it delivered.
    #[tokio::test]
    async fn a_delivery_for_a_consumer_not_hosted_here_is_refused() {
        let n = node(A).await;
        let mut r = tonic::Request::new(PutRequest {
            key: format!("/q/del/c/{}/5/k", Uuid::new_v4()).into_bytes(), value: b"v".to_vec(), ..Default::default() });
        r.metadata_mut().insert(crate::etcdpb::XPEER, "987654321".parse().unwrap());
        let e = n.put(r).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::NotFound);
        assert!(held(&queue(&n, "del").await).await.is_empty(), "and nothing was enqueued");
    }

    /// ...and one for a consumer it does host goes straight to its watcher,
    /// under the delivery key, without a second copy in a queue here.
    #[tokio::test]
    async fn a_delivery_from_a_peer_reaches_the_local_consumer() {
        let n = node(A).await;
        let cid = Uuid::new_v4();
        let mut c = Consumer::watch(&n, &cid, &format!("/q/del2/c/{}", cid), 1).await;
        let key = format!("/q/del2/c/{}/9/k", cid);
        let mut r = tonic::Request::new(PutRequest { key: key.clone().into_bytes(), value: b"v".to_vec(), ..Default::default() });
        r.metadata_mut().insert(crate::etcdpb::XPEER, "987654321".parse().unwrap());
        n.put(r).await.unwrap();
        assert_eq!(c.next().await, (key, "v".to_string()));
        assert!(held(&queue(&n, "del2").await).await.is_empty());

        // a watcher whose stream closed is dropped, and the delivery refused
        drop(c);
        let mut r = tonic::Request::new(PutRequest {
            key: format!("/q/del2/c/{}/10/k", cid).into_bytes(), value: b"v".to_vec(), ..Default::default() });
        r.metadata_mut().insert(crate::etcdpb::XPEER, "987654321".parse().unwrap());
        assert_eq!(n.put(r).await.unwrap_err().code(), tonic::Code::NotFound);
        assert!(queue(&n, "del2").await.clients.read().await.is_empty());
    }

    /// Cancelling a watch answers `canceled` and stops its events.
    #[tokio::test]
    async fn a_cancelled_watch_gets_no_more_events() {
        let n = node(A).await;
        let cid = Uuid::new_v4();
        let mut c = Consumer::watch(&n, &cid, "/w/prefix/", 4).await;
        // a second, ranged watch on the same client
        c.req.send(WatchRequest { request_union: Some(watch_request::RequestUnion::CreateRequest(
            WatchCreateRequest { key: b"/w/r/".to_vec(), range_end: b"/w/r0".to_vec(), watch_id: 5, ..Default::default() })) })
            .await.unwrap();
        assert!(c.frame().await.unwrap().unwrap().created);
        n.kv_put(b"/w/r/x".to_vec(), b"1".to_vec()).await.unwrap();
        assert_eq!(c.next().await.0, "/w/r/x");

        c.req.send(WatchRequest { request_union: Some(watch_request::RequestUnion::CancelRequest(
            WatchCancelRequest { watch_id: 5 })) }).await.unwrap();
        let r = c.frame().await.unwrap().unwrap();
        assert!(r.canceled && r.watch_id == 5);
        assert!(n.observer_ranges.read().await.is_empty(), "the range goes with its last watcher");
        n.kv_put(b"/w/r/y".to_vec(), b"2".to_vec()).await.unwrap();
        assert!(c.quiet().await);
        // cancelling an id that is not there is ignored
        c.req.send(WatchRequest { request_union: Some(watch_request::RequestUnion::CancelRequest(
            WatchCancelRequest { watch_id: 99 })) }).await.unwrap();
        assert!(c.quiet().await);
    }

    /// A progress request answers with the client's watch id.
    #[tokio::test]
    async fn a_progress_request_names_the_watch() {
        use crate::etcdpb::etcdserverpb::WatchProgressRequest;
        let n = node(A).await;
        let cid = Uuid::new_v4();
        let mut c = Consumer::watch(&n, &cid, "/p/k", 11).await;
        c.req.send(WatchRequest { request_union: Some(watch_request::RequestUnion::ProgressRequest(
            WatchProgressRequest {})) }).await.unwrap();
        let r = c.frame().await.unwrap().unwrap();
        assert_eq!(r.watch_id, 11);
        assert!(!r.fragment, "one watch, no fragment");

        // a client with no watch at all is answered with id 0
        let other = Uuid::new_v4();
        let (req, req_rx) = mpsc::channel(10);
        let (tx, mut rx) = mpsc::channel(10);
        n.new_watcher(req_rx, tx, &other).await;
        req.send(WatchRequest { request_union: Some(watch_request::RequestUnion::ProgressRequest(
            WatchProgressRequest {})) }).await.unwrap();
        assert_eq!(rx.recv().await.unwrap().unwrap().watch_id, 0);
        // an empty request is ignored
        req.send(WatchRequest { request_union: None }).await.unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(200), rx.recv()).await.is_err());
    }

    /// [q-dead] A consumer that goes away holding a message it never
    /// acknowledged: the message goes to the next consumer. Nothing else is sent
    /// to the gone one, so its closed stream is seen without a send into it.
    #[tokio::test]
    async fn a_message_handed_to_a_consumer_that_goes_is_handed_to_the_next() {
        let n = node(A).await;
        let first = Uuid::new_v4();
        let mut c1 = Consumer::watch(&n, &first, &format!("/q/redo/c/{}", first), 1).await;
        produce(&n, "/q/redo/p/m1", "1").await;
        assert_eq!(c1.next().await.0, "/q/redo/1/m1");
        let q = queue(&n, "redo").await;
        assert_eq!(q.queue.read().await[0].handled_by, Some(first));

        let second = Uuid::new_v4();
        let mut c2 = Consumer::watch(&n, &second, &format!("/q/redo/c/{}", second), 1).await;
        assert!(c2.quiet().await, "handed to a live consumer: not handed twice");
        drop(c1); // gone, unacknowledged
        assert_eq!(c2.next().await, ("/q/redo/1/m1".to_string(), "1".to_string()), "handed again");
        assert!(until(|| async { n.kv_get(format!("/q/redo/c/{}", first).as_bytes()).await.is_none() }).await,
            "the gone one is withdrawn");
        ack(&n, "/q/redo/1/m1").await;
        assert!(held(&q).await.is_empty());
        assert!(c2.quiet().await, "once");
    }

    /// [q-dead] One that cancels its watch receives no more, but keeps what it
    /// was handed while its stream is open - it may be finishing that work. Its
    /// stream closed, the message goes to the next consumer.
    #[tokio::test]
    async fn a_cancelled_consumer_keeps_its_message_until_its_stream_closes() {
        let n = node(A).await;
        let first = Uuid::new_v4();
        let mut c1 = Consumer::watch(&n, &first, &format!("/q/redo2/c/{}", first), 1).await;
        produce(&n, "/q/redo2/p/m1", "1").await;
        assert_eq!(c1.next().await.0, "/q/redo2/1/m1");
        let second = Uuid::new_v4();
        let mut c2 = Consumer::watch(&n, &second, &format!("/q/redo2/c/{}", second), 1).await;

        c1.req.send(WatchRequest { request_union: Some(watch_request::RequestUnion::CancelRequest(
            WatchCancelRequest { watch_id: 1 })) }).await.unwrap();
        assert!(c1.frame().await.unwrap().unwrap().canceled);
        let reg = format!("/q/redo2/c/{}", first);
        assert!(until(|| async { registration(&n, &reg).await.map(|r| (r.node, r.state))
            == Some((n.node_id, crate::route::StreamState::Draining)) }).await, "registered as draining");
        tokio::time::sleep(REDISPATCH * 2).await;
        assert!(c2.quiet().await, "kept for the cancelled one while its stream is open");

        produce(&n, "/q/redo2/p/m2", "2").await;
        assert_eq!(c2.next().await.0, "/q/redo2/2/m2", "new messages go to the receiving one");
        drop(c1); // its stream closes
        assert_eq!(c2.next_after_drain().await.0, "/q/redo2/1/m1", "then handed again");
        assert!(until(|| async { n.kv_get(reg.as_bytes()).await.is_none() }).await, "and withdrawn");
    }

    /// [q-dead] A consumer that stalls - its stream full - is slow, not gone:
    /// it gets nothing new, and keeps what it was handed, so a second consumer
    /// does not run it again. Its stream closed, that goes to the next one.
    #[tokio::test]
    async fn a_stalled_consumer_keeps_what_it_was_handed() {
        let n = node(A).await;
        let slow = Uuid::new_v4();
        let (req, req_rx) = mpsc::channel(10);
        let (tx, mut rx) = mpsc::channel(2); // the created frame, then two messages
        n.new_watcher(req_rx, tx, &slow).await;
        req.send(WatchRequest { request_union: Some(watch_request::RequestUnion::CreateRequest(
            WatchCreateRequest { key: format!("/q/full/c/{}", slow).into_bytes(), watch_id: 1, ..Default::default() })) })
            .await.unwrap();
        assert!(rx.recv().await.unwrap().unwrap().created);
        produce(&n, "/q/full/p/m1", "1").await;
        produce(&n, "/q/full/p/m2", "2").await;
        let q = queue(&n, "full").await;
        assert!(until(|| async { q.queue.read().await.iter().all(|m| m.handled_by == Some(slow)) }).await,
            "both in its stream, unread");
        produce(&n, "/q/full/p/m3", "3").await; // no room: it stalls
        tokio::time::sleep(DISPATCH_TIMEOUT).await;
        assert!(until(|| async { q.clients.read().await.is_empty() }).await, "dropped as slow");

        let next = Uuid::new_v4();
        let mut c = Consumer::watch(&n, &next, &format!("/q/full/c/{}", next), 1).await;
        assert_eq!(c.next().await.0, "/q/full/3/m3", "the one it did not take");
        tokio::time::sleep(REDISPATCH * 2).await;
        assert!(c.quiet().await, "not what the slow one holds");
        assert_eq!(rx.recv().await.unwrap().unwrap().events[0].kv.as_ref().unwrap().key, b"/q/full/1/m1",
            "the slow one reads on");
        ack(&n, "/q/full/1/m1").await; // finishes that one
        assert_eq!(rx.recv().await.unwrap().unwrap().events[0].kv.as_ref().unwrap().key, b"/q/full/2/m2");
        let cancel = rx.recv().await.unwrap().unwrap();
        assert!(cancel.canceled && cancel.watch_id == 1, "told its watch is over: {:?}", cancel);
        assert!(cancel.cancel_reason.starts_with("ETCD-E052 "), "{}", cancel.cancel_reason);
        assert!(!n.watchers.read().await[&slow].read().await.watchers.contains_key(&1), "as a cancel takes it");
        drop((req, rx)); // goes, m2 unfinished
        assert_eq!(c.next_after_drain().await.0, "/q/full/2/m2", "what it held and did not finish");
    }

    /// [q-dead] Cancels amid puts to a watched key: no put waits for good. A
    /// cancel writes the draining registration, and that put waits on the
    /// watch-notify task - which reads the watchers a cancel used to hold.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cancels_amid_watched_puts_stop_no_put() {
        let n = node(A).await;
        let mut w = Consumer::watch(&n, &Uuid::new_v4(), "/busy/k", 1).await; // a key observer
        tokio::spawn(async move { while w.rx.recv().await.is_some() {} });
        for round in 0..20 {
            let cid = Uuid::new_v4();
            let mut c = Consumer::watch(&n, &cid, &format!("/q/busy{}/c/{}", round, cid), 1).await;
            let puts: Vec<_> = (0..40).map(|i| {
                let n = n.clone();
                tokio::spawn(async move { n.kv_put(b"/busy/k".to_vec(), i.to_string().into_bytes()).await })
            }).collect();
            c.req.send(WatchRequest { request_union: Some(watch_request::RequestUnion::CancelRequest(
                WatchCancelRequest { watch_id: 1 })) }).await.unwrap();
            let all = tokio::time::timeout(Duration::from_secs(10), async {
                for p in puts { p.await.unwrap().unwrap(); }
            }).await;
            assert!(all.is_ok(), "round {}: a put never returned", round);
            assert!(c.frame().await.unwrap().unwrap().canceled);
        }
    }

    /// [q-dead] A cancelled consumer's stream still ends for its client once
    /// the client stops sending: nothing the queue keeps holds it open.
    #[tokio::test]
    async fn a_cancelled_consumer_s_stream_ends() {
        let n = node(A).await;
        let cid = Uuid::new_v4();
        let mut c = Consumer::watch(&n, &cid, &format!("/q/end/c/{}", cid), 1).await;
        c.req.send(WatchRequest { request_union: Some(watch_request::RequestUnion::CancelRequest(
            WatchCancelRequest { watch_id: 1 })) }).await.unwrap();
        assert!(c.frame().await.unwrap().unwrap().canceled);
        let Consumer { req, mut rx } = c;
        drop(req);
        let end = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;
        assert!(matches!(end, Ok(None)), "the stream ends: {:?}", end.map(|r| r.is_some()));
    }

    /// [q-dead] A consumer that is slow, not gone, keeps what it was handed:
    /// handing it to another would run the job twice (at-most-once).
    #[tokio::test]
    async fn a_live_consumer_keeps_what_it_was_handed() {
        let n = node(A).await;
        let first = Uuid::new_v4();
        let mut c1 = Consumer::watch(&n, &first, &format!("/q/keep/c/{}", first), 1).await;
        produce(&n, "/q/keep/p/m1", "1").await;
        assert_eq!(c1.next().await.0, "/q/keep/1/m1");
        let second = Uuid::new_v4();
        let mut c2 = Consumer::watch(&n, &second, &format!("/q/keep/c/{}", second), 1).await;
        tokio::time::sleep(REDISPATCH * 3).await; // past the loop's own looks
        assert!(c2.quiet().await, "not to another consumer");
        assert!(c1.quiet().await, "nor again to its own");
        assert_eq!(queue(&n, "keep").await.queue.read().await[0].handled_by, Some(first));
    }

    /// [q-dead] A message handed to a consumer on another node is kept while
    /// the registry places it there, and goes back when that node withdraws it -
    /// the delete of its registration, as a peer sends it.
    #[tokio::test]
    async fn a_message_handed_to_a_withdrawn_remote_consumer_goes_back() {
        let n = node(A).await;
        let ghost = Uuid::new_v4();
        let reg = format!("/q/away/c/{}", ghost);
        produce(&n, "/q/away/p/m1", "1").await;
        produce(&n, &reg, "987654321").await; // hosted on another node
        let q = queue(&n, "away").await;
        q.queue.write().await[0].handled_by = Some(ghost); // as if that node took it
        tokio::time::sleep(REDISPATCH * 2).await;
        assert_eq!(q.queue.read().await[0].handled_by, Some(ghost), "registered elsewhere: kept for it");

        // only the node it names withdraws it: not one it left
        let mut r = tonic::Request::new(DeleteRangeRequest { key: reg.clone().into_bytes(), ..Default::default() });
        r.metadata_mut().insert(crate::etcdpb::XPEER, "555".parse().unwrap());
        n.delete_range(r).await.unwrap();
        assert!(n.kv_get(reg.as_bytes()).await.is_some(), "another node's withdrawal is ignored");
        assert_eq!(q.queue.read().await[0].handled_by, Some(ghost));

        let mut r = tonic::Request::new(DeleteRangeRequest { key: reg.into_bytes(), ..Default::default() });
        r.metadata_mut().insert(crate::etcdpb::XPEER, "987654321".parse().unwrap());
        n.delete_range(r).await.unwrap();
        assert!(q.queue.read().await[0].handled_by.is_none(), "withdrawn: handed out again");
        let live = Uuid::new_v4();
        let mut c = Consumer::watch(&n, &live, &format!("/q/away/c/{}", live), 1).await;
        assert_eq!(c.next().await, ("/q/away/1/m1".to_string(), "1".to_string()));
    }

    /// [q-dead] A client that moved to another node - its registration there is
    /// newer - and left a stream here: when that stream closes, the
    /// registration there stays, and what the stream was handed and never
    /// acknowledged goes to the next consumer - unread in it, or not, it is
    /// nobody's but the line's now.
    #[tokio::test]
    async fn a_stream_left_behind_hands_back_what_it_was_handed() {
        use crate::route::{Registration, StreamState};
        let n = node(A).await;
        let cid = Uuid::new_v4();
        let reg = format!("/q/moved/c/{}", cid);
        let c = Consumer::watch(&n, &cid, &reg, 1).await;
        produce(&n, "/q/moved/p/m1", "1").await; // handed to it, never read
        let q = queue(&n, "moved").await;
        assert!(until(|| async { q.queue.read().await[0].handled_by == Some(cid) }).await);
        let there = Registration::new(987654321, next_gen(), StreamState::Receiving);
        produce(&n, &reg, &there.render()).await; // registered there since

        drop(c);
        assert!(until(|| async { q.queue.read().await[0].handled_by.is_none() }).await, "handed back");
        tokio::time::sleep(REDISPATCH).await;
        assert_eq!(registration(&n, &reg).await, Some(there), "still registered there");
        let next = Uuid::new_v4();
        let mut c2 = Consumer::watch(&n, &next, &format!("/q/moved/c/{}", next), 1).await;
        assert_eq!(c2.next().await.0, "/q/moved/1/m1");
    }

    /// [q-dead] A stream on another node: what was handed to it goes back when
    /// its node writes its tombstone - from that node only - whatever the
    /// registry holds; and the tombstone of an older stream leaves a newer
    /// registration where it is.
    #[tokio::test]
    async fn a_tombstone_hands_back_what_its_stream_was_handed() {
        use crate::route::{Registration, StreamState};
        let n = node(A).await;
        let cid = Uuid::new_v4();
        let reg = format!("/q/far2/c/{}", cid);
        produce(&n, "/q/far2/p/m1", "1").await;
        let q = queue(&n, "far2").await;
        let newer = Registration::new(987654321, 9, StreamState::Receiving);
        produce(&n, &reg, &newer.render()).await; // the client has a newer stream there now - first
        q.queue.write().await[0].handled_by = Some(cid); // as if node 987654321's stream 7 took it
        q.queue.write().await[0].stream = Some((987654321, 7));
        let tomb = |sender: &str| {
            let mut r = tonic::Request::new(PutRequest { key: reg.clone().into_bytes(),
                value: Registration::new(987654321, 7, StreamState::Closed).render().into_bytes(), ..Default::default() });
            r.metadata_mut().insert(crate::etcdpb::XPEER, sender.parse().unwrap());
            r
        };
        n.put(tomb("555")).await.unwrap();
        tokio::time::sleep(REDISPATCH * 2).await;
        assert_eq!(q.queue.read().await[0].handled_by, Some(cid), "not from another node");

        n.put(tomb("987654321")).await.unwrap();
        assert!(until(|| async { q.queue.read().await[0].handled_by.is_none() }).await, "handed back");
        assert_eq!(registration(&n, &reg).await, Some(newer), "the newer stream stays registered");
        let t9 = Registration::new(987654321, 9, StreamState::Closed);
        let mut r = tonic::Request::new(PutRequest { key: reg.clone().into_bytes(), value: t9.render().into_bytes(), ..Default::default() });
        r.metadata_mut().insert(crate::etcdpb::XPEER, "987654321".parse().unwrap());
        n.put(r).await.unwrap();
        assert!(registration(&n, &reg).await.is_none(), "its own tombstone removes it");
    }

    /// [q-dead] A stream that closes late - a check, then a write, raced a
    /// watch made again meanwhile - withdraws only itself: the newer stream's
    /// registration stays.
    #[tokio::test]
    async fn an_older_stream_closing_leaves_a_newer_registration() {
        use crate::route::StreamState;
        let n = node(A).await;
        let cid = Uuid::new_v4();
        let reg = format!("/q/late/c/{}", cid);
        let _c = Consumer::watch(&n, &cid, &reg, 1).await;
        let now = registration(&n, &reg).await.unwrap();
        assert_eq!(now.state, StreamState::Receiving);
        queue(&n, "late").await.stream_closed(&cid, now.generation - 1).await;
        assert_eq!(registration(&n, &reg).await, Some(now), "still registered");
    }

    /// [q-dead] One that cancels and watches again on the same stream keeps
    /// that stream - and what it was handed on it - and receives again.
    #[tokio::test]
    async fn a_watch_made_again_on_a_draining_stream_receives_again() {
        use crate::route::StreamState;
        let n = node(A).await;
        let cid = Uuid::new_v4();
        let reg = format!("/q/again2/c/{}", cid);
        let mut c = Consumer::watch(&n, &cid, &reg, 1).await;
        produce(&n, "/q/again2/p/m1", "1").await;
        assert_eq!(c.next().await.0, "/q/again2/1/m1");
        let first = registration(&n, &reg).await.unwrap();
        c.req.send(WatchRequest { request_union: Some(watch_request::RequestUnion::CancelRequest(
            WatchCancelRequest { watch_id: 1 })) }).await.unwrap();
        assert!(c.frame().await.unwrap().unwrap().canceled);
        assert!(until(|| async { registration(&n, &reg).await.map(|r| r.state) == Some(StreamState::Draining) }).await);

        c.create(&reg, 2).await; // the same stream
        assert!(until(|| async { registration(&n, &reg).await == Some(first) }).await, "receiving, the same stream");
        tokio::time::sleep(REDISPATCH * 2).await;
        assert_eq!(registration(&n, &reg).await, Some(first), "and stays so");
        let q = queue(&n, "again2").await;
        assert_eq!(q.queue.read().await[0].handled_by, Some(cid), "it keeps what it has");
        produce(&n, "/q/again2/p/m2", "2").await;
        assert_eq!(c.next().await.0, "/q/again2/2/m2");
    }

    /// [q-dead] The loop's own check: a message held for a consumer that is
    /// neither on this node nor registered on another goes back, with no event
    /// to say so - as one dropped between its delivery and the mark of it.
    #[tokio::test]
    async fn a_message_held_for_a_consumer_known_nowhere_goes_back() {
        let n = node(A).await;
        produce(&n, "/q/nowhere/p/m1", "1").await;
        let q = queue(&n, "nowhere").await;
        q.queue.write().await[0].handled_by = Some(Uuid::new_v4());
        assert!(until(|| async { q.queue.read().await[0].handled_by.is_none() }).await,
            "the loop's next look hands it back");
        let live = Uuid::new_v4();
        let mut c = Consumer::watch(&n, &live, &format!("/q/nowhere/c/{}", live), 1).await;
        assert_eq!(c.next().await.0, "/q/nowhere/1/m1");
    }

    /// [q-registry] What a node writes again when a peer joins: each stream
    /// here as it is - receiving, or draining - from the streams it holds, not
    /// from the store; the tombstone of a registration naming it that it no
    /// longer holds; and the dispatcher record while it names this node and a
    /// consumer is here. Not over a newer registration of another node - the
    /// client moved there - nor a claim another node made since, nor the
    /// record of a queue with no consumer here.
    #[tokio::test]
    async fn the_registry_written_again_is_this_node_s_own() {
        use crate::route::{Registration, StreamState};
        let n = node(A).await;
        let cid = Uuid::new_v4();
        let reg = format!("/q/again/c/{}", cid);
        let mut c = Consumer::watch(&n, &cid, &reg, 1).await;
        produce(&n, "/q/solo/p/m1", "1").await; // a producer's claim, no consumer here
        assert_eq!(n.queue_dispatcher("/q/solo").await, Some(n.node_id));
        assert_eq!(n.republish_registry().await, 2, "the consumer and its node's record, not /q/solo");
        let held = registration(&n, &reg).await.unwrap();
        assert_eq!((held.node, held.state), (n.node_id, StreamState::Receiving));

        // from the streams held, not from the store; one left behind goes
        n.vault.write().await.remove(reg.as_bytes());
        let behind = format!("/q/again/c/{}", Uuid::new_v4());
        produce(&n, &behind, &n.node_id.to_string()).await;
        assert_eq!(n.republish_registry().await, 3, "the consumer, the record, the tombstone of the one left behind");
        assert_eq!(registration(&n, &reg).await, Some(held), "back");
        assert!(n.kv_get(behind.as_bytes()).await.is_none(), "withdrawn");

        // a draining stream is written as draining, and not withdrawn
        c.req.send(WatchRequest { request_union: Some(watch_request::RequestUnion::CancelRequest(
            WatchCancelRequest { watch_id: 1 })) }).await.unwrap();
        assert!(c.frame().await.unwrap().unwrap().canceled);
        let draining = Registration::new(n.node_id, held.generation, StreamState::Draining).since(held.since);
        assert!(until(|| async { registration(&n, &reg).await == Some(draining) }).await);
        n.republish_registry().await;
        assert_eq!(registration(&n, &reg).await, Some(draining), "draining as it is");

        // the client moved: its newer registration there is not written over
        let there = Registration::new(987654321, next_gen(), StreamState::Receiving);
        produce(&n, &reg, &there.render()).await;
        n.republish_registry().await;
        assert_eq!(registration(&n, &reg).await, Some(there), "the newer one there stands");

        produce(&n, "/q/solo", "987654321").await; // another node claimed it since
        n.republish_registry().await;
        assert_eq!(n.queue_dispatcher("/q/solo").await, Some(987654321), "the other claim stands");
    }

    /// [q-dead] A registration naming this node under a queue it does not have
    /// - one a restart pulled back from a peer - is withdrawn when the registry
    /// is written again, as at start.
    #[tokio::test]
    async fn a_registration_from_before_a_restart_is_withdrawn() {
        use crate::route::{Registration, StreamState};
        let n = node(A).await;
        let reg = format!("/q/before/c/{}", Uuid::new_v4());
        produce(&n, &reg, &Registration::new(n.node_id, 5, StreamState::Receiving).render()).await;
        assert!(n.queues.read().await.get("before").is_none());
        assert_eq!(n.republish_registry().await, 1);
        assert!(n.kv_get(reg.as_bytes()).await.is_none());
    }

    /// [q-dead] Cancel, then watch again on the same stream, while the
    /// registration writes are held up: the draining write, decided before the
    /// watch was made again, lands with the receiving one in either order -
    /// and the registration still ends as the stream is, receiving.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_draining_write_that_lands_late_is_undone() {
        use crate::route::StreamState;
        let n = node(A).await;
        let cid = Uuid::new_v4();
        let reg = format!("/q/flap/c/{}", cid);
        let mut c = Consumer::watch(&n, &cid, &reg, 1).await;
        // `created` is sent before the consumer is registered: let that end
        assert!(until(|| async { registration(&n, &reg).await.map(|r| r.state) == Some(StreamState::Receiving) }).await);
        tokio::time::sleep(Duration::from_millis(100)).await;
        for id in 2..12 {
            let held = n.vault.write().await; // the writes wait here
            c.req.send(WatchRequest { request_union: Some(watch_request::RequestUnion::CancelRequest(
                WatchCancelRequest { watch_id: id - 1 })) }).await.unwrap();
            assert!(c.frame().await.unwrap().unwrap().canceled);
            tokio::time::sleep(Duration::from_millis(50)).await; // draining decided
            c.create(&reg, id).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            drop(held);
            assert!(until(|| async { registration(&n, &reg).await.map(|r| r.state) == Some(StreamState::Receiving) }).await,
                "round {}", id);
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert_eq!(registration(&n, &reg).await.map(|r| r.state), Some(StreamState::Receiving), "round {}: stays", id);
        }
    }

    /// [q-dead] A registration of a node from a later incarnation releases
    /// what was handed to that node's streams from before its restart -
    /// nobody is left to write their tombstones - and so does the loop's
    /// check, for one it missed.
    #[tokio::test]
    async fn a_registration_from_a_restarted_node_releases_its_older_streams() {
        use crate::route::{Registration, StreamState};
        let n = node(A).await;
        let cid = Uuid::new_v4();
        let reg = format!("/q/inc/c/{}", cid);
        produce(&n, "/q/inc/p/m1", "1").await;
        produce(&n, "/q/inc/p/m2", "2").await;
        let q = queue(&n, "inc").await;
        produce(&n, &reg, &Registration::new(987654321, 5, StreamState::Draining).render()).await; // first
        for m in q.queue.write().await.iter_mut() {
            m.handled_by = Some(cid);
            m.stream = Some((987654321, 5));
        }
        tokio::time::sleep(REDISPATCH * 2).await;
        assert!(q.queue.read().await.iter().all(|m| m.handled_by.is_some()), "its stream, alive");

        let mut r = tonic::Request::new(PutRequest { key: reg.clone().into_bytes(),
            value: Registration::new(987654321, 100, StreamState::Receiving).since(50).render().into_bytes(), ..Default::default() });
        r.metadata_mut().insert(crate::etcdpb::XPEER, "987654321".parse().unwrap());
        n.put(r).await.unwrap();
        assert!(until(|| async { q.queue.read().await.iter().all(|m| m.stream != Some((987654321, 5))) }).await,
            "from before the restart: handed back");
    }

    /// [q-dead] A registration its tombstone already superseded - the two
    /// crossed on the way here - is not stored; a newer one is.
    #[tokio::test]
    async fn a_registration_behind_its_tombstone_is_not_stored() {
        use crate::route::{Registration, StreamState};
        let n = node(A).await;
        let reg = format!("/q/cross/c/{}", Uuid::new_v4());
        let from_x = |v: String| {
            let mut r = tonic::Request::new(PutRequest { key: reg.clone().into_bytes(), value: v.into_bytes(), ..Default::default() });
            r.metadata_mut().insert(crate::etcdpb::XPEER, "987654321".parse().unwrap());
            r
        };
        n.put(from_x(Registration::new(987654321, 7, StreamState::Closed).render())).await.unwrap();
        n.put(from_x(Registration::new(987654321, 7, StreamState::Receiving).render())).await.unwrap();
        assert!(n.kv_get(reg.as_bytes()).await.is_none(), "behind its tombstone");
        let newer = Registration::new(987654321, 8, StreamState::Receiving);
        n.put(from_x(newer.render())).await.unwrap();
        assert_eq!(registration(&n, &reg).await, Some(newer));
    }

    /// [q-dead] A newer tombstone of another node removes this node's older
    /// registration of the client wherever it lands - here too - and this
    /// node, which still has the client's stream, writes it again.
    #[tokio::test]
    async fn a_newer_tombstone_of_another_node_leaves_this_node_registered() {
        use crate::route::{Registration, StreamState};
        let n = node(A).await;
        let cid = Uuid::new_v4();
        let reg = format!("/q/both/c/{}", cid);
        let _c = Consumer::watch(&n, &cid, &reg, 1).await;
        let mine = registration(&n, &reg).await.unwrap();
        let mut r = tonic::Request::new(PutRequest { key: reg.clone().into_bytes(),
            value: Registration::new(987654321, mine.generation + 1_000_000, StreamState::Closed).render().into_bytes(),
            ..Default::default() });
        r.metadata_mut().insert(crate::etcdpb::XPEER, "987654321".parse().unwrap());
        n.put(r).await.unwrap();
        assert!(until(|| async { registration(&n, &reg).await == Some(mine) }).await, "written again");
    }

    /// [q-dead] A send that waited on a full stream drops its watcher only if
    /// that watch is still on that stream: the client may have watched again,
    /// under the same id, on another - which keeps receiving.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_watch_made_again_on_another_stream_is_not_dropped_for_the_old_one() {
        let n = node(A).await;
        let cid = Uuid::new_v4();
        let key = format!("/q/l6/c/{}", cid);
        let create = || WatchRequest { request_union: Some(watch_request::RequestUnion::CreateRequest(
            WatchCreateRequest { key: key.clone().into_bytes(), watch_id: 1, ..Default::default() })) };
        let (req1, req1_rx) = mpsc::channel(10);
        let (tx1, mut rx1) = mpsc::channel(2);
        n.new_watcher(req1_rx, tx1, &cid).await;
        req1.send(create()).await.unwrap();
        assert!(rx1.recv().await.unwrap().unwrap().created);
        produce(&n, "/q/l6/p/m1", "1").await;
        produce(&n, "/q/l6/p/m2", "2").await; // its stream is full
        let q = queue(&n, "l6").await;
        assert!(until(|| async { q.queue.read().await.iter().filter(|m| m.handled_by.is_some()).count() == 2 }).await);
        produce(&n, "/q/l6/p/m3", "3").await; // the loop waits on it

        tokio::time::sleep(Duration::from_millis(500)).await;
        let (req2, req2_rx) = mpsc::channel(10);
        let (tx2, mut rx2) = mpsc::channel(10);
        n.new_watcher(req2_rx, tx2, &cid).await;
        req2.send(create()).await.unwrap(); // the same client and id, another stream
        let got = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Ok(r) = rx2.recv().await.unwrap() {
                    if let Some(kv) = r.events.first().and_then(|e| e.kv.clone()) {
                        return kv.key;
                    }
                }
            }
        }).await.expect("the new stream receives");
        assert_eq!(got, b"/q/l6/3/m3");
        drop((req1, rx1, req2));
    }

    /// [q-dead] A consumer's stream closed is taken off as a cancel takes it:
    /// back under the same client and watch id, it watches again, rather than
    /// being told the watch exists.
    #[tokio::test]
    async fn a_client_back_under_the_same_ids_watches_again() {
        let n = node(A).await;
        let cid = Uuid::new_v4();
        let key = format!("/q/back2/c/{}", cid);
        let c = Consumer::watch(&n, &cid, &key, 1).await;
        drop(c);
        let q = queue(&n, "back2").await;
        assert!(until(|| async { q.clients.read().await.is_empty() }).await);
        assert!(until(|| async { n.watchers.read().await.get(&cid)
            .map_or(true, |c| c.try_read().map_or(false, |c| !c.watchers.contains_key(&1))) }).await, "the watch is gone");
        let (req, req_rx) = mpsc::channel(10);
        let (tx, mut rx) = mpsc::channel(10);
        n.new_watcher(req_rx, tx, &cid).await;
        req.send(WatchRequest { request_union: Some(watch_request::RequestUnion::CreateRequest(
            WatchCreateRequest { key: key.into_bytes(), watch_id: 1, ..Default::default() })) }).await.unwrap();
        let first = rx.recv().await.unwrap();
        assert!(first.as_ref().map_or(false, |r| r.created), "created, not refused: {:?}", first.err());
    }

    /// [q-dead] The registry written again when a peer joins goes to the
    /// peers; unchanged here, a watch here on it hears nothing.
    #[tokio::test]
    async fn a_registry_written_again_unchanged_is_no_news_here() {
        let n = node(A).await;
        let cid = Uuid::new_v4();
        let _c = Consumer::watch(&n, &cid, &format!("/q/bo/c/{}", cid), 1).await;
        let (req, req_rx) = mpsc::channel(10);
        let (tx, mut rx) = mpsc::channel(10);
        n.new_watcher(req_rx, tx, &Uuid::new_v4()).await;
        req.send(WatchRequest { request_union: Some(watch_request::RequestUnion::CreateRequest(
            WatchCreateRequest { key: b"/q/".to_vec(), range_end: b"/q0".to_vec(), watch_id: 1, ..Default::default() })) })
            .await.unwrap();
        assert!(rx.recv().await.unwrap().unwrap().created);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(n.republish_registry().await >= 1, "written to the peers");
        assert!(tokio::time::timeout(Duration::from_millis(300), rx.recv()).await.is_err(), "no event here");
        drop(req);
    }

    /// [q-dead] A consumer on a node down past the down timeout is withdrawn
    /// on its behalf, and what it held goes to the next consumer; down for less,
    /// or with the timeout 0, it keeps it.
    #[tokio::test]
    async fn a_consumer_on_a_node_down_past_the_timeout_is_withdrawn_on_its_behalf() {
        use crate::route::{Registration, StreamState};
        let n = node(A).await;
        fake_peer(&n, 987654321, &crate::peer::status_version()).await;
        let cid = Uuid::new_v4();
        let reg = format!("/q/down/c/{}", cid);
        produce(&n, "/q/down/p/m1", "1").await;
        let q = queue(&n, "down").await;
        produce(&n, &reg, &Registration::new(987654321, 7, StreamState::Receiving).render()).await;
        q.queue.write().await[0].handled_by = Some(cid); // handed to its stream 7 there
        q.queue.write().await[0].stream = Some((987654321, 7));
        let ago = |s: u64| Some(Instant::now().checked_sub(Duration::from_secs(s)).unwrap());

        down_since(&n, 987654321, None).await; // the writes above found it gone: as if it answered
        assert!(q.remote_consumers().await.iter().any(|(c, _, _)| *c == cid), "routed to while it answers");
        n.cfg.write().await.queue_down_timeout = Some(60);
        down_since(&n, 987654321, ago(30)).await;
        assert!(q.remote_consumers().await.is_empty(), "not while it is down");
        tokio::time::sleep(REDISPATCH * 2).await;
        assert_eq!(q.queue.read().await[0].handled_by, Some(cid), "down 30 s of 60: kept");
        n.cfg.write().await.queue_down_timeout = Some(0);
        down_since(&n, 987654321, ago(600)).await;
        tokio::time::sleep(REDISPATCH * 2).await;
        assert_eq!(q.queue.read().await[0].handled_by, Some(cid), "0: never");

        n.cfg.write().await.queue_down_timeout = Some(60);
        q.wake(); // look now, not at the loop's next timed look
        assert!(until(|| async { q.queue.read().await[0].handled_by.is_none() }).await, "down 10 minutes of 1: handed back");
        assert!(n.kv_get(reg.as_bytes()).await.is_none(), "and its registration withdrawn");
        assert_eq!(crate::cli::EtcdConfig::with_defaults().down_timeout(), Some(Duration::from_secs(300)), "5 minutes unset");
    }

    /// [q-dead] A tombstone written on behalf of a down node is taken from any
    /// node, and not remembered against its node's own registration after.
    #[tokio::test]
    async fn a_tombstone_on_behalf_of_a_down_node_is_taken_from_any_node() {
        use crate::route::{Registration, StreamState};
        let n = node(A).await;
        let cid = Uuid::new_v4();
        let reg = format!("/q/behalf/c/{}", cid);
        produce(&n, "/q/behalf/p/m1", "1").await;
        let q = queue(&n, "behalf").await;
        produce(&n, &reg, &Registration::new(987654321, 7, StreamState::Draining).render()).await;
        q.queue.write().await[0].handled_by = Some(cid);
        q.queue.write().await[0].stream = Some((987654321, 7));
        let from = |sender: &str, v: String| {
            let mut r = tonic::Request::new(PutRequest { key: reg.clone().into_bytes(), value: v.into_bytes(), ..Default::default() });
            r.metadata_mut().insert(crate::etcdpb::XPEER, sender.parse().unwrap());
            r
        };
        n.put(from("555", Registration::gone(987654321, 7).render())).await.unwrap();
        assert!(n.kv_get(reg.as_bytes()).await.is_none(), "taken from node 555");
        assert!(until(|| async { q.queue.read().await[0].handled_by.is_none() }).await, "and what it held handed back");
        let back = Registration::new(987654321, 7, StreamState::Receiving);
        n.put(from("987654321", back.render())).await.unwrap();
        assert_eq!(registration(&n, &reg).await, Some(back), "its node, back, registers it again");
    }

    /// [q-dead] With an older member here the registry is written as it reads
    /// it - the bare node id - and withdrawn by a delete; with none, in full.
    #[tokio::test]
    async fn an_older_member_gets_the_registry_in_its_form() {
        let n = node(A).await;
        fake_peer(&n, 987654321, "rust").await; // an older node answers so
        assert!(n.peers.read().await.legacy_registry().await);
        let cid = Uuid::new_v4();
        let reg = format!("/q/old/c/{}", cid);
        let c = Consumer::watch(&n, &cid, &reg, 1).await;
        assert!(until(|| async { n.kv_get(reg.as_bytes()).await == Some(n.node_id.to_string().into_bytes()) }).await,
            "the bare node id");

        reads_registry(&n, 987654321, crate::peer::REGISTRY).await; // upgraded
        n.republish_registry().await;
        assert!(registration(&n, &reg).await.map_or(false, |r| r.generation > 0), "in full once none is older");
        reads_registry(&n, 987654321, 1).await;
        n.republish_registry().await;
        assert_eq!(n.kv_get(reg.as_bytes()).await, Some(n.node_id.to_string().into_bytes()), "and back");

        drop(c); // its stream closes: deleted, no tombstone left as a value
        assert!(until(|| async { n.kv_get(reg.as_bytes()).await.is_none() }).await);
        tokio::time::sleep(REDISPATCH).await;
        assert!(n.kv_get(reg.as_bytes()).await.is_none());
        assert!(n.tombstones.read().await.get(&(reg.clone().into_bytes(), n.node_id)).is_none(), "a delete, not a tombstone");
    }

    /// [q-dead] Generation 0 - a stream marked while an older member was here -
    /// is unknown, not older: the node's later registration in full, from a
    /// later incarnation, does not release it; and a bare registration is not
    /// dead by a tombstone of that node.
    #[tokio::test]
    async fn generation_0_is_unknown_not_older() {
        use crate::route::{Registration, StreamState};
        let n = node(A).await;
        let cid = Uuid::new_v4();
        let reg = format!("/q/zero/c/{}", cid);
        produce(&n, "/q/zero/p/m1", "1").await;
        let q = queue(&n, "zero").await;
        let from_x = |v: String| {
            let mut r = tonic::Request::new(PutRequest { key: reg.clone().into_bytes(), value: v.into_bytes(), ..Default::default() });
            r.metadata_mut().insert(crate::etcdpb::XPEER, "987654321".parse().unwrap());
            r
        };
        n.put(from_x(Registration::new(987654321, 0, StreamState::Receiving).render_legacy())).await.unwrap();
        q.queue.write().await[0].handled_by = Some(cid); // handed while it wrote the bare id
        q.queue.write().await[0].stream = Some((987654321, 0));
        n.put(from_x(Registration::new(987654321, 100, StreamState::Receiving).since(50).render())).await.unwrap();
        tokio::time::sleep(REDISPATCH * 2).await;
        assert_eq!(q.queue.read().await[0].handled_by, Some(cid), "the end of the upgrade does not hand it out");

        n.put(from_x(Registration::new(987654321, 7, StreamState::Closed).render())).await.unwrap();
        n.put(from_x(Registration::new(987654321, 0, StreamState::Receiving).render_legacy())).await.unwrap();
        assert_eq!(n.kv_get(reg.as_bytes()).await, Some(b"987654321".to_vec()), "a bare one is stored after a tombstone");
    }

    /// [q-dead] A stream that closes while an older member is here is
    /// remembered, so its tombstone is written once the registry is back in
    /// full.
    #[tokio::test]
    async fn a_stream_closed_in_the_older_form_is_remembered() {
        let n = node(A).await;
        fake_peer(&n, 987654321, "rust").await;
        let cid = Uuid::new_v4();
        let c = Consumer::watch(&n, &cid, &format!("/q/rem/c/{}", cid), 1).await;
        let q = queue(&n, "rem").await;
        drop(c);
        assert!(until(|| async { q.closed.read().await.iter().any(|(c, _, _)| *c == cid) }).await);
    }

    /// [q-dead] A slow consumer is told once, however often its stall is seen:
    /// a second frame would have it watch twice.
    #[tokio::test]
    async fn a_slow_consumer_is_told_once() {
        let n = node(A).await;
        let cid = Uuid::new_v4();
        let (req, req_rx) = mpsc::channel(10);
        let (tx, mut rx) = mpsc::channel(10);
        n.new_watcher(req_rx, tx.clone(), &cid).await;
        req.send(WatchRequest { request_union: Some(watch_request::RequestUnion::CreateRequest(
            WatchCreateRequest { key: format!("/q/once/c/{}", cid).into_bytes(), watch_id: 1, ..Default::default() })) })
            .await.unwrap();
        assert!(rx.recv().await.unwrap().unwrap().created);
        let q = queue(&n, "once").await;
        q.cancel_stalled(&cid, 1, tx.clone());
        tokio::time::sleep(Duration::from_millis(200)).await;
        q.cancel_stalled(&cid, 1, tx);
        let mut told = 0;
        while let Ok(Some(Ok(r))) = tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
            if r.canceled { told += 1; }
        }
        assert_eq!(told, 1);
        drop(req);
    }

    /// [q-dead] A client re-using a watch id while its stream is full is told
    /// so without holding the watchers: no other watch's notification, and no
    /// write, waits on it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_duplicate_watch_on_a_full_stream_holds_up_no_write() {
        let n = node(A).await;
        let cid = Uuid::new_v4();
        let (req, req_rx) = mpsc::channel(10);
        let (tx, mut rx) = mpsc::channel(1);
        n.new_watcher(req_rx, tx, &cid).await;
        let create = |key: &str| WatchRequest { request_union: Some(watch_request::RequestUnion::CreateRequest(
            WatchCreateRequest { key: key.as_bytes().to_vec(), watch_id: 1, ..Default::default() })) };
        req.send(create("/dup/a")).await.unwrap();
        assert!(rx.recv().await.unwrap().unwrap().created);
        n.kv_put(b"/dup/a".to_vec(), b"1".to_vec()).await.unwrap(); // its stream is full now
        req.send(create("/dup/b")).await.unwrap(); // the same id again
        tokio::time::sleep(Duration::from_millis(300)).await;
        let mut other = Consumer::watch(&n, &Uuid::new_v4(), "/dup/c", 1).await;
        let puts: Vec<_> = (0..14).map(|i| {
            let n = n.clone();
            tokio::spawn(async move { n.kv_put(b"/dup/c".to_vec(), i.to_string().into_bytes()).await })
        }).collect();
        tokio::spawn(async move { while other.rx.recv().await.is_some() {} });
        let all = tokio::time::timeout(Duration::from_secs(8), async { for p in puts { p.await.unwrap().unwrap(); } }).await;
        assert!(all.is_ok(), "the writes do not wait on it");
        drop((req, rx));
    }

    /// [q-dead] A key watcher that does not take its notifications in time is
    /// cancelled, as a stalled queue consumer is: the writes to its key do not
    /// wait on it, and it is told why.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_key_watcher_that_does_not_read_is_cancelled() {
        let n = node(A).await;
        let cid = Uuid::new_v4();
        let (req, req_rx) = mpsc::channel(10);
        let (tx, mut rx) = mpsc::channel(1);
        n.new_watcher(req_rx, tx, &cid).await;
        req.send(WatchRequest { request_union: Some(watch_request::RequestUnion::CreateRequest(
            WatchCreateRequest { key: b"/k4".to_vec(), watch_id: 1, ..Default::default() })) }).await.unwrap();
        assert!(rx.recv().await.unwrap().unwrap().created);
        let puts: Vec<_> = (0..14).map(|i| {
            let n = n.clone();
            tokio::spawn(async move { n.kv_put(b"/k4".to_vec(), i.to_string().into_bytes()).await })
        }).collect();
        let all = tokio::time::timeout(Duration::from_secs(8), async { for p in puts { p.await.unwrap().unwrap(); } }).await;
        assert!(all.is_ok(), "no write waits on a watcher that does not read");
        assert!(until(|| async { n.watchers.read().await.get(&cid)
            .map_or(true, |c| c.try_read().map_or(false, |c| !c.watchers.contains_key(&1))) }).await, "its watch is gone");
        let mut told = None;
        while let Ok(Some(Ok(r))) = tokio::time::timeout(Duration::from_secs(3), rx.recv()).await {
            if r.canceled { told = Some(r.cancel_reason); break; }
        }
        assert!(told.map_or(false, |r| r.starts_with("ETCD-E085 ")), "told, with the reason");
        drop(req);
    }

    /// [q-dead] A consumer dropped as slow that also watches a key on the same
    /// stream: the watch-notify task holds the watchers while it waits for room
    /// in that stream, and the dispatch loop does not wait with it - the next
    /// consumer still gets what the slow one did not take.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_slow_consumer_with_a_key_watch_holds_up_no_one() {
        let n = node(A).await;
        let slow = Uuid::new_v4();
        let (req, req_rx) = mpsc::channel(10);
        let (tx, mut rx) = mpsc::channel(1);
        n.new_watcher(req_rx, tx, &slow).await;
        for (key, id) in [("/slowk".to_string(), 1), (format!("/q/sk/c/{}", slow), 2)] {
            req.send(WatchRequest { request_union: Some(watch_request::RequestUnion::CreateRequest(
                WatchCreateRequest { key: key.into_bytes(), watch_id: id, ..Default::default() })) }).await.unwrap();
            assert!(rx.recv().await.unwrap().unwrap().created);
        }
        produce(&n, "/q/sk/p/m1", "1").await; // fills its stream
        let q = queue(&n, "sk").await;
        assert!(until(|| async { q.queue.read().await[0].handled_by == Some(slow) }).await);
        // the notify task now waits on its stream, and the puts after fill the
        // notify queue: a registration written now waits too
        for i in 0..12 {
            let n = n.clone();
            tokio::spawn(async move { n.kv_put(b"/slowk".to_vec(), i.to_string().into_bytes()).await });
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        produce(&n, "/q/sk/p/m2", "2").await; // it stalls on this one
        tokio::time::sleep(DISPATCH_TIMEOUT + Duration::from_millis(500)).await;

        let next = Uuid::new_v4();
        let (req2, req2_rx) = mpsc::channel(10);
        let (tx2, mut rx2) = mpsc::channel(10);
        n.new_watcher(req2_rx, tx2, &next).await;
        req2.send(WatchRequest { request_union: Some(watch_request::RequestUnion::CreateRequest(
            WatchCreateRequest { key: format!("/q/sk/c/{}", next).into_bytes(), watch_id: 1, ..Default::default() })) })
            .await.unwrap();
        let got = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let r = rx2.recv().await.unwrap().unwrap();
                if let Some(kv) = r.events.first().and_then(|e| e.kv.clone()) {
                    return kv.key;
                }
            }
        }).await.expect("the next consumer is served");
        assert_eq!(got, b"/q/sk/2/m2");
        drop((req, rx, req2));
    }

    /// [q-dead] A consumer dropped as slow is taken off as a cancel takes it -
    /// a range watch's observer and range too - so a watch made again under
    /// its id is one watch, not two.
    #[tokio::test]
    async fn a_slow_range_consumer_leaves_no_observer_behind() {
        let n = node(A).await;
        let slow = Uuid::new_v4();
        let key = format!("/q/rng/c/{}", slow);
        let (req, req_rx) = mpsc::channel(10);
        let (tx, mut rx) = mpsc::channel(2);
        n.new_watcher(req_rx, tx, &slow).await;
        req.send(WatchRequest { request_union: Some(watch_request::RequestUnion::CreateRequest(
            WatchCreateRequest { key: key.clone().into_bytes(), range_end: format!("{}0", key).into_bytes(),
                watch_id: 1, ..Default::default() })) }).await.unwrap();
        assert!(rx.recv().await.unwrap().unwrap().created);
        assert!(n.observers.read().await.get(key.as_bytes()).map_or(false, |o| o.contains(&(slow, 1))));
        // its own registration write is an event in its range: the stream fills
        for i in 0..3 {
            produce(&n, &format!("/q/rng/p/m{}", i), "v").await;
        }
        tokio::time::sleep(DISPATCH_TIMEOUT + Duration::from_millis(500)).await;
        // it reads on - the cancel waits for the notify task, which waits on it
        tokio::spawn(async move { while rx.recv().await.is_some() {} });
        assert!(until(|| async { !n.observers.read().await.get(key.as_bytes()).map_or(false, |o| o.contains(&(slow, 1))) }).await,
            "its observer is gone");
        assert!(until(|| async { n.observer_ranges.read().await.get(key.as_bytes()).is_none() }).await, "and its range");
        drop(req);
    }

    #[test]
    fn queue_keys_are_classified_by_shape() {
        let k = QueueNameKey::new("/queue/jobs/consumer/7e1d0f57-1d3c-4a7b-9b5c-2a1d1e9f0a11/12/tail".into());
        assert!(k.consumer && !k.producer && k.is_queue());
        assert_eq!((k.idx, k.tail.as_str()), (Some(12), "tail"));
        assert!(k.client_id.is_some());
        let k = QueueNameKey::new("/q/jobs/i/key".into());
        assert!(k.producer && k.idx.is_none());
        assert_eq!(k.name(), "/q/jobs");
        let k = QueueNameKey::new("/q/jobs/4/key".into());
        assert_eq!(k.idx, Some(4), "a delivery key carries its index");
        assert!(!QueueNameKey::new("/other/jobs/p/k".into()).is_queue());
        assert!(!QueueNameKey::new("/q".into()).is_queue());
    }
}

#[cfg(test)]
mod routing_tests {
    //! [q-route] Two nodes of one cluster, peered and Online, and the routes a
    //! message takes between them (queue-p2p-route.md).
    use super::node_tests::{ack, held, node, produce, queue, until, Consumer};
    use super::*;
    use crate::etcdpb::etcdserverpb::{watch_request, WatchRequest};
    use crate::etcdpb::etcdserverpb::kv_server::Kv as _;
    use crate::{EtcdEvents, EtcdMgmtEvent};
    use std::time::Duration;

    const A: &str = "77777777-7777-5777-8777-777777777771";
    const B: &str = "77777777-7777-5777-8777-777777777772";

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }

    /// Two served nodes of one cluster that do not know each other yet - as
    /// every node is when a cluster starts together - and their urls.
    async fn served_pair() -> (EtcdNode, EtcdNode, Vec<String>) {
        let ports = [free_port(), free_port()];
        let urls: Vec<String> = ports.iter().map(|p| format!("http://127.0.0.1:{}", p)).collect();
        let mut nodes = Vec::new();
        for (id, p) in [A, B].iter().zip(ports) {
            let n = node(id).await;
            n.cfg.write().await.listen_client_urls = format!("127.0.0.1:{}", p);
            n.serve().await.unwrap();
            nodes.push(n);
        }
        for p in ports {
            assert!(until(|| async { tokio::net::TcpStream::connect(("127.0.0.1", p)).await.is_ok() }).await);
        }
        (nodes[0].clone(), nodes[1].clone(), urls)
    }

    /// Peer them, each holding the other Online.
    async fn peer_up(a: &EtcdNode, b: &EtcdNode, urls: &[String]) {
        // what an embedder sends once it knows the roster
        for (i, n) in [a, b].iter().enumerate() {
            let mut c = crate::cli::EtcdConfig::default();
            c.listen_client_urls = urls[i].clone();
            c.initial_advertise_peer_urls = urls.join(",");
            n.event.send(EtcdEvents::Mgmt(EtcdMgmtEvent::Config(c))).await.unwrap();
        }
        for n in [a, b] {
            assert!(until(|| async { n.peers.read().await.connected() == 1 }).await, "peered");
        }
        // one replicated write each way proves the peer, which is what Online means
        a.kv_put(b"/warm/a".to_vec(), b"1".to_vec()).await.unwrap();
        b.kv_put(b"/warm/b".to_vec(), b"1".to_vec()).await.unwrap();
        assert!(until(|| async { a.peers.read().await.is_online(b.node_id).await }).await);
        assert!(until(|| async { b.peers.read().await.is_online(a.node_id).await }).await);
    }

    /// Two served nodes that know each other, each holding the other Online.
    async fn online_pair() -> (EtcdNode, EtcdNode) {
        let (a, b, urls) = served_pair().await;
        peer_up(&a, &b, &urls).await;
        (a, b)
    }

    /// A producer on another node sends the dispatcher one unicast, and keeps
    /// no copy. The consumer on the dispatcher's node receives it under the
    /// producer's own key - the documented gap (agent.md, "Known gaps"): that
    /// key has no index, so the ack removes nothing and the message stays held.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_remote_producer_forwards_to_the_dispatcher_and_keeps_nothing() {
        let (a, b) = online_pair().await;
        let cid = Uuid::new_v4();
        let mut c = Consumer::watch(&a, &cid, &format!("/q/fwd/consumer/{}", cid), 1).await;
        assert!(until(|| async { b.queue_dispatcher("/q/fwd").await == Some(a.node_id) }).await,
            "the consumer's node claimed the queue, and B knows it");

        produce(&b, "/q/fwd/p/m1", "hello").await;
        assert_eq!(c.next().await, ("/q/fwd/p/m1".to_string(), "hello".to_string()));
        let on_b = queue(&b, "fwd").await;
        assert_eq!(on_b.dispatch().await, crate::route::Dispatch::Remote(a.node_id));
        assert!(held(&on_b).await.is_empty(), "the producer's node forwards and forgets");

        let on_a = queue(&a, "fwd").await;
        assert_eq!(held(&on_a).await.len(), 1);
        ack(&a, "/q/fwd/p/m1").await;
        assert_eq!(held(&on_a).await.len(), 1, "the known gap: an ack without an index removes nothing");
        assert!(c.quiet().await, "...but it is not delivered twice");
    }

    /// A consumer on B takes the queue from a dispatcher that hosts none.
    /// Messages A already holds go to B by one unicast each, under the delivery
    /// key with the consumer and index; B's ack reaches A and removes exactly
    /// that one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_dispatcher_hands_messages_to_a_consumer_on_another_node() {
        let (a, b) = online_pair().await;
        produce(&a, "/q/far/p/m1", "1").await;
        assert_eq!(a.queue_dispatcher("/q/far").await, Some(a.node_id), "the producer's node claimed it");

        let cid = Uuid::new_v4();
        let mut c = Consumer::watch(&b, &cid, &format!("/q/far/c/{}", cid), 1).await;
        assert!(until(|| async { a.queue_dispatcher("/q/far").await == Some(b.node_id) }).await,
            "the consumer's node took it over, and A knows it");
        assert!(until(|| async { a.kv_get(format!("/q/far/c/{}", cid).as_bytes()).await.is_some() }).await);

        // A still holds m1 and still dispatches what it holds: the next put
        // there wakes it, and both go to B, oldest first
        produce(&a, "/q/far/p/m2", "2").await;
        let first = c.next().await;
        let second = c.next().await;
        assert_eq!(first, (format!("/q/far/c/{}/1/m1", cid), "1".to_string()));
        assert_eq!(second, (format!("/q/far/c/{}/2/m2", cid), "2".to_string()));

        let on_a = queue(&a, "far").await;
        assert_eq!(held(&on_a).await, vec![1, 2]);
        ack(&b, &first.0).await;
        assert!(until(|| async { held(&on_a).await == vec![2] }).await, "B's ack removed #1 on A");
        ack(&b, &second.0).await;
        assert!(until(|| async { held(&on_a).await.is_empty() }).await);
    }

    /// With exactly one consumer, on another node, stable and with nothing
    /// outstanding, the dispatcher moves to that node and the record says
    /// `pinned`. Several consumers, or one not yet stable, and it stays.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_dispatcher_follows_a_lone_stable_consumer() {
        let (a, b) = online_pair().await;
        // the queue and its election, without a put. The registration below
        // wakes its dispatch loop, which calls pin_to_consumer as well - under
        // the same guards, so it moves the dispatcher no sooner than this test
        let (on_a, _) = a.get_or_create_queue(&PutRequest { key: b"/q/pin/p/m1".to_vec(), ..Default::default() })
            .await.unwrap();
        assert!(on_a.elect(crate::route::Claim::Producer, &a.log).await.is_local());
        on_a.queue.write().await.push_front(QueueMsg { idx: 1, key: "/q/pin/1/m1".into(), value: vec![],
            created: Instant::now(), handled_by: None, stream: None });
        // a consumer registered on B without taking the dispatcher: as if B's
        // consumer had moved there after A was elected
        let cid = Uuid::new_v4();
        produce(&a, &format!("/q/pin/c/{}", cid), &b.node_id.to_string()).await;
        let log = a.log.clone();

        on_a.pin_to_consumer(&log).await;
        assert!(on_a.dispatch().await.is_local(), "seen once is not stable yet");
        assert_eq!(on_a.pin_seen.read().await.map(|(n, _)| n), Some(b.node_id));

        *on_a.pin_seen.write().await = Some((b.node_id,
            Instant::now().checked_sub(PIN_STABLE + Duration::from_secs(1)).unwrap()));
        on_a.pin_to_consumer(&log).await;
        assert!(on_a.dispatch().await.is_local(), "a message waiting for a consumer holds it here");
        on_a.queue.write().await[0].handled_by = Some(cid);
        on_a.pin_to_consumer(&log).await;
        assert!(on_a.dispatch().await.is_local(), "an unacknowledged message holds it here");

        on_a.queue.write().await.clear();
        on_a.pin_to_consumer(&log).await;
        assert_eq!(on_a.dispatch().await, crate::route::Dispatch::Remote(b.node_id));
        let rec = crate::route::DispatchRecord::parse(
            &String::from_utf8_lossy(&a.kv_get(b"/q/pin").await.unwrap())).unwrap();
        assert_eq!((rec.node, rec.pinned), (b.node_id, true));
        assert!(until(|| async { b.queue_dispatcher("/q/pin").await == Some(b.node_id) }).await,
            "replicated to B");
        // the loop may be making the same move: let it finish before the queue
        // is set back below
        tokio::time::sleep(Duration::from_millis(300)).await;

        // a second consumer: no single right place, and the clock resets
        *on_a.dispatch.write().await = crate::route::Dispatch::Local;
        produce(&a, &format!("/q/pin/c/{}", Uuid::new_v4()), &b.node_id.to_string()).await;
        on_a.pin_to_consumer(&log).await;
        assert!(on_a.pin_seen.read().await.is_none());
        assert!(on_a.dispatch().await.is_local());
    }

    /// A lone consumer on the dispatcher's own node: nothing to move.
    #[tokio::test]
    async fn a_local_lone_consumer_pins_nothing() {
        let a = node(A).await;
        let cid = Uuid::new_v4();
        let _c = Consumer::watch(&a, &cid, &format!("/q/home/c/{}", cid), 1).await;
        let q = queue(&a, "home").await;
        *q.pin_seen.write().await = Some((12345, Instant::now()));
        q.pin_to_consumer(&a.log).await;
        assert!(q.pin_seen.read().await.is_none());
        assert!(q.dispatch().await.is_local());
    }

    /// A dispatcher that cannot be reached: the put is broadcast instead of
    /// dropped, the queue forgets its dispatcher, and the next put re-elects.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_unreachable_dispatcher_is_replaced() {
        let (a, b) = online_pair().await;
        let cid = Uuid::new_v4();
        let _c = Consumer::watch(&a, &cid, &format!("/q/lost/c/{}", cid), 1).await;
        assert!(until(|| async { b.queue_dispatcher("/q/lost").await == Some(a.node_id) }).await);
        produce(&b, "/q/lost/p/m1", "1").await;
        let on_b = queue(&b, "lost").await;
        assert_eq!(on_b.dispatch().await, crate::route::Dispatch::Remote(a.node_id));

        // A drops out of B's Online set: unicast has no target
        assert!(b.peers.read().await.demote_to_spare(a.node_id, &b.log).await);
        produce(&b, "/q/lost/p/m2", "2").await;
        assert!(on_b.dispatch().await.is_unknown(), "the dispatcher is forgotten, not kept");

        produce(&b, "/q/lost/p/m3", "3").await;
        assert_eq!(b.queue_dispatcher("/q/lost").await, Some(b.node_id), "re-elected here");
        assert!(on_b.dispatch().await.is_local());
    }

    /// A registry entry naming a node that will not take the delivery: the
    /// message stays queued and undelivered rather than counted delivered.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_node_that_refuses_a_delivery_leaves_the_message_queued() {
        let (a, b) = online_pair().await;
        let ghost = Uuid::new_v4();
        // A believes B hosts `ghost`; B has no such consumer
        produce(&a, &format!("/q/ghost/c/{}", ghost), &b.node_id.to_string()).await;
        produce(&a, "/q/ghost/p/m1", "1").await;
        let on_a = queue(&a, "ghost").await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(held(&on_a).await, vec![1]);
        assert!(on_a.queue.read().await[0].handled_by.is_none(), "not counted delivered");
        assert!(!on_a.is_reapable().await, "a registered consumer anywhere keeps the queue");
        assert!(on_a.node_hosts_consumer(b.node_id).await);
        assert!(!on_a.node_hosts_consumer(12345).await);
    }

    /// [q-registry] A consumer registered before the peers connect - the order
    /// of a cluster that starts together - is known to the peer once it
    /// connects, and a producer there routes to it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_consumer_registered_before_peering_is_known_to_the_peer() {
        let (a, b, urls) = served_pair().await;
        let cid = Uuid::new_v4();
        let reg = format!("/q/early/c/{}", cid);
        let mut c = Consumer::watch(&a, &cid, &reg, 1).await;
        assert!(b.kv_get(reg.as_bytes()).await.is_none(), "no peer to tell yet");

        peer_up(&a, &b, &urls).await;
        assert!(until(|| async { super::node_tests::registration(&b, &reg).await.map(|r| r.node) == Some(a.node_id) }).await,
            "B knows the consumer, and its host");
        assert!(until(|| async { b.queue_dispatcher("/q/early").await == Some(a.node_id) }).await,
            "and the consumer's node as the dispatcher");

        produce(&b, "/q/early/p/m1", "1").await;
        assert_eq!(c.next().await, ("/q/early/p/m1".to_string(), "1".to_string()));
        assert_eq!(queue(&b, "early").await.dispatch().await, crate::route::Dispatch::Remote(a.node_id));
    }

    /// [q-registry] [q-dead] The 93b_etcd_upsert shape. B took a message before
    /// the peers connected and holds it, knowing no consumer; the consumer was
    /// registered on A before that. Once they connect the consumer gets it -
    /// with no further put on B to wake it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_message_held_before_peering_reaches_a_consumer_registered_before_peering() {
        let (a, b, urls) = served_pair().await;
        let cid = Uuid::new_v4();
        let mut c = Consumer::watch(&a, &cid, &format!("/q/held/c/{}", cid), 1).await;
        produce(&b, "/q/held/p/m1", "1").await;
        let on_b = queue(&b, "held").await;
        assert!(on_b.dispatch().await.is_local(), "B claimed it, knowing no other node");
        assert_eq!(held(&on_b).await, vec![1]);

        peer_up(&a, &b, &urls).await;
        let got = tokio::time::timeout(Duration::from_secs(15), c.rx.recv()).await
            .expect("the held message reaches the consumer").expect("open").expect("not an error");
        let key = String::from_utf8_lossy(&got.events[0].kv.as_ref().unwrap().key).to_string();
        assert_eq!(key, format!("/q/held/c/{}/1/m1", cid));
        // and the ack reaches the node holding it
        ack(&a, &key).await;
        assert!(until(|| async { held(&on_b).await.is_empty() }).await);
    }

    /// [q-registry] A peer announcing itself - a node that restarted, and lost
    /// what it held - gets the registry again, though it was held all along and
    /// so is no new peer here.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_peer_announcing_itself_gets_the_registry_again() {
        use crate::etcdpb::etcdserverpb::{cluster_server::Cluster as _, MemberAddRequest};
        let (a, b) = online_pair().await;
        let cid = Uuid::new_v4();
        let reg = format!("/q/back/c/{}", cid);
        let _c = Consumer::watch(&a, &cid, &reg, 1).await;
        assert!(until(|| async { b.kv_get(reg.as_bytes()).await.is_some() }).await);
        tokio::time::sleep(Duration::from_millis(300)).await; // the joins of peering are written

        // B forgets it, as a restart does, and nothing writes it again by itself
        b.vault.write().await.remove(reg.as_bytes());
        b.vault.write().await.remove(&b"/q/back"[..]);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(b.kv_get(reg.as_bytes()).await.is_none());

        a.member_add(tonic::Request::new(MemberAddRequest { peer_ur_ls: vec![], is_learner: true })).await.unwrap();
        assert!(until(|| async { b.kv_get(reg.as_bytes()).await.is_some() }).await, "written to B again");
        assert_eq!(b.queue_dispatcher("/q/back").await, Some(a.node_id));
    }

    /// [q-dead] A node that restarts gets its registrations from before back
    /// from a peer (`announce_and_sync`), for consumers it no longer has. It
    /// withdraws them at start, and the peer hands out again what it held for
    /// them - not only once that node is back on its own.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_restarted_node_withdraws_what_it_held_before() {
        use crate::route::{Registration, StreamState};
        const C: &str = "77777777-7777-5777-8777-777777777773";
        let c_id = { let (x, y) = Uuid::parse_str(C).unwrap().as_u64_pair(); x ^ y };
        let (a, _b, urls) = served_pair().await;
        produce(&a, "/q/rs/p/m1", "1").await;
        let on_a = queue(&a, "rs").await;
        let cid = Uuid::new_v4();
        let reg = format!("/q/rs/c/{}", cid);
        produce(&a, &reg, &Registration::new(c_id, 5, StreamState::Draining).render()).await; // first
        on_a.queue.write().await[0].handled_by = Some(cid); // handed to C's stream 5, before C restarted
        on_a.queue.write().await[0].stream = Some((c_id, 5));
        tokio::time::sleep(REDISPATCH * 2).await;
        assert_eq!(on_a.queue.read().await[0].stream, Some((c_id, 5)), "held for it while registered");

        // C starts again, A its peer
        let mut cfg = crate::cli::EtcdConfig::with_defaults();
        cfg.cluster = "11111111-1111-5111-8111-111111111111".to_string();
        cfg.node = C.to_string();
        cfg.listen_client_urls = "127.0.0.1:0".to_string();
        cfg.initial_advertise_peer_urls = urls[0].clone();
        let c = EtcdNode::init(cfg, Logger::root(slog::Discard, slog::o!()),
            #[cfg(feature = "tracer")] None).await.unwrap();
        assert_eq!(c.node_id, c_id);
        assert!(until(|| async { a.kv_get(reg.as_bytes()).await.is_none() }).await, "withdrawn on A");
        assert!(until(|| async { on_a.queue.read().await[0].handled_by.is_none() }).await, "and handed back");
    }

    /// [q-registry] A peer held Spare gets no writes - the registry neither.
    /// Promoted, it gets the registry again, so it does not dispatch without
    /// knowing the consumers.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_peer_promoted_from_spare_gets_the_registry_again() {
        use crate::etcdpb::etcdserverpb::{cluster_server::Cluster as _, MemberPromoteRequest};
        let (a, b) = online_pair().await;
        assert!(a.peers.read().await.demote_to_spare(b.node_id, &a.log).await);
        let cid = Uuid::new_v4();
        let reg = format!("/q/spare/c/{}", cid);
        let _c = Consumer::watch(&a, &cid, &reg, 1).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(b.kv_get(reg.as_bytes()).await.is_none(), "a Spare peer gets no writes");

        a.member_promote(tonic::Request::new(MemberPromoteRequest { id: b.node_id })).await.unwrap();
        assert!(until(|| async { super::node_tests::registration(&b, &reg).await.map(|r| r.node) == Some(a.node_id) }).await,
            "promoted, it gets the registry");
    }

    /// [q-dead] A consumer on another node that stalls: its node drops it as
    /// slow and drains it well within the dispatcher's wait for the call - not
    /// cut off with that call - and what it took stays its own.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_remote_consumer_that_stalls_is_drained_on_its_node() {
        use crate::route::StreamState;
        let (a, b) = online_pair().await;
        produce(&a, "/q/rst/p/m1", "1").await; // A holds it
        let cid = Uuid::new_v4();
        let reg = format!("/q/rst/c/{}", cid);
        let (req, req_rx) = mpsc::channel(10);
        let (tx, mut rx) = mpsc::channel(2); // two messages and it is full
        b.new_watcher(req_rx, tx, &cid).await;
        req.send(WatchRequest { request_union: Some(watch_request::RequestUnion::CreateRequest(
            WatchCreateRequest { key: reg.clone().into_bytes(), watch_id: 1, ..Default::default() })) }).await.unwrap();
        assert!(rx.recv().await.unwrap().unwrap().created);
        let on_a = queue(&a, "rst").await;
        assert!(until(|| async { on_a.queue.read().await[0].handled_by == Some(cid) }).await, "m1 to B");
        produce(&a, "/q/rst/p/m2", "2").await;
        assert!(until(|| async { on_a.queue.read().await.iter().all(|m| m.handled_by == Some(cid)) }).await, "m2 to B");
        produce(&a, "/q/rst/p/m3", "3").await; // B cannot take it

        let drained = tokio::time::timeout(Duration::from_secs(4), async {
            while super::node_tests::registration(&b, &reg).await.map(|r| r.state) != Some(StreamState::Draining) {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }).await;
        assert!(drained.is_ok(), "dropped as slow and draining on B, before A gives up the call");
        // once A knows it drains, A offers it nothing more
        assert!(until(|| async { super::node_tests::registration(&a, &reg).await.map(|r| r.state) == Some(StreamState::Draining) }).await);
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let m3 = on_a.queue.read().await.iter().find(|m| m.idx == 3).map(|m| m.handled_by);
        assert_eq!(m3, Some(None), "not handed");
        assert!(on_a.queue.read().await.iter().filter(|m| m.idx < 3).all(|m| m.handled_by == Some(cid)), "what it took stays its own");
        drop((req, rx));
    }

    /// [q-dead] A delivery to another node that gets no answer may have been
    /// carried out: it is held for that stream rather than handed out again on
    /// a guess - and goes back when the stream closes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_delivery_with_no_answer_is_held_for_its_stream() {
        let (a, b) = online_pair().await;
        let cid = Uuid::new_v4();
        let reg = format!("/q/na/c/{}", cid);
        let c = Consumer::watch(&b, &cid, &reg, 1).await;
        assert!(until(|| async { super::node_tests::registration(&a, &reg).await.is_some() }).await);
        let g = super::node_tests::registration(&b, &reg).await.unwrap().generation;
        let on_a = {
            produce(&a, "/q/na/p/m0", "0").await; // A holds what A's producers put
            queue(&a, "na").await
        };
        *on_a.dispatch.write().await = crate::route::Dispatch::Local;
        on_a.queue.write().await.clear();

        let held = b.queues.write().await; // B takes no delivery now
        // as a producer elsewhere sends it: kept on A, the dispatcher
        on_a.put(QueueNameKey::new("/q/na/7/m1".to_string()),
            PutRequest { key: b"/q/na/p/m1".to_vec(), value: b"1".to_vec(), ..Default::default() },
            &Some("987654321".to_string()), &a.log).await.unwrap();
        tokio::time::sleep(crate::peer::PEER_RPC_TIMEOUT + Duration::from_millis(700)).await;
        drop(held);
        let m1 = on_a.queue.read().await.iter().find(|m| m.idx == 7).map(|m| (m.handled_by, m.stream));
        assert_eq!(m1, Some((Some(cid), Some((b.node_id, g)))), "held for that stream");

        drop(c); // the stream closes
        assert!(until(|| async { on_a.queue.read().await.iter().find(|m| m.idx == 7).map_or(false, |m| m.handled_by.is_none()) }).await,
            "and back when it closes");
    }

    /// [q-dead] A stream of this node it no longer holds, still registered -
    /// the client comes back on a new one: its tombstone is written first, so
    /// a dispatcher holding what that stream was handed hands it out again.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_stale_stream_is_withdrawn_when_its_client_comes_back() {
        use crate::route::{Registration, StreamState};
        let (a, b) = online_pair().await;
        // the republish of peering's joins is done: it would withdraw the stale
        // stream itself, which is not the path under test
        tokio::time::sleep(Duration::from_millis(500)).await;
        let cid = Uuid::new_v4();
        let reg = format!("/q/w2/c/{}", cid);
        produce(&b, "/q/w2/p/m1", "1").await; // B holds it
        let on_b = queue(&b, "w2").await;
        let old = next_gen(); // this process's, so no restart in between
        // registered first: held for a stream nobody registers, B's own check
        // would hand it back, whatever this test is about. As draining: B
        // would otherwise try to deliver to a stream that is not there
        produce(&a, &reg, &Registration::new(a.node_id, old, StreamState::Draining).render()).await;
        assert!(until(|| async { super::node_tests::registration(&b, &reg).await.map(|r| r.generation) == Some(old) }).await);
        on_b.queue.write().await[0].handled_by = Some(cid);
        on_b.queue.write().await[0].stream = Some((a.node_id, old));
        tokio::time::sleep(REDISPATCH * 2).await;
        assert_eq!(on_b.queue.read().await[0].stream, Some((a.node_id, old)), "held for it while registered");

        let _c = Consumer::watch(&a, &cid, &reg, 1).await; // the client is back on A
        assert!(until(|| async { on_b.queue.read().await[0].stream != Some((a.node_id, old)) }).await,
            "what the old stream held is handed out again");
    }

    /// [q-dead] A peer connected late still places on this node a stream it
    /// does not hold - from before a restart that could not reach that peer:
    /// this node withdraws it, and the peer hands out what it held for it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_late_peer_gets_this_node_s_old_streams_withdrawn() {
        use crate::route::{Registration, StreamState};
        let (a, b, urls) = served_pair().await;
        let cid = Uuid::new_v4();
        let reg = format!("/q/pull/c/{}", cid);
        produce(&b, "/q/pull/p/m1", "1").await;
        let on_b = queue(&b, "pull").await;
        let old = next_gen();
        produce(&b, &reg, &Registration::new(a.node_id, old, StreamState::Draining).render()).await; // first
        on_b.queue.write().await[0].handled_by = Some(cid);
        on_b.queue.write().await[0].stream = Some((a.node_id, old));
        tokio::time::sleep(REDISPATCH * 2).await;
        assert_eq!(on_b.queue.read().await[0].stream, Some((a.node_id, old)), "held for it while registered");

        peer_up(&a, &b, &urls).await;
        assert!(until(|| async { b.kv_get(reg.as_bytes()).await.is_none() }).await, "withdrawn on B");
        assert!(until(|| async { on_b.queue.read().await[0].handled_by.is_none() }).await, "and handed back");
    }

    /// [q-dead] A stream's tombstone that arrives while a delivery to it is
    /// still on the way releases it: marked before it was sent, it is not
    /// marked after, for a stream already closed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_tombstone_during_the_delivery_releases_it() {
        use crate::route::{Registration, StreamState};
        let (a, b) = online_pair().await;
        let cid = Uuid::new_v4();
        let reg = format!("/q/mid/c/{}", cid);
        let _c = Consumer::watch(&b, &cid, &reg, 1).await;
        assert!(until(|| async { super::node_tests::registration(&a, &reg).await.is_some() }).await);
        let g = super::node_tests::registration(&b, &reg).await.unwrap().generation;
        produce(&a, "/q/mid/p/m0", "0").await;
        let on_a = queue(&a, "mid").await;
        *on_a.dispatch.write().await = crate::route::Dispatch::Local;
        on_a.queue.write().await.clear();
        let from_b = |v: String| {
            let mut r = tonic::Request::new(PutRequest { key: reg.clone().into_bytes(), value: v.into_bytes(), ..Default::default() });
            r.metadata_mut().insert(crate::etcdpb::XPEER, b.node_id.to_string().parse().unwrap());
            r
        };

        let held = b.queues.write().await; // the delivery waits at B
        on_a.put(QueueNameKey::new("/q/mid/9/m1".to_string()),
            PutRequest { key: b"/q/mid/p/m1".to_vec(), value: b"1".to_vec(), ..Default::default() },
            &Some(b.node_id.to_string()), &a.log).await.unwrap();
        assert!(until(|| async { on_a.queue.read().await.iter().any(|m| m.stream == Some((b.node_id, g))) }).await,
            "marked while on the way");
        a.put(from_b(Registration::new(b.node_id, g, StreamState::Closed).render())).await.unwrap();
        assert!(until(|| async { on_a.queue.read().await.iter().all(|m| m.handled_by.is_none()) }).await, "released");
        // the client is back on B on a new stream; then the delivery returns
        a.put(from_b(Registration::new(b.node_id, next_gen(), StreamState::Receiving).render())).await.unwrap();
        drop(held);
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(on_a.queue.read().await.iter().all(|m| m.stream != Some((b.node_id, g))), "not held for the closed stream");
    }

    /// [q-registry] A peer promoted after it missed this node's withdrawals for
    /// long - past the tombstones written again on joins - has them all the
    /// same: what it still places on this node and this node no longer holds is
    /// withdrawn there.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_peer_promoted_late_misses_nothing_of_this_node_s() {
        use crate::etcdpb::etcdserverpb::{cluster_server::Cluster as _, MemberPromoteRequest};
        let (a, b) = online_pair().await;
        let cid = Uuid::new_v4();
        let reg = format!("/q/late/c/{}", cid);
        let c = Consumer::watch(&a, &cid, &reg, 1).await;
        assert!(until(|| async { b.kv_get(reg.as_bytes()).await.is_some() }).await);
        assert!(a.peers.read().await.demote_to_spare(b.node_id, &a.log).await);
        drop(c); // withdrawn on A while B is Spare: B keeps it
        let on_a = queue(&a, "late").await;
        assert!(until(|| async { a.kv_get(reg.as_bytes()).await.is_none() }).await);
        on_a.closed.write().await.clear(); // as if long ago: not written again on a join
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(b.kv_get(reg.as_bytes()).await.is_some(), "B missed it");

        a.member_promote(tonic::Request::new(MemberPromoteRequest { id: b.node_id })).await.unwrap();
        assert!(until(|| async { b.kv_get(reg.as_bytes()).await.is_none() }).await, "promoted, B has it withdrawn");
    }

    /// [q-dead] A partition heals: a peer that answers again after it was down
    /// is a join both ways - each writes its registry to the other, and a
    /// consumer withdrawn as gone meanwhile is registered again.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_peer_back_from_down_registers_its_consumers_again() {
        let (a, b) = online_pair().await;
        let cid = Uuid::new_v4();
        let reg = format!("/q/heal/c/{}", cid);
        let _c = Consumer::watch(&b, &cid, &reg, 1).await;
        assert!(until(|| async { a.kv_get(reg.as_bytes()).await.is_some() }).await);
        tokio::time::sleep(Duration::from_millis(500)).await; // peering's joins are done
        a.vault.write().await.remove(reg.as_bytes()); // withdrawn as gone while B was cut off
        super::node_tests::down_since(&a, b.node_id, Some(Instant::now())).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(a.kv_get(reg.as_bytes()).await.is_none());

        a.probe_peers().await; // B answers again
        assert!(until(|| async { a.kv_get(reg.as_bytes()).await.is_some() }).await, "B's consumer registered here again");
    }

    /// [q-dead] A node that answers here is not gone here, whoever was cut off
    /// from it: a tombstone on its behalf is not taken.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_tombstone_on_behalf_of_a_node_that_answers_is_not_taken() {
        let (a, b) = online_pair().await;
        let cid = Uuid::new_v4();
        let reg = format!("/q/near/c/{}", cid);
        let _c = Consumer::watch(&b, &cid, &reg, 1).await;
        assert!(until(|| async { a.kv_get(reg.as_bytes()).await.is_some() }).await);
        let g = super::node_tests::registration(&a, &reg).await.unwrap().generation;
        let mut r = tonic::Request::new(PutRequest { key: reg.clone().into_bytes(),
            value: crate::route::Registration::gone(b.node_id, g).render().into_bytes(), ..Default::default() });
        r.metadata_mut().insert(crate::etcdpb::XPEER, "555".parse().unwrap());
        a.put(r).await.unwrap();
        assert!(a.kv_get(reg.as_bytes()).await.is_some(), "B answers A: still registered here");
    }

    /// [q-dead] A consumer on another node that goes away holding a message:
    /// its node withdraws it, and the dispatcher hands the message to the next
    /// consumer.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_message_handed_to_a_consumer_on_a_node_that_drops_it_is_handed_again() {
        let (a, b) = online_pair().await;
        produce(&a, "/q/hop/p/m1", "1").await;
        let on_a = queue(&a, "hop").await;
        let gone = Uuid::new_v4();
        let c1 = Consumer::watch(&b, &gone, &format!("/q/hop/c/{}", gone), 1).await;
        // A dispatches what it holds to B's consumer, woken by its registration
        assert!(until(|| async { on_a.queue.read().await[0].handled_by == Some(gone) }).await, "handed to B");

        drop(c1);
        assert!(until(|| async { on_a.queue.read().await[0].handled_by.is_none() }).await,
            "B withdrew it, and A hands it out again");
        let live = Uuid::new_v4();
        let mut c2 = Consumer::watch(&a, &live, &format!("/q/hop/c/{}", live), 1).await;
        assert_eq!(c2.next().await, ("/q/hop/1/m1".to_string(), "1".to_string()));
    }
}
