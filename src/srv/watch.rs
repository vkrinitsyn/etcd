use std::collections::HashMap;
use crate::cluster::{WatcherConsumer, EtcdClientNode, EtcdNode, EtcdObserverType, KvKey};
use crate::etcdpb::etcdserverpb::watch_server::Watch;
use crate::etcdpb::etcdserverpb::{
    WatchRequest,
    WatchResponse,
};
use std::pin::Pin;
use std::sync::Arc;
use std::vec;
use slog::warn;
use tokio::sync::{mpsc, RwLock};
use tokio::sync::mpsc::{Receiver};
use tokio_stream::{Stream, StreamExt};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{async_trait, Request, Response, Status, Streaming};
use uuid::Uuid;
use crate::etcdpb::etcdserverpb::watch_request::RequestUnion;
use crate::etcdpb::mvccpb::{Event};
use crate::queue::Queue;

type WatchResult<T> = Result<Response<T>, Status>;
type ResponseStream = Pin<Box<dyn Stream<Item = Result<WatchResponse, Status>> + Send>>;

impl EtcdNode {
    pub(crate) async fn watch_notify(&self, mut receiver: Receiver<crate::kv::Kv>) {
        let observers = self.observers.clone();
        let watchers = self.watchers.clone();
        let log = self.log.clone();
        tokio::spawn(async move {
            while let Some(kv) = receiver.recv().await {
                if let Some(v) = observers.read().await.get(&kv.key).map(|v| v.clone()) {
                    for (cid, wid) in v { // expected watcher per client
                        if let Some(client_watcher) = watchers.read().await.get(&cid) {
                            if let Some(client) = client_watcher.read().await.watchers.get(&wid) {
                                if let Err(e) = client.client.send(Ok(
                                    WatchResponse {
                                        header: None,
                                        watch_id: wid,
                                        created: false,
                                        canceled: false,
                                        compact_revision: 0,
                                        cancel_reason: "".to_string(),
                                        fragment: false,
                                        events: vec![ Event {
                                            r#type: 0, // put
                                            kv: Some(kv.clone().into()),
                                            prev_kv: None,
                                        }],
                                    })).await {
                                    //
                                    
                                }
                            }
                        }
                    }
                }
            }
        });
    }
}

#[async_trait]
impl Watch for EtcdNode {
    type WatchStream = ResponseStream;

    async fn watch(&self, request: Request<Streaming<WatchRequest>>) -> WatchResult<ResponseStream> {
        // request
        let (sender, mut receiver) = mpsc::channel::<Result<WatchResponse, Status>>(10);
        let mut stream = request.into_inner();
        let etcd = self.clone();
        tokio::spawn(async move {
            // TODO call this to get client ID
            let mut cid = Uuid::new_v4(); // lets give this client an ID 
            
            while let Some(item) = stream.next().await {
                match item {
                    Ok(r) => {
                        if let Some(rx) = r.request_union {
                            match rx {
                                // TODO implement filters etc
                                // TODO get value from a consumer creation key
                                RequestUnion::CreateRequest(r) => {
                                    let mut key = String::from_utf8_lossy(&r.key).to_string();
                                    if key.starts_with("/q") {
                                        let v: Vec<&str> = key.split("/").collect();
                                        if Queue::is_consumer(&v) {
                                            let q_name = v[2].to_string();
                                            let mut queue_map = etcd.queues.write().await;
                                            key = match queue_map.get_mut(&q_name) {
                                                None => { // no queue exists 
                                                    let mut queue = Queue::new(&etcd, v[1].into(), v[2].into()).await;
                                                    let queue_consumer_key = queue.make_consumer(&v, cid, r.watch_id, &sender).await;
                                                    queue_map.insert(q_name, queue);
                                                    queue_consumer_key
                                                }
                                                Some(queue) => queue.make_consumer(&v, cid, r.watch_id, &sender).await
                                            };
                                        }
                                    }
                                    let key = key.into_bytes();
                                    {
                                        let mut o = etcd.observers.write().await;
                                        match o.get_mut(&key) {
                                            None => {
                                                o.insert(key.clone(), vec![(cid, r.watch_id)]);
                                            }
                                            Some(v) => {
                                                v.push((cid, r.watch_id));
                                            }
                                        }
                                    }
                                    {
                                        let mut watcher = etcd.watchers.write().await;
                                        match watcher.get_mut(&cid) {
                                            None => {
                                                let mut watcher_client_writer = EtcdClientNode {
                                                    watchers: HashMap::new(),
                                                };
                                                watcher_client_writer.watchers.insert(r.watch_id, WatcherConsumer {
                                                    key,
                                                    client: sender.clone(),
                                                });
                                                watcher.insert(cid, Arc::new(RwLock::new(watcher_client_writer)));
                                            }
                                            Some(watcher_client) => {
                                                let mut watcher_client_writer = watcher_client.write().await;
                                                match watcher_client_writer.watchers.get_mut(&r.watch_id) {
                                                    None => { // create 
                                                        watcher_client_writer.watchers.insert(r.watch_id, WatcherConsumer {
                                                            key,
                                                            client: sender.clone(),
                                                        });
                                                    }
                                                    Some(_) => {
                                                        if let Err(e) = sender.send(Result::Err(Status::already_exists("watcher"))).await {
                                                            warn!(etcd.log, "Create watcher: {}", e);
                                                            continue;
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }

                                    if let Err(e) = sender.send(Result::Ok(
                                        WatchResponse {
                                            watch_id: r.watch_id,
                                            created: true, .. Default::default()
                                        }
                                    )).await {
                                        warn!(etcd.log, "Create watcher: {}", e);
                                    }
                                }
                                RequestUnion::CancelRequest(r) => {
                                    let mut canceled = false;

                                    if let Some(mut c) =  etcd.watchers.write().await.get_mut(&cid) {
                                        if let Some(w) = c.write().await.watchers.remove(&r.watch_id) {
                                            // let v: Vec<&str> = w.key.split("/").collect();
                                            // TODO cleanup consumer
                                            
                                            canceled = true;
                                        }
                                    }
                                    
                                    if let Err(e) = sender.send(Result::Ok(
                                        WatchResponse {
                                            watch_id: r.watch_id,
                                            canceled, .. Default::default()
                                        }
                                    )).await {
                                        warn!(etcd.log, "Cancel watcher: {}", e);
                                    }
                                }
                                RequestUnion::ProgressRequest(r) => {
                                    if let Err(e) = sender.send(Result::Err(Status::ok("progress"))).await {
                                        warn!(etcd.log, "Progress watcher: {}", e);
                                        // break;
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        warn!(etcd.log, "watcher: {}", e);
                        break;
                    }
                }
            }
        });

        let output_stream = ReceiverStream::new(receiver);
        Ok(Response::new(Box::pin(output_stream) as Self::WatchStream))
    }
}
