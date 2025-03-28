use crate::cli::Config;
use crate::cluster::{ClientId, EtcdClientNode, EtcdClientType, EtcdNode, NodeId, WatcherConsumer, WatcherId};
use crate::etcdpb::etcdserverpb::{DeleteRangeRequest, DeleteRangeResponse, PutRequest, PutResponse, WatchResponse};
use crate::srv::UNIMPL;
use crate::{kv, EtcdEvents, KvEvent};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::{mpsc, RwLock};
use tonic::{Response, Status};
use uuid::Uuid;
use crate::etcdpb::mvccpb::{Event, KeyValue};
use crate::kv::Kv;
use crate::peer::{BroadcastRequest, EtcdCluster};

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
    pub(crate) name: String,

    pub(crate) etcd: Arc<RwLock<EtcdNode>>,
    pub(crate) dispatcher: Arc<RwLock<Option< crate::cluster::EtcdPeerNodeType>>>,

    /// store messages /queue/{q_name}/{idx}/{key}
    queue: Arc<RwLock<VecDeque<QueueMsg>>>,
    /// store messages /queue/{q_name}/{idx}/{key}
    idx: Arc<AtomicU64>,

    // store messages /queue/{q_name}/{idx}/{key}
    // delivery: Arc<RwLock<VecDeque<QueueMsg>>>,

    /// store deliveries /queue/{q_name}/consumer/{client_id}/{idx}/{key}
    clients: Arc<RwLock<HashMap<ClientId, crate::cluster::EtcdClientType>>>,
    sender: Sender<MsgNotyfiType>,
}

type MsgNotyfiType = u64;

/// client that produce queue input  
pub struct Producer {
    client_id: ClientId,
    peer_id: NodeId,
}

/// clients that consume a queue with a queue on a client to delivery
pub struct Consumer {
    client_id: ClientId,
    /// TODO tbd?
    delivery: VecDeque<QueueMsg>,
    // notify: Sender<EtcdEvents>,
}


pub struct QueueMsg {
    /// message index (uuid?)
    idx: u64,
    /// send as vec<u8> but must be a string literal 
    key: String,
    value: Vec<u8>
}

impl From<&QueueMsg> for KeyValue {
    fn from(value: &QueueMsg) -> Self {
        todo!()
    }
}

impl Queue {
    /// store deliveries /queue/{q_name}/consumer/{client_id}
    #[inline]
    pub(crate) fn is_consumer(v: &Vec<&str>) -> bool {
        Self::is_queue(v) && v.len() > 4
            && (v[3] == "c" || v[3] == "consumer")
    }

    #[inline]
    pub(crate) fn is_queue(v: &Vec<&str>) -> bool {
         v.len() > 2 && (v[1] == "q" || v[1] == "queue")
    }

    /// /queue/{q_name}/producer/*
    pub(crate) fn get_producer_key(v: &Vec<&str>) -> Result<String, ()> {
        if Self::is_queue(v) && v.len() > 3
            && (v[3] == "p" || v[3] == "producer" || v[3] == "i" || v[3] == "input") {
            Ok(v[4].into())
        } else { Err(()) }
    }

    /// store deliveries /queue/{q_name}
    #[inline]
    pub(crate) fn queue_name(v: &Vec<&str>) -> Option<(String, String)> {
        if Self::is_queue(v) {
            Some((v[1].to_string(), v[2].to_string()))
        } else {
            None
        }
    }


