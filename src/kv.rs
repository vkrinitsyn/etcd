use std::collections::HashMap;
use slog::trace;
use crate::cluster::KvKey;
use crate::etcdpb::etcdserverpb::{Compare, DeleteRangeRequest, DeleteRangeResponse, PutRequest, PutResponse, RangeRequest, RangeResponse, ResponseOp, TxnRequest, TxnResponse};
use crate::etcdpb::etcdserverpb::compare::{CompareResult, CompareTarget, TargetUnion};
use crate::etcdpb::etcdserverpb::{request_op, response_op};
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

impl From<KeyValue> for Kv {
  fn from(kv: KeyValue) -> Self {
    Kv {
      key: kv.key,
      value: kv.value,
      create_revision: kv.create_revision,
      mod_revision: kv.mod_revision,
      lease: kv.lease,
      version: kv.version as u32,
      created: Instant::now(),
      lut: Instant::now(),
      lus: Instant::now(),
    }
  }
}


impl EtcdNode {

    /// TODO implement all variation
    pub(crate) async fn get_impl(&self, request: Request<RangeRequest>) -> Result<Response<RangeResponse>, Status> {
        let r = request.into_inner();

        // all keys: empty key + range_end == [0]
        if r.key.is_empty() && r.range_end == vec![0u8] {
            let vault = self.vault.read().await;
            let count = vault.len() as i64;
            let kvs = if r.count_only {
                vec![]
            } else {
                vault.values().map(|v| v.into()).collect()
            };
            return Ok(Response::new(RangeResponse {
                header: None,
                count,
                kvs,
                more: false,
            }));
        }

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

    pub(crate) async fn txn_impl(&self, request: Request<TxnRequest>) -> Result<Response<TxnResponse>, Status> {
        let from_peer = crate::srv::peer(request.metadata());
        let r = request.into_inner();

        let (succeeded, responses, modified) = {
            let mut vault = self.vault.write().await;
            let succeeded = r.compare.iter().all(|c| Self::evaluate_compare(c, &vault));
            let ops = if succeeded { &r.success } else { &r.failure };
            let mut responses = Vec::new();
            let mut modified = Vec::new();
            for op in ops {
                if let Some(ref req) = op.request {
                    let (resp, mods) = Self::execute_op(req, &mut vault);
                    responses.push(resp);
                    modified.extend(mods);
                }
            }
            (succeeded, responses, modified)
        };

        for kv in &modified {
            let _ = self.watch_notify.send(kv.clone()).await;
        }

        if from_peer.is_none() {
            let _ = self.peers.read().await.broadcast(BroadcastRequest::Kv(KvEvent::Txn(r))).await?;
        }

        Ok(Response::new(TxnResponse {
            header: None,
            succeeded,
            responses,
        }))
    }

    fn evaluate_compare(cmp: &Compare, vault: &HashMap<KvKey, kv::Kv>) -> bool {
        let kv = vault.get(&cmp.key);
        let result = CompareResult::try_from(cmp.result).unwrap_or(CompareResult::Equal);
        let target = CompareTarget::try_from(cmp.target).unwrap_or(CompareTarget::Version);

        match target {
            CompareTarget::Version => {
                let actual = kv.map(|k| k.version as i64).unwrap_or(0);
                let expected = match &cmp.target_union {
                    Some(TargetUnion::Version(v)) => *v,
                    _ => 0,
                };
                compare_i64(actual, expected, result)
            }
            CompareTarget::Create => {
                let actual = kv.map(|k| k.create_revision).unwrap_or(0);
                let expected = match &cmp.target_union {
                    Some(TargetUnion::CreateRevision(v)) => *v,
                    _ => 0,
                };
                compare_i64(actual, expected, result)
            }
            CompareTarget::Mod => {
                let actual = kv.map(|k| k.mod_revision).unwrap_or(0);
                let expected = match &cmp.target_union {
                    Some(TargetUnion::ModRevision(v)) => *v,
                    _ => 0,
                };
                compare_i64(actual, expected, result)
            }
            CompareTarget::Value => {
                let actual: &[u8] = kv.map(|k| k.value.as_slice()).unwrap_or(&[]);
                let expected: &[u8] = match &cmp.target_union {
                    Some(TargetUnion::Value(v)) => v.as_slice(),
                    _ => &[],
                };
                match result {
                    CompareResult::Equal => actual == expected,
                    CompareResult::NotEqual => actual != expected,
                    CompareResult::Greater => actual > expected,
                    CompareResult::Less => actual < expected,
                }
            }
            CompareTarget::Lease => {
                let actual = kv.map(|k| k.lease).unwrap_or(0);
                let expected = match &cmp.target_union {
                    Some(TargetUnion::Lease(v)) => *v,
                    _ => 0,
                };
                compare_i64(actual, expected, result)
            }
        }
    }

    fn execute_op(req: &request_op::Request, vault: &mut HashMap<KvKey, kv::Kv>) -> (ResponseOp, Vec<kv::Kv>) {
        let mut modified = Vec::new();
        let response = match req {
            request_op::Request::RequestRange(r) => {
                let kvs: Vec<KeyValue> = if r.key.is_empty() && r.range_end == vec![0u8] {
                    if r.count_only { vec![] } else { vault.values().map(|v| v.into()).collect() }
                } else if let Some(v) = vault.get(&r.key) {
                    vec![v.into()]
                } else {
                    vec![]
                };
                let count = if r.key.is_empty() && r.range_end == vec![0u8] {
                    vault.len() as i64
                } else {
                    kvs.len() as i64
                };
                ResponseOp {
                    response: Some(response_op::Response::ResponseRange(RangeResponse {
                        header: None, count, kvs, more: false,
                    })),
                }
            }
            request_op::Request::RequestPut(r) => {
                let new_kv: kv::Kv = r.clone().into();
                let prev = vault.insert(r.key.clone(), new_kv.clone());
                modified.push(new_kv);
                ResponseOp {
                    response: Some(response_op::Response::ResponsePut(PutResponse {
                        header: None,
                        prev_kv: if r.prev_kv { prev.map(|p| p.into()) } else { None },
                    })),
                }
            }
            request_op::Request::RequestDeleteRange(r) => {
                let removed = vault.remove(&r.key);
                let deleted = if removed.is_some() { 1 } else { 0 };
                let prev_kvs = if r.prev_kv {
                    removed.into_iter().map(|x| x.into()).collect()
                } else {
                    vec![]
                };
                ResponseOp {
                    response: Some(response_op::Response::ResponseDeleteRange(DeleteRangeResponse {
                        header: None, deleted, prev_kvs,
                    })),
                }
            }
            request_op::Request::RequestTxn(r) => {
                let succeeded = r.compare.iter().all(|c| Self::evaluate_compare(c, vault));
                let ops = if succeeded { &r.success } else { &r.failure };
                let mut responses = Vec::new();
                for op in ops {
                    if let Some(ref inner) = op.request {
                        let (resp, mods) = Self::execute_op(inner, vault);
                        responses.push(resp);
                        modified.extend(mods);
                    }
                }
                ResponseOp {
                    response: Some(response_op::Response::ResponseTxn(TxnResponse {
                        header: None, succeeded, responses,
                    })),
                }
            }
        };
        (response, modified)
    }

}

fn compare_i64(actual: i64, expected: i64, result: CompareResult) -> bool {
    match result {
        CompareResult::Equal => actual == expected,
        CompareResult::NotEqual => actual != expected,
        CompareResult::Greater => actual > expected,
        CompareResult::Less => actual < expected,
    }
}
