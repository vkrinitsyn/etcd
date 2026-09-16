use crate::cluster::{ClientId, EtcdClientNode, EtcdNode, EtcdPeerNodeType, WatcherConsumer, WatcherId};
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
    dispatcher: Arc<RwLock<Option< EtcdPeerNodeType>>>,

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
            warn!(log, "{}queue {} targets ClickHouse but clickhouse_url is not set", LP, self.fq_name);
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
                debug!(log, "{}clickhouse {} <- {} row(s) from {} [{} left]",
                    LP, table, rows.len(), self.fq_name, left);
            }
            Err(e) => {
                // left queued on purpose: the next put or retry delivers them
                warn!(log, "{}clickhouse insert into {} failed, {} row(s) stay queued: {}",
                    LP, table, rows.len(), e);
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
        }
    }

    pub(crate) async fn new(etcd: &EtcdNode, qn: &QueueNameKey) -> Self {
        let (sender, rsvr) = mpsc::channel(100);
        Queue {
            fq_name: format!("/{}/{}", qn.prefix, qn.queue_name),
            etcd: Arc::new(RwLock::new(etcd.clone())),
            dispatcher: Arc::new(RwLock::new(None)),
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
        let idx = qn.idx.unwrap_or(self.idx.fetch_add(1, Ordering::Relaxed));
        let msg = QueueMsg {
            idx,
            key: if from_peer.is_some() { qn.input.clone() }else{format!("{}/{}/{}", self.fq_name, idx, qn.tail)},
            value: x.value.clone(),
            created: Instant::now(),
            handled_by: None,
        };

        let depth = { let mut q = self.queue.write().await; q.push_front(msg); q.len() };
        let notified = self.sender.try_send(idx);
        debug!(_log, "{}queued #{} [{}] notify={:?} cap={}", LP, idx, depth,
            notified.as_ref().map(|_| "ok").map_err(|e| e.to_string()), self.sender.capacity());

        if self.dispatcher.read().await.is_none() {
            let r = PutRequest {
                key: qn.input.into_bytes(),
                value: x.value,
                lease: 0,
                prev_kv: false,
                ignore_value: true,
                ignore_lease: true,
            };
            let peers = self.etcd.read().await.peers.clone(); // copy smart link to peers
            let _ = peers.read().await.broadcast(BroadcastRequest::Kv(KvEvent::Put(r))).await?;
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
                    if candidates.is_empty() {
                        debug!(log, "{}no consumer yet for {} [{}]", LP, key, depth);
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
                                debug!(log, "{}dispatch {} [{}]", LP, key, depth);
                                delivered = Some(cid);
                                break;
                            }
                            Ok(Err(_)) => {
                                warn!(log, "{}consumer {} watcher {} is gone, dropping it", LP, cid, wid);
                                queue.drop_watcher(&cid, wid).await;
                            }
                            Err(_) => {
                                warn!(log, "{}consumer {} watcher {} did not accept {} within {:?}, dropping it",
                                    LP, cid, wid, key, DISPATCH_TIMEOUT);
                                queue.drop_watcher(&cid, wid).await;
                            }
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
        debug!(log, "{}ack [{}] # {:?} queue size: [{}] cap={}", LP, request.input, request.idx, q, self.sender.capacity());
        let _ = self.sender.try_send(0);
    }

    /// TODO queue: check if dispatcher is not available, then try become queue dispatcher AND set idx
    pub(crate) async fn dispatcher(&self) -> Option<EtcdPeerNodeType> {
        self.dispatcher.read().await.clone()
    }

}

impl EtcdNode {

    /// process queue message from /producer/ (no watcher notify)
    /// will create queue bucket if not exists
    /// The returned queue will call for put() local or remote
    ///
    /// if not a queue capable key, then return Err(())
    pub(crate) async fn get_or_create_queue(&self, r: &PutRequest) -> Result<(Queue, QueueNameKey), ()> {
        let key = String::from_utf8(r.key.clone()).map_err(|_|())?;

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
        info!(self.log, "{}Create watcher for client: {}, WatchID: {}, {}", LP, cid, r.watch_id, &qn.input);
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
        {
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
                            let msg = format!("Watcher#{} already on {}", r.watch_id, String::from_utf8_lossy(&w.key));
                            if let Err(e) = sender.send(Err(Status::already_exists(msg))).await
                            {
                                warn!(self.log, "{}Create watcher: {}", LP, e);
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
            warn!(self.log, "{}Create watcher: {}", LP, e);
        }
        cid
    }

    pub(crate) async fn remove_watcher(&self, r: WatchCancelRequest, cid: ClientId) {
        if let Some(c) =  self.watchers.write().await.get_mut(&cid) {
            trace!(self.log, "CancelRequest watching by client: {} of [{}] watchers", cid, c.read().await.watchers.len());
            if let Some(w) = c.write().await.watchers.remove(&r.watch_id) {
                if let Some(o) = self.observers.write().await.get_mut(&w.key) {
                    o.retain(|(o_cid, o_wid)| !(o_cid == &cid && o_wid == &r.watch_id));
                }
                debug!(self.log, "{}CancelRequest watching: {} {}", LP, cid, r.watch_id);

                if let Err(e) = w.client.send(Ok(
                    WatchResponse { watch_id: r.watch_id, canceled: true, .. Default::default() }
                )).await {
                    warn!(self.log, "{}Cancel watcher: {}", LP, e);
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