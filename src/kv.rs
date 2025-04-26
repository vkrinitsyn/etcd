use slog::trace;
use crate::etcdpb::etcdserverpb::{DeleteRangeRequest, DeleteRangeResponse, PutRequest, PutResponse, RangeRequest, RangeResponse};
use crate::etcdpb::mvccpb::KeyValue;
use tokio::time::Instant;
use tonic::{Request, Response, Status};
use crate::cluster::EtcdNode;
use crate::{kv, KvEvent};
use crate::peer::BroadcastRequest;
use crate::queue::QueueNameKey;
use crate::srv::peer;

/// Key Value
#[derive(Clone)]
pub struct Kv {
  pub key: Vec<u8>,
  pub value: Vec<u8>,
 
  pub create_revision: i64,
  pub mod_revision: i64,
  pub lease: i64,
  
  pub version: u32,
  pub created: Instant,
  pub lut: Instant,
  pub lus: Instant,
}

impl From<PutRequest> for Kv {
  fn from(value: PutRequest) -> Self {
    Kv {
     key: value.key,
     value: value.value,
     create_revision: 0,
     mod_revision: 0,
     lease: 0,
     version: 0,
     created: Instant::now(),
     lut: Instant::now(),
     lus: Instant::now(),
    }
  }
}


impl From<Kv> for KeyValue {
  fn from(value: Kv) -> Self {
   crate::etcdpb::mvccpb::KeyValue {
    key: value.key,
    create_revision: value.create_revision,
    mod_revision: value.mod_revision,
    version: value.version as i64,
    value: value.value,
    lease: value.lease,
   }
  }
}

impl From<&Kv> for KeyValue {
  fn from(value: &Kv) -> Self {
   crate::etcdpb::mvccpb::KeyValue {
    key: value.key.clone(),
    create_revision: value.create_revision,
    mod_revision: value.mod_revision,
    version: value.version as i64,
    value: value.value.clone(),
    lease: value.lease,
   }
  }
}


impl EtcdNode {

    /// TODO implement all variation
    pub(crate) async fn get_impl(&self, request: Request<RangeRequest>) -> Result<Response<RangeResponse>, Status> {
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
    pub(crate) async fn put_impl(&self, request: Request<PutRequest>) -> Result<Response<PutResponse>, Status> {
        let from_peer = crate::srv::peer(request.metadata());

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
    pub(crate) async fn delete_impl(&self, request: Request<DeleteRangeRequest>) -> Result<Response<DeleteRangeResponse>, Status> {
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


}
