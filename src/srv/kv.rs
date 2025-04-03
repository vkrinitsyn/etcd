use slog::trace;
use crate::cluster::EtcdNode;
use crate::etcdpb::etcdserverpb::kv_server::Kv;
use crate::etcdpb::etcdserverpb::{CompactionRequest, CompactionResponse, DeleteRangeRequest, DeleteRangeResponse, PutRequest, PutResponse, RangeRequest, RangeResponse, TxnRequest, TxnResponse};
use crate::peer::BroadcastRequest;
use crate::queue::QueueNameKey;
use crate::srv::{peer, UNIMPL};
use crate::{kv, KvEvent};
use tonic::{async_trait, Request, Response, Status};

#[async_trait]
impl Kv for EtcdNode {
    /// TODO implement all variation
    async fn range(&self, request: Request<RangeRequest>) -> Result<Response<RangeResponse>, Status> {
        let r = request.into_inner();

        let mut kvs = Vec::new();
        let key = QueueNameKey::new(String::from_utf8_lossy(&r.key).to_string());
        if key.is_queue() {
            if let Some(q) = self.queues.read().await.get(&key.queue_name) {
                if let Some(x) = q.get(&key).await {
                    kvs.push(x.into());
                }
            }
        } 
        if kvs.is_empty() {
            if let Some(x) = self.vault.read().await.get(&r.key) {
                kvs.push(x.into());
            }
        }
        Ok(Response::new( RangeResponse {
            header: None,
            count: kvs.len() as i64,
            kvs,
            more: false,
        }))
    }

    /// TODO implement all variation of Requests ignores
    /// 1. if from peer - do not send to any peers
    /// 2. else if queue producer - do not put to vault, send to queue dispatcher host
    /// 3. else if send to 
    async fn put(&self, request: Request<PutRequest>) -> Result<Response<PutResponse>, Status> {
        let from_peer = peer(request.metadata());

        let r = request.into_inner();
        match self.get_or_create_queue(&r).await {
            Ok((q, key)) => {
                if from_peer.is_none() { 
                    if let Some(d) = q.dispatcher().await {
                        return d.lock().await.kv_client.lock().await.put(Request::new(r), None).await;
                    }
                }
                q.put(key, r, &from_peer, &self.log).await
            }
            Err(()) => { // not a queue producer
                let kv: kv::Kv = r.clone().into();
                let prev_kv = self.vault.write().await.insert(r.key.clone(), kv.clone());
                let _ = self.watch_notify.send(kv).await;
                if from_peer.is_none() {
                    let _ = self.peers.read().await.broadcast(BroadcastRequest::Kv(KvEvent::Put(
                        PutRequest {
                            prev_kv: false,
                            ignore_value: true, ..r.clone()
                        }))).await?;
                }
                Ok(Response::new( PutResponse {
                    header: None,
                    prev_kv: if from_peer.is_some() || r.ignore_value { None } else { prev_kv.map(|x| x.into()) },
                }))
            }
            
        }
       
    }

    /// TODO implement all variation of request
    /// TODO implement kv owner and queue consumer access 
    async fn delete_range(&self, request: Request<DeleteRangeRequest>) -> Result<Response<DeleteRangeResponse>, Status> {
        let from_peer = peer(request.metadata());
        let r = request.into_inner();
        trace!(self.log, "remove request {} ", String::from_utf8_lossy(&r.key));

        let x = self.vault.write().await.remove(&r.key);
        let deleted = if x.is_some() { 1 } else { 0 };

        let qn = QueueNameKey::new(String::from_utf8_lossy(&r.key).to_string());
        if let Some(q) = self.queues.read().await.get(&qn.queue_name) {
            let _ = q.delete(&qn, &from_peer, &self.log).await;
        }

        if from_peer.is_none() {
            let _ = self.peers.read().await.broadcast(BroadcastRequest::Kv(KvEvent::Delete(
                DeleteRangeRequest {
                    prev_kv: false, range_end: r.range_end, key: r.key
                }))).await?;
        }
        
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

    async fn txn(&self, _request: Request<TxnRequest>) -> Result<Response<TxnResponse>, Status> {
        Err(Status::unimplemented(UNIMPL))
    }

    async fn compact(&self, _request: Request<CompactionRequest>) -> Result<Response<CompactionResponse>, Status> {
        Err(Status::unimplemented(UNIMPL))
    }
}
