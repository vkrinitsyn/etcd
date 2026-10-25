use crate::cluster::{ClientId, EtcdClientNode, EtcdNode, WatcherConsumer, WatcherId};
use crate::etcdpb::etcdserverpb::{PutRequest, PutResponse, WatchCancelRequest, WatchCreateRequest, WatchResponse};
use crate::{KvEvent, LP};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use slog::{debug, info, trace, warn, Logger};
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::{mpsc, RwLock};
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
const QUEUE_IDLE: std::time::Duration = std::time::Duration::from_secs(600);

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
        let mut clients = self.clients.write().await;
        match clients.get_mut(&client_id) {
            None => {
                let mut watchers = HashMap::new();
                watchers.insert(watcher_id, WatcherConsumer { key, client} );
                clients.insert(client_id,  Arc::new(RwLock::new(EtcdClientNode { client_id, watchers } )));
            }
            Some(c) => {
                let mut watchers = c.write().await;
                match watchers.watchers.get_mut(&watcher_id) {
                    None => {
                        watchers.watchers.insert(watcher_id, WatcherConsumer { key, client});
                    }
                    Some(w) => {
                        w.key = key;
                        w.client = client;
                    }
                }
            }
        }
        drop(clients);
        self.touched().await;

        // [q-route Phase 2] Record which node hosts this consumer, so the
        // dispatcher can route to it instead of broadcasting and hoping.
        // Replicated like any key, so every node can resolve it.
        //
        // BEFORE the claim below, and the order matters: the claim asks whether
        // the current dispatcher hosts a consumer, and it has to be able to see
        // this one.
        self.publish_consumer(&client_id).await;

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
    pub(crate) async fn drop_watcher(&self, client_id: &ClientId, watcher_id: WatcherId) {
        let mut clients = self.clients.write().await;
        let empty = match clients.get(client_id) {
            None => false,
            Some(c) => {
                let mut c = c.write().await;
                c.watchers.remove(&watcher_id);
                c.watchers.is_empty()
            }
        };
        if empty {
            clients.remove(client_id);
            // [q-route Phase 2] The last watcher for this client is gone, so
            // this node no longer hosts it. Withdrawing is what stops the
            // dispatcher unicasting into a node with nobody listening - which
            // would look exactly like a slow consumer.
            drop(clients);
            self.withdraw_consumer(client_id).await;
        }
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
            idx: Arc::new(AtomicU64::new(1)),
            // delivery: Arc::new(Default::default()),
            clients: Arc::new(Default::default()),
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
                let ok = self.deliver_local(&cid, kv, _log).await;
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
        let idx = qn.idx.unwrap_or(self.idx.fetch_add(1, Ordering::Relaxed));
        let msg = QueueMsg {
            idx,
            key: if from_peer.is_some() { qn.input.clone() }else{format!("{}/{}/{}", self.fq_name, idx, qn.tail)},
            value: x.value.clone(),
            created: Instant::now(),
            handled_by: None,
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

            while rsvr.recv().await.is_some() {
                // A ch: queue consumes itself: take everything pending, write it
                // as one JSONEachRow batch, and drop the messages that landed.
                // Nothing else in this loop applies - there is no watcher to pick.
                #[cfg(feature = "clickhouse")]
                if let Some(table) = &ch_table {
                    queue.drain_to_clickhouse(table, &log).await;
                    continue;
                }

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
                    let candidates: Vec<(ClientId, WatcherId, Sender<Result<WatchResponse, Status>>)> = {
                        let clients = queue.clients.read().await;
                        let mut v = Vec::new();
                        for (cid, c) in clients.iter() {
                            for (wid, w) in c.read().await.watchers.iter() {
                                v.push((*cid, *wid, w.client.clone()));
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
                    let mut delivered = None;
                    for (cid, wid, client) in candidates {
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
                        match tokio::time::timeout(DISPATCH_TIMEOUT, client.send(Ok(resp))).await {
                            Ok(Ok(())) => {
                                debug!(log, "{}{}", LP, tr!("queue.dispatched", key = key, depth = depth));
                                delivered = Some(cid);
                                break;
                            }
                            Ok(Err(_)) => {
                                warn!(log, "{}{}", LP, tr!("queue.watcher_gone", consumer = cid, watch = wid));
                                queue.drop_watcher(&cid, wid).await;
                            }
                            Err(_) => {
                                warn!(log, "{}{}", LP, tr!("queue.watcher_slow", consumer = cid, watch = wid, key = key,
                                    timeout = format!("{:?}", DISPATCH_TIMEOUT)));
                                queue.drop_watcher(&cid, wid).await;
                            }
                        }
                    }
                    // [q-route Phase 2] Nothing local took it; offer it to a
                    // consumer on another node. One unicast, to the node the
                    // registry names - not a broadcast to everyone in the hope
                    // that whoever hosts it notices.
                    if delivered.is_none() {
                        for (cid, node) in remote {
                            let etcd = queue.etcd.read().await.clone();
                            let dkey = format!("{}/c/{}/{}/{}",
                                queue.fq_name, cid, idx, key.rsplit('/').next().unwrap_or(""));
                            let zone = {
                                let policy = etcd.policy.read().await;
                                if policy.is_zone_scoped(dkey.as_bytes()) {
                                    Some(etcd.peers.read().await.my_zone().to_string())
                                } else { None }
                            };
                            let req = PutRequest {
                                key: dkey.into_bytes(),
                                value: kv.value.clone(),
                                lease: 0, prev_kv: false, ignore_value: true, ignore_lease: true,
                            };
                            // [lockup] never under `peers.read()`
                            let sent = etcd
                                .unicast(BroadcastRequest::Kv(KvEvent::Put(req)), node,
                                    zone.as_deref()).await;
                            if sent {
                                debug!(log, "{}[q-route] {}", LP, tr!("queue.dispatched_remote", key = key, consumer = cid, node = node));
                                delivered = Some(cid);
                                break;
                            }
                            warn!(log, "{}[q-route] {}", LP, tr!("queue.remote_refused", node = node, key = key, consumer = cid));
                        }
                    }

                    let Some(cid) = delivered else {
                        // no consumer took it; leave it queued for the next one
                        break;
                    };
                    // Mark it so the next wake moves on instead of re-sending the
                    // same message; the consumer's acknowledge (delete) is what
                    // drops it for good.
                    // TODO queue: implement dead queue - redeliver a message that
                    // was handed over but never acknowledged within a timeout
                    let mut q = queue.queue.write().await;
                    if let Some(m) = q.iter_mut().find(|m| m.idx == idx) {
                        m.handled_by = Some(cid);
                    }
                }

                // [q-route Phase 3] With the queue drained, consider moving the
                // dispatcher onto its consumer's node. Here rather than on a
                // timer because this is the one moment the guard "nothing
                // outstanding" is most likely to hold, and because the
                // dispatcher is the only node that can evaluate it.
                queue.pin_to_consumer(&log).await;
            }
        });
        self
    }


    // TODO queue: - implement queue acknowledge auth, allow only logged in clientId to cleanup only his dispatcher message
    pub(crate) async fn delete(&self, request: &QueueNameKey, _from_peer: &Option<String>, log: &Logger) {
        let q = if let Some(idx) = request.idx {
            let mut q = self.queue.write().await;
            // Remove ONLY the acknowledged message. `> idx` treated every ack as
            // cumulative, but a consumer acknowledges one specific key, and they
            // come back out of order (a burst acks e.g. 2,3,1,8,9,7,...). Acking
            // a high idx therefore silently discarded every lower message still
            // waiting to be dispatched - a steady, load-dependent message loss.
            q.retain(|v| v.idx != idx); // TODO optimize for big queue size
            q.len() as i64
        } else {-1};
        debug!(log, "{}{}", LP, tr!("queue.ack", key = request.input, idx = format!("{:?}", request.idx), size = q, cap = self.sender.capacity()));
        let _ = self.sender.try_send(0);
    }

    // ─── [q-route Phase 2] the consumer -> host registry ───────────────────

    /// Record that this node hosts `client`.
    async fn publish_consumer(&self, client: &ClientId) {
        let etcd = self.etcd.read().await.clone();
        let me = etcd.peers.read().await.me();
        let key = crate::route::consumer_key(&self.fq_name, client);
        if let Err(e) = put_control(&etcd, &key, &me.to_string()).await {
            warn!(etcd.log, "{}[q-route] {}", LP, tr!("queue.publish_failed", key = key, err = e));
        }
    }

    /// Withdraw it.
    async fn withdraw_consumer(&self, client: &ClientId) {
        let etcd = self.etcd.read().await.clone();
        let key = crate::route::consumer_key(&self.fq_name, client);
        if let Err(e) = etcd.kv_delete(key.clone().into_bytes()).await {
            warn!(etcd.log, "{}[q-route] {}", LP, tr!("queue.withdraw_failed", key = key, err = e));
        }
    }

    /// Consumers this queue has anywhere in the cluster, and where they live.
    ///
    /// Local ones are excluded: the dispatch loop already has their channels
    /// and delivering to them costs nothing, so including them here would only
    /// offer the network as an alternative to a local send.
    async fn remote_consumers(&self) -> Vec<(ClientId, crate::cluster::NodeId)> {
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
            let Ok(node) = String::from_utf8_lossy(&v).trim().parse::<crate::cluster::NodeId>()
                else { continue };
            if node == me {
                // Our own registration, left behind by a watcher that went away
                // without withdrawing. Routing to ourselves over the network
                // would be a loop.
                continue;
            }
            out.push((cid, node));
        }
        out.sort_by_key(|(_, n)| *n);
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
            if String::from_utf8_lossy(&v).trim().parse::<crate::cluster::NodeId>() == Ok(node) {
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
    async fn deliver_local(&self, client: &ClientId, kv: KeyValue, log: &Logger) -> bool {
        let watchers: Vec<(WatcherId, Sender<Result<WatchResponse, Status>>)> = {
            let clients = self.clients.read().await;
            match clients.get(client) {
                None => Vec::new(),
                Some(c) => c.read().await.watchers.iter()
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
            match tokio::time::timeout(DISPATCH_TIMEOUT, ch.send(Ok(resp))).await {
                Ok(Ok(())) => return true,
                Ok(Err(_)) | Err(_) => {
                    warn!(log, "{}[q-route] {}", LP, tr!("queue.local_refused", consumer = client, watch = wid));
                    self.drop_watcher(client, wid).await;
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
            *self.dispatch.write().await = d;
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
        *self.dispatch.write().await = d;
        d
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

        // Nothing dispatched-but-unacked.
        if self.queue.read().await.iter().any(|m| m.handled_by.is_some()) {
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
}

/// [q-route] Write a registry key through the plain KV path.
///
/// Deliberately **not** `EtcdNode::kv_put`: that goes through `put_impl`, which
/// is the function deciding whether a key is a message - and `elect` is called
/// from inside it. Beyond the borrow-checker's objection to the cycle, a
/// registry write is by definition not queue traffic and should not be asking
/// that question at all.
async fn put_control(etcd: &EtcdNode, key: &str, value: &str) -> Result<(), Status> {
    etcd.put_kv(PutRequest {
        key: key.as_bytes().to_vec(),
        value: value.as_bytes().to_vec(),
        lease: 0, prev_kv: false, ignore_value: false, ignore_lease: false,
    }, &None).await.map(|_| ())
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
        {
            let c = WatcherConsumer { key: r.key.clone(), client: sender.clone() };
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
                            let msg = tr!("queue.watch_exists", watch = r.watch_id, key = String::from_utf8_lossy(&w.key));
                            if let Err(e) = sender.send(Err(Status::already_exists(msg))).await
                            {
                                warn!(self.log, "{}{}", LP, tr!("queue.watch_send_failed", err = e));
                            }
                        }
                    }
                }
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
        cid
    }

    pub(crate) async fn remove_watcher(&self, r: WatchCancelRequest, cid: ClientId) {
        if let Some(c) =  self.watchers.write().await.get_mut(&cid) {
            trace!(self.log, "{}{}", LP, tr!("queue.cancel_request", client = cid, count = c.read().await.watchers.len()));
            if let Some(w) = c.write().await.watchers.remove(&r.watch_id) {
                let empty = {
                    let mut obs = self.observers.write().await;
                    if let Some(o) = obs.get_mut(&w.key) {
                        o.retain(|(o_cid, o_wid)| !(o_cid == &cid && o_wid == &r.watch_id));
                        o.is_empty()
                    } else { false }
                };
                // [range] ...and drop the range with the last watcher that used
                // it, or every cancelled prefix watch leaves a scan entry behind.
                if empty {
                    self.observers.write().await.remove(&w.key);
                    self.observer_ranges.write().await.remove(&w.key);
                }
                debug!(self.log, "{}{}", LP, tr!("queue.cancelled", client = cid, watch = r.watch_id));

                // A cancelled consumer watch is no longer a consumer. Its stream
                // stays open, so without this the dispatcher kept handing it
                // messages - marked delivered, so no other consumer got them.
                let qn = QueueNameKey::new(String::from_utf8_lossy(&w.key).to_string());
                if qn.consumer {
                    let q = self.queues.read().await.get(&qn.queue_name).cloned();
                    if let Some(q) = q {
                        q.drop_watcher(&cid, r.watch_id).await;
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

}

#[cfg(test)]
pub mod test {
    use super::*;
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
                   created: Instant::now(), handled_by: None }
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

    /// Regression: acknowledges come back out of order, so an ack must remove
    /// only its own message. Removing everything below it discarded messages
    /// that had not been dispatched yet.
    #[test]
    pub fn acknowledge_removes_only_its_own_message() {
        let mut q: VecDeque<QueueMsg> = VecDeque::new();
        for i in 1..=5 { q.push_front(msg(i)); }
        // consumer finishes #4 first and acknowledges it
        let idx = 4;
        q.retain(|v| v.idx != idx);
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
        // Queue::delete does exactly this retain on acknowledge of idx 1
        q.retain(|v| v.idx != 1);
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

    pub(crate) async fn queue(n: &EtcdNode, name: &str) -> Queue {
        n.queues.read().await.get(name).cloned().expect("the queue exists")
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
        assert_eq!(n.kv_get(format!("/q/reg/c/{}", cid).as_bytes()).await,
            Some(n.node_id.to_string().into_bytes()), "the consumer's host");
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
        q.drop_watcher(&cid, 1).await;
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
    use crate::{EtcdEvents, EtcdMgmtEvent};
    use std::time::Duration;

    const A: &str = "77777777-7777-5777-8777-777777777771";
    const B: &str = "77777777-7777-5777-8777-777777777772";

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }

    /// Two served nodes that know each other, each holding the other Online.
    async fn online_pair() -> (EtcdNode, EtcdNode) {
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
        // what an embedder sends once it knows the roster
        for (i, n) in nodes.iter().enumerate() {
            let mut c = crate::cli::EtcdConfig::default();
            c.listen_client_urls = urls[i].clone();
            c.initial_advertise_peer_urls = urls.join(",");
            n.event.send(EtcdEvents::Mgmt(EtcdMgmtEvent::Config(c))).await.unwrap();
        }
        for n in &nodes {
            assert!(until(|| async { n.peers.read().await.connected() == 1 }).await, "peered");
        }
        // one replicated write each way proves the peer, which is what Online means
        let (a, b) = (nodes[0].clone(), nodes[1].clone());
        a.kv_put(b"/warm/a".to_vec(), b"1".to_vec()).await.unwrap();
        b.kv_put(b"/warm/b".to_vec(), b"1".to_vec()).await.unwrap();
        assert!(until(|| async { a.peers.read().await.is_online(b.node_id).await }).await);
        assert!(until(|| async { b.peers.read().await.is_online(a.node_id).await }).await);
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
        // the queue and its election, without a put: nothing wakes its dispatch
        // loop, so only this test moves the dispatcher
        let (on_a, _) = a.get_or_create_queue(&PutRequest { key: b"/q/pin/p/m1".to_vec(), ..Default::default() })
            .await.unwrap();
        assert!(on_a.elect(crate::route::Claim::Producer, &a.log).await.is_local());
        on_a.queue.write().await.push_front(QueueMsg { idx: 1, key: "/q/pin/1/m1".into(), value: vec![],
            created: Instant::now(), handled_by: None });
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
}
