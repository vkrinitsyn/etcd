use crate::cluster::{EtcdNode, WatcherId};
use crate::etcdpb::etcdserverpb::watch_server::Watch;
use crate::etcdpb::etcdserverpb::{
    WatchRequest,
    WatchResponse,
};
use std::pin::Pin;
use std::vec;
use slog::{debug, info, warn};
use tokio::sync::{mpsc};
use tokio::sync::mpsc::Receiver;
use tokio_stream::{Stream, StreamExt};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{async_trait, Request, Response, Status, Streaming};
use uuid::Uuid;
use crate::etcdpb::etcdserverpb::watch_request::RequestUnion;
use crate::etcdpb::mvccpb::{Event};
use crate::{WatchSender, LP};

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
                                info!(log, "{}watch notify: {}", LP, String::from_utf8_lossy(&kv.key));
                                if let Err(_e) = client.client.send(Ok(
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
        let (sender, receiver) = mpsc::channel::<Result<WatchResponse, Status>>(10);
        let mut stream = request.into_inner();
        let etcd = self.clone();
        tokio::spawn(async move {
            let mut cid = Uuid::new_v4(); // lets give this client a temporary ID 
            
            while let Some(item) = stream.next().await {
                match item {
                    Ok(r) => {
                        cid = etcd.handle_watch_request(r.request_union, &sender, &cid).await;
                    }
                    Err(e) => {
                        warn!(etcd.log, "watcher [{}]: {}", cid, e.message());
                    }
                }
            }
            info!(etcd.log, "{}end watching: {}", LP, cid);
        });

        let output_stream = ReceiverStream::new(receiver);
        Ok(Response::new(Box::pin(output_stream) as Self::WatchStream))
    }
}

impl EtcdNode {
    /// use as library watcher entry point
    pub async fn new_watcher(&self, mut stream: Receiver<WatchRequest>, sender: WatchSender, client_id: &Uuid) {
        let etcd = self.clone();
        let mut cid = client_id.clone();

        tokio::spawn(async move {
            while let Some(r) = stream.recv().await {
                cid = etcd.handle_watch_request(r.request_union, &sender, &cid).await;
            }
            info!(etcd.log, "{}end watching: {}", LP, cid);
        });
    }

    async fn handle_watch_request(&self, request: Option<RequestUnion>, sender: &WatchSender, cid: &Uuid) -> Uuid {
        if let Some(rx) = request {
            match rx {
                RequestUnion::CreateRequest(r) => {
                    return self.create_watcher(r, cid.clone(), sender.clone()).await;
                }
                RequestUnion::CancelRequest(r) => {
                    let _ = self.remove_watcher(r, cid.clone()).await;
                }
                RequestUnion::ProgressRequest(_r) => {
                    let watchers = self.list_watchers(cid).await;
                    debug!(self.log, "{}Progress watcher: {}", LP, cid);
                    if let Err(e) = sender.send(Ok(
                            WatchResponse {
                                watch_id: *watchers.get(0).unwrap_or(&0),
                                fragment: watchers.len() > 1, ..Default::default()
                            }
                        )
                    ).await {
                        warn!(self.log, "{}Progress watcher: {}", LP, e);
                    }
                }
            }
        }
        cid.clone()
    }
    
    async fn list_watchers(&self, cid: &Uuid) -> Vec<WatcherId> {
        match self.watchers.read().await.get(cid) {
            None => vec![],
            Some(w) => w.read().await.watchers.keys().map(|e| *e).collect()
        }
    }
}
