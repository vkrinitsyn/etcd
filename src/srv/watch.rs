use crate::cluster::{EtcdNode};
use crate::etcdpb::etcdserverpb::watch_server::Watch;
use crate::etcdpb::etcdserverpb::{
    WatchRequest,
    WatchResponse,
};
use std::pin::Pin;
use std::vec;
use slog::{info, warn};
use tokio::sync::{mpsc};
use tokio::sync::mpsc::{Receiver};
use tokio_stream::{Stream, StreamExt};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{async_trait, Request, Response, Status, Streaming};
use uuid::Uuid;
use crate::etcdpb::etcdserverpb::watch_request::RequestUnion;
use crate::etcdpb::mvccpb::{Event};

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
                                info!(log, "watch notify: {}", String::from_utf8_lossy(&kv.key));
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
                        if let Some(rx) = r.request_union {
                            match rx {
                                RequestUnion::CreateRequest(r) => {
                                    cid = etcd.create_watcher(r, cid, sender.clone()).await;
                                }
                                RequestUnion::CancelRequest(r) => {
                                    let _ = etcd.remove_watcher(r, cid).await;
                                }
                                RequestUnion::ProgressRequest(_r) => {
                                    info!(etcd.log, "Progress watcher: {}", cid);
                                    if let Err(e) = sender.send(Result::Err(Status::ok("progress"))).await {
                                        warn!(etcd.log, "Progress watcher: {}", e);
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        warn!(etcd.log, "watcher [{}]: {}", cid, e.message());
                    }
                }
            }
            info!(etcd.log, "end watching: {}", cid);
        });

        let output_stream = ReceiverStream::new(receiver);
        Ok(Response::new(Box::pin(output_stream) as Self::WatchStream))
    }
}