    pub(crate) async fn make_consumer(&mut self, v: &Vec<&str>, client_id: Uuid, watcher_id: i64, sender: &Sender<Result<WatchResponse, Status>>) -> String {
        let consumer_key = format!("/{}/{}/{}/{}", v[1], v[2], v[3], v[4]);
        let key = consumer_key.clone().into_bytes();
        let client = sender.clone();
        let mut clients = self.clients.write().await;
        match clients.get_mut(&client_id) {
            None => {
                let mut watchers = HashMap::new();
                watchers.insert(watcher_id, WatcherConsumer { key, client} );
                clients.insert(client_id,  Arc::new(RwLock::new(EtcdClientNode { watchers } )));
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

        consumer_key
    }

    pub(crate) async fn new(etcd: &EtcdNode, prefix: String, name: String) -> Self {
        debug_assert!(prefix.trim().is_empty() && !name.trim().is_empty(), "empty queue name");
        debug_assert!(!prefix.contains("/"), "empty queue name format");
        debug_assert!(!name.contains("/"), "empty queue name format");
        let name = format!("/{}/{}", prefix, name);
        let (sender, mut rsvr) = mpsc::channel(100);
        Queue {
            name,
            etcd: Arc::new(RwLock::new(etcd.clone())),
            dispatcher: Arc::new(RwLock::new(None)),
            queue: Arc::new(Default::default()),
            idx: Arc::new(AtomicU64::new(1)),
            // delivery: Arc::new(Default::default()),
            clients: Arc::new(Default::default()),
            sender
        }.run(rsvr).await
    }

    /// EtcdNode required to notify consumers
    pub(crate) async fn put(&self, key: String, x: PutRequest) -> Result<Response<PutResponse>, Status> {
        let idx = self.idx.fetch_add(1, Ordering::Relaxed);
        let key = format!("{}/{}/{}", self.name, idx, key);
        let r = PutRequest {
            key: key.clone().into_bytes(),
            value: x.value.clone(),
            lease: 0,
            prev_kv: false,
            ignore_value: true,
            ignore_lease: true,
        };
        // create queue indexed message
        let kv: kv::Kv = r.clone().into();
        // let _ = self.etcd.read().await.vault.write().await.insert(kv.key.clone(), kv.clone());
        
        let peers = self.etcd.read().await.peers.clone(); // copy smart link to peers 
        // TODO put on a queue /{idx}/ - make a copy, in case of dispatcher node die
        let _ = peers.read().await.broadcast(BroadcastRequest::Kv(KvEvent::Put(r))).await?;
        self.queue.write().await.push_front(QueueMsg{
            idx,
            key,
            value: kv.value,
        });
        let _ = self.sender.send(idx).await;
        
        Ok(Response::new(PutResponse::default()))
    }


    /// start a thread to dispatch a queue messages
    async fn run(self, mut rsvr: Receiver<MsgNotyfiType>) -> Self {
        let queue = self.clone();
        tokio::spawn(async move {
            while let Some(r) = rsvr.recv().await {
                while let Some(x) = queue.queue.read().await.back() {
                    // TODO define consumer
                    if let Some((cid, c)) = queue.clients.read().await.iter().next() {
                        if let Some((wid, w)) = c.read().await.watchers.iter().next() {
                            if let Err(e) = w.client.send(Ok(WatchResponse {
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
                                // TODO remove client after few try
                                
                            }
                        }
                    }
                    // try to send
                }
            }
        });
        self
    }

    
    async fn delete(&self, request: DeleteRangeRequest) -> Result<Response<DeleteRangeResponse>, Status> {
        
        Err(Status::unimplemented(UNIMPL))
    }

   
}


#[cfg(test)]
pub mod test {
    use super::*;
    #[test]
    pub fn test() {
        assert_eq!(Queue::get_producer_key(&("/q/name/p/key".split("/").collect())).unwrap(), "key".to_string());
        assert_eq!(Queue::get_producer_key(&("/q/name/i/key".split("/").collect())).unwrap(), "key".to_string());
        assert_eq!(Queue::get_producer_key(&("/queue/name/p/key".split("/").collect())).unwrap(), "key".to_string());
        assert_eq!(Queue::get_producer_key(&("/queue/name/i/key".split("/").collect())).unwrap(), "key".to_string());
        assert_eq!(Queue::get_producer_key(&("/queue/name/producer/key".split("/").collect())).unwrap(), "key".to_string());
        assert_eq!(Queue::get_producer_key(&("/q/name/producer/key".split("/").collect())).unwrap(), "key".to_string());
        assert_eq!(Queue::queue_name(&("/q/name/p/key".split("/").collect())).unwrap(), ("q".to_string(), "name".to_string()));
        assert_eq!(Queue::queue_name(&("/q/name/p/key".split("/").collect())).unwrap(), ("q".to_string(), "name".to_string()));
        assert!(Queue::is_consumer(&("/q/name/consumer/client".split("/").collect())));
        assert!(Queue::is_consumer(&("/q/name/c/client".split("/").collect())));
    }
}