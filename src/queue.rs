use crate::cluster::{ClientId, EtcdClientNode, EtcdNode, EtcdPeerNodeType, WatcherConsumer};
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

    sender: Sender<MsgNotifyType>,
}

type MsgNotifyType = u64;

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

        self.queue.write().await.push_front(msg);
        let _ = self.sender.send(idx).await;

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
            while let Some(r) = rsvr.recv().await {
                if let Some(x) = queue.queue.read().await.back() {
                    if x.idx > 0 && r != x.idx { continue; }
                    trace!(log, "dispatch {} [{}]", x.key, queue.queue.read().await.len());
                    // TODO queue: implement picking best consumer strategy
                    // TODO queue: implement dead queue - self send a message after a while, check timeout and remove
                    
                    if let Some((_cid, c)) = queue.clients.read().await.iter().next() {
                        if let Some((wid, w)) = c.read().await.watchers.iter().next() {
                            if let Err(_e) = w.client.send(Ok(WatchResponse {
                                header: None,
                                watch_id: *wid,
                                created: false,
                                canceled: false,
                                compact_revision: 0,
                                cancel_reason: "".to_string(),
                                fragment: false,
                                events: vec![ Event {
                                    r#type: 0, // put
                                    kv: Some(x.into()),
                                    prev_kv: None,
                                }],
                            })).await {
                                // TODO queue: remove client after few try

                            }
                        }
                    }
                    // try to send
                }
            }
        });
        self
    }


    // TODO queue: - implement queue acknowledge auth, allow only logged in clientId to cleanup only his dispatcher message
    pub(crate) async fn delete(&self, request: &QueueNameKey, _from_peer: &Option<String>, log: &Logger) {
        let q = if let Some(idx) = request.idx {
            let mut q = self.queue.write().await;
            q.retain(|v| v.idx > idx); // TODO optimize for big queue size
            q.len() as i64
        } else {-1};
        trace!(log, "removed# {:?} queue size: [{}]", request.idx, q);
        let _ = self.sender.send(0).await;
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
            return match self.queues.read().await.get(&qn.queue_name).map(|q| q.clone()) {
                None => {
                    let q = Queue::new(&self, &qn).await;
                    self.queues.write().await.insert(qn.queue_name.clone(), q.clone());
                    Ok((q, qn))
                }
                Some(q) => Ok((q, qn))
            };
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
        assert_eq!(QueueNameKey::new("/q/name/p/key".into()).queue_name, "key".to_string());
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
}