use crate::cluster::EtcdNode;
use crate::etcdpb::etcdserverpb::kv_server::Kv;
use crate::etcdpb::etcdserverpb::{CompactionRequest, CompactionResponse, DeleteRangeRequest, DeleteRangeResponse, PutRequest, PutResponse, RangeRequest, RangeResponse, TxnRequest, TxnResponse, WatchResponse};
use crate::srv::{is_peer, UNIMPL};
use tonic::{async_trait, Request, Response, Status};
use crate::etcdpb::mvccpb::Event;
use crate::{kv, KvEvent};
use crate::peer::BroadcastRequest;
use crate::queue::Queue;

#[async_trait]
impl Kv for EtcdNode {
    /// TODO implement all variation
    async fn range(&self, request: Request<RangeRequest>) -> Result<Response<RangeResponse>, Status> {
        let r = request.into_inner();
        
        // let key = String::from_utf8_lossy(&r.key).to_string();
        let mut kvs = Vec::new();
        if let Some(x) = self.vault.read().await.get(&r.key) {
            kvs.push(x.into());
        }

        Ok(Response::new( RangeResponse {
            header: None,
            count: kvs.len() as i64,
            kvs,
            more: false,
        }))
    }

    /// TODO implement all variation of ignores
    /// 1. if from peer - do not send to any peers
    /// 2. else if queue producer - do not put to vault, send to queue dispatcher host
    /// 3. else if send to 
    async fn put(&self, request: Request<PutRequest>) -> Result<Response<PutResponse>, Status> {
        let from_peer = is_peer(request.metadata());

        let r = request.into_inner();
        
        match self.queue(&r).await {
            Ok((q, key)) => {
                if let Some(d) = &*q.dispatcher.read().await {
                    // check if node dispatcher created a watcher to this producer queue
                    return d.lock().await.kv_client.lock().await.put(Request::new(r), None).await;
                }
                q.put(key, r).await
            }
            Err(()) => { // not a queue producer
                let kv: kv::Kv = r.clone().into();
                let prev_kv = self.vault.write().await.insert(r.key.clone(), kv.clone());
                let _ = self.watch_notify.send(kv).await;
                if !from_peer {
                    let _ = self.peers.read().await.broadcast(BroadcastRequest::Kv(KvEvent::Put(
                        PutRequest {
                            prev_kv: false,
                            ignore_value: true, ..r.clone()
                        }))).await?;
                }
                Ok(Response::new( PutResponse {
                    header: None,
                    prev_kv: if from_peer || r.ignore_value{ None } else { prev_kv.map(|x| x.into()) },
                }))
            }
            
        }
       
    }

    /// TODO implement all variation of request
    async fn delete_range(&self, request: Request<DeleteRangeRequest>) -> Result<Response<DeleteRangeResponse>, Status> {
        let r = request.into_inner();
        
        // let key = String::from_utf8_lossy(&r.key).to_string();
        // TODO FIXME - implement queue acknowledge: check key is a queue and remove as delivered
        // if it's a queue message, than it's not in vault?

        let x = self.vault.write().await.remove(&r.key);
        let deleted = if x.is_some() { 1 } else { 0 };
        let mut prev_kvs = Vec::new();
        if r.prev_kv {
            if let Some(x) = x{
                prev_kvs.push(x.into());
            }
        }
        Ok(Response::new( DeleteRangeResponse {
            header: None,
            deleted,
            prev_kvs,
        }))

    }

    async fn txn(&self, request: Request<TxnRequest>) -> Result<Response<TxnResponse>, Status> {
        Err(Status::unimplemented(UNIMPL))
    }

    async fn compact(&self, request: Request<CompactionRequest>) -> Result<Response<CompactionResponse>, Status> {
        Err(Status::unimplemented(UNIMPL))
    }
}
