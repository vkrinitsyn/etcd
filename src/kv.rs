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

/// Every key a txn touches: its comparisons and both branches.
///
/// The comparison keys count. A txn whose *condition* reads a local key is
/// still a txn about that key, and shipping it to a peer would have the peer
/// evaluate a condition against a value it must not hold.
pub(crate) fn txn_keys(r: &TxnRequest) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = r.compare.iter().map(|c| c.key.clone()).collect();
    for op in r.success.iter().chain(r.failure.iter()) {
        if let Some(req) = &op.request {
            match req {
                request_op::Request::RequestPut(p) => out.push(p.key.clone()),
                request_op::Request::RequestRange(g) => out.push(g.key.clone()),
                request_op::Request::RequestDeleteRange(d) => out.push(d.key.clone()),
                request_op::Request::RequestTxn(t) => out.extend(txn_keys(t)),
            }
        }
    }
    out
}

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
    /// [range] etcd's `range_end` semantics, which this used to ignore.
    ///
    /// * empty       - one key, exactly
    /// * `[0]`       - from `key` to the end of the keyspace (with an empty
    ///                 `key`, that is every key)
    /// * anything    - the half-open interval `[key, range_end)`
    ///
    /// A PREFIX is the third form: a client asks for `key = "/p/"` and
    /// `range_end = "/p0"` - the key with its last byte incremented - which is
    /// what `etcd-client`'s `with_prefix()` sends and what every etcd tool
    /// builds for "list what is under here".
    #[inline]
    pub(crate) fn in_range(k: &[u8], key: &[u8], range_end: &[u8]) -> bool {
        if range_end.is_empty() { return k == key; }
        if range_end == [0u8] { return k >= key; }
        k >= key && k < range_end
    }

    pub(crate) async fn get_impl(&self, request: Request<RangeRequest>) -> Result<Response<RangeResponse>, Status> {
        let r = request.into_inner();

        // [range] A request carrying a `range_end` is a RANGE, and answering it
        // with an exact-key lookup is the worst available failure: the lookup
        // misses (nothing is stored under the literal prefix), the reply is
        // `count: 0`, and a client cannot tell that from "nothing is registered
        // here". Service discovery and the queue's "consumers registered against
        // a key prefix" are both this operation, so both read as empty rather
        // than as unsupported.
        //
        // The old code special-cased only `key == [] && range_end == [0]`; that
        // is subsumed here, since `k >= []` is true for every key.
        if !r.range_end.is_empty() {
            let vault = self.vault.read().await;
            let mut hits: Vec<&crate::kv::Kv> = vault
                .iter()
                .filter(|(k, _)| Self::in_range(k, &r.key, &r.range_end))
                .map(|(_, v)| v)
                .collect();
            // The vault is a HashMap, so iteration order is arbitrary and would
            // differ between two identical calls. etcd returns a range in key
            // order and callers page through it; sorting is what makes `limit`
            // mean anything.
            hits.sort_by(|a, b| a.key.cmp(&b.key));
            let count = hits.len() as i64;
            if r.limit > 0 && hits.len() > r.limit as usize {
                hits.truncate(r.limit as usize);
            }
            let more = (hits.len() as i64) < count;
            let kvs = if r.count_only { vec![] } else { hits.into_iter().map(|v| v.into()).collect() };
            return Ok(Response::new(RangeResponse { header: None, count, kvs, more }));
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
                // [q-route] Resolve the dispatcher on first touch. This used
                // to be a read of a field nothing ever wrote, so the branch
                // below was dead and every message fell through to a broadcast.
                //
                // `Claim::Producer` takes only an UNCLAIMED queue: if a
                // consumer's node already holds it, that is the better place
                // and a producer must not pull it away.
                if from_peer.is_none() && q.dispatch().await.is_unknown() {
                    q.elect(crate::route::Claim::Producer, &self.log).await;
                }
                q.put(key, r, &from_peer, &self.log).await
            }
            Err(()) => self.put_kv(r, &from_peer).await,
        }

    }

    /// An ordinary key: store it, notify watchers, replicate if the policy says
    /// so. **Never queue traffic** — the queue path is the `Ok` arm above.
    ///
    /// Split out of `put_impl` so `Queue::elect` can write the dispatcher
    /// registry without going back through the queue-aware entry point. That is
    /// not only to break the `put_impl -> elect -> kv_put -> put_impl` cycle the
    /// compiler objects to: a registry write is **by definition** not a
    /// message, and routing it through the function that decides whether
    /// something is a message would be misleading even where it happens to
    /// work.
    pub(crate) async fn put_kv(&self, r: PutRequest, from_peer: &Option<String>)
        -> Result<Response<PutResponse>, Status>
    {
        let kv: kv::Kv = r.clone().into();
        let prev_kv = self.vault.write().await.insert(r.key.clone(), kv.clone());
        let _ = self.watch_notify.send(kv).await;
        // [prefix] A local key is never broadcast. Decided by the KEY,
        // not by `from_peer` - that comes from the client-settable
        // XPEER header, so a caller could otherwise assert peerhood and
        // have its write silently skip replication.
        if from_peer.is_none() && self.policy.read().await.propagates(&r.key) {
            let peers = self.peers.read().await;
            let scope = if self.policy.read().await.is_zone_scoped(&r.key) {
                Some(peers.my_zone().to_string())
            } else { None };
            let _ = peers.broadcast_scoped(BroadcastRequest::Kv(KvEvent::Put(
                PutRequest {
                    prev_kv: false,
                    ignore_value: true, ..r.clone()
                })), scope.as_deref()).await?;
        }
        Ok(Response::new( PutResponse {
            header: None,
            prev_kv: if from_peer.is_some() || r.ignore_value { None } else { prev_kv.map(|x| x.into()) },
        }))
    }

    /// TODO implement all variation of request
    /// TODO implement kv owner and queue consumer access 
    pub(crate) async fn delete_impl(&self, request: Request<DeleteRangeRequest>) -> Result<Response<DeleteRangeResponse>, Status> {
        let from_peer = peer(request.metadata());
        let r = request.into_inner();
        trace!(self.log, "remove request {} ", String::from_utf8_lossy(&r.key));

        // [range] A DeleteRange carrying a `range_end` deletes the RANGE. This
        // used to `remove(&r.key)` and nothing else, so a prefix delete removed
        // nothing and reported `deleted: 0` - which reads as "there was nothing
        // to delete". The broadcast below already forwards `range_end`, so peers
        // were making the same exact-key removal and staying consistent with the
        // wrong answer.
        let removed: Vec<crate::kv::Kv> = if r.range_end.is_empty() {
            self.vault.write().await.remove(&r.key).into_iter().collect()
        } else {
            let mut vault = self.vault.write().await;
            let doomed: Vec<Vec<u8>> = vault
                .keys()
                .filter(|k| Self::in_range(k, &r.key, &r.range_end))
                .cloned()
                .collect();
            doomed.iter().filter_map(|k| vault.remove(k)).collect()
        };
        let deleted = removed.len() as i64;

        let qn = QueueNameKey::new(String::from_utf8_lossy(&r.key).to_string());
        if let Some(q) = self.queues.read().await.get(&qn.queue_name) {
            let _ = q.delete(&qn, &from_peer, &self.log).await;
        }

        // [prefix] same rule as put: a local key's deletion is local too,
        // or a peer would keep a value this node has dropped.
        if from_peer.is_none() && self.policy.read().await.propagates(&r.key) {
            let peers = self.peers.read().await;
            let scope = if self.policy.read().await.is_zone_scoped(&r.key) {
                Some(peers.my_zone().to_string())
            } else { None };
            let _ = peers.broadcast_scoped(BroadcastRequest::Kv(KvEvent::Delete(
                DeleteRangeRequest {
                    prev_kv: false, range_end: r.range_end, key: r.key
                })), scope.as_deref()).await?;
        }

        // prev_kv now returns EVERY key the range took, not just the one an
        // exact-key delete would have found.
        let mut prev_kvs = Vec::new();
        if r.prev_kv {
            for x in removed {
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

        // [prefix] A txn carries many keys and is broadcast whole, so it can
        // only propagate if EVERY key it touches may. Erring toward not
        // propagating is the right direction for a subtree that exists because
        // its contents are confidential - the cost of getting it wrong the
        // other way is a secret on every node.
        //
        // A txn that MIXES local and propagating keys is a caller error: those
        // are different domains and one request cannot honour both. It is
        // logged rather than silently half-applied, because "my writes stopped
        // replicating" is otherwise a very quiet symptom.
        if from_peer.is_none() {
            let policy = self.policy.read().await;
            let keys = txn_keys(&r);
            let any_local = keys.iter().any(|k| !policy.propagates(k));
            let all_local = !keys.is_empty() && keys.iter().all(|k| !policy.propagates(k));
            if any_local && !all_local {
                slog::warn!(self.log,
                    "txn mixes local-only and replicated keys; not broadcasting. \
                     Split it: one transaction cannot be both local and cluster-wide");
            }
            drop(policy);
            if !any_local {
                let _ = self.peers.read().await.broadcast(BroadcastRequest::Kv(KvEvent::Txn(r))).await?;
            }
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

#[cfg(test)]
mod range_tests {
	//! [range] `range_end` was ignored: every range request fell through to an
	//! exact-key lookup, missed, and returned `count: 0` - which a client cannot
	//! tell from "nothing is registered under this prefix". Service discovery and
	//! the queue's consumer registry are both prefix reads, so both read as empty
	//! rather than as unsupported.
	use crate::cluster::EtcdNode;

	fn inr(k: &str, key: &str, end: &str) -> bool {
		EtcdNode::in_range(k.as_bytes(), key.as_bytes(), end.as_bytes())
	}

	#[test]
	fn an_empty_range_end_is_one_exact_key() {
		assert!(inr("/a", "/a", ""));
		assert!(!inr("/ab", "/a", ""));
		assert!(!inr("/b", "/a", ""));
	}

	#[test]
	fn a_prefix_is_key_to_key_with_the_last_byte_bumped() {
		// what etcd-client's with_prefix() sends for "/p/"
		assert!(inr("/p/a", "/p/", "/p0"));
		assert!(inr("/p/z/deep", "/p/", "/p0"));
		assert!(inr("/p/", "/p/", "/p0"));
		// outside the prefix
		assert!(!inr("/q/a", "/p/", "/p0"));
		assert!(!inr("/p0", "/p/", "/p0"), "range_end is EXCLUSIVE");
		assert!(!inr("/o/a", "/p/", "/p0"));
	}

	#[test]
	fn a_zero_byte_range_end_runs_to_the_end_of_the_keyspace() {
		assert!(inr("/a", "/a", "\0"));
		assert!(inr("/zzz", "/a", "\0"));
		assert!(!inr("/A", "/a", "\0"), "before the start key");
	}

	#[test]
	fn the_all_keys_form_still_works() {
		// empty key + [0] was the ONE case the old code handled, and it has to
		// keep working: k >= [] is true for every key
		for k in ["/a", "/z/deep", "", "\u{1}"] {
			assert!(inr(k, "", "\0"), "{:?} should be in the all-keys range", k);
		}
	}

	#[test]
	fn an_explicit_interval_is_half_open() {
		assert!(inr("/b", "/b", "/d"));
		assert!(inr("/c", "/b", "/d"));
		assert!(!inr("/d", "/b", "/d"), "the end is exclusive");
		assert!(!inr("/a", "/b", "/d"));
	}
}
