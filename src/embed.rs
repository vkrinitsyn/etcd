//! [embed] Leases and locks for the embedder, in process (owner, 2026-11-21; YT Cluster's
//! `yt_advisory_lock`): what the Lease and Lock services do, as a call of this node's own -
//! sent on to the leader as a peer's call is, with the cluster's `peer_token`, and none of a
//! client's checks: the embedder is this node's own process. The sequences' in-process calls
//! are `seq_next`, `seq_take` and `seq_define` (`sys.rs`).
use std::time::Duration;

use tonic::Status;

use crate::cluster::EtcdNode;
use crate::coord::XFWD;
use crate::etcdpb::etcdserverpb::lease_client::LeaseClient;
use crate::etcdpb::etcdserverpb::lease_server::Lease;
use crate::etcdpb::etcdserverpb::{LeaseGrantRequest, LeaseKeepAliveRequest, LeaseRevokeRequest};
use crate::etcdpb::v3lockpb::lock_server::Lock;
use crate::etcdpb::v3lockpb::{LockRequest, UnlockRequest};

/// [embed] a Lock call that is answered at once: granted, or refused `aborted` while the lock
/// is held or waited for - none of etcd's API, a header (the proto files never change)
pub const XTRY: &str = "x-etcds-try";

/// [embed] the key a lock of `name` is held by under `lease`: what [`EtcdNode::embed_lock`]
/// answers, and what [`EtcdNode::embed_unlock`] frees
pub fn lock_holder_key(name: &[u8], lease: i64) -> Vec<u8> {
    crate::coord::lock_key(name, lease)
}

impl EtcdNode {
    /// [embed] A lease of `ttl` seconds - at least the minimum - granted on the leader: its id
    pub async fn embed_lease_grant(&self, ttl: i64) -> Result<i64, Status> {
        let r = self.peer_call(LeaseGrantRequest { ttl, id: 0 }, "", None).await;
        Ok(<Self as Lease>::lease_grant(self, r).await?.into_inner().id)
    }

    /// [embed] Lease `id` renewed on the leader: the TTL it was granted, 0 for a lease gone
    pub async fn embed_lease_keep(&self, id: i64) -> Result<i64, Status> {
        let Some(ch) = self.route_to_leader(false).await? else {
            return Ok(self.renew_lease_here(id).await);
        };
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let _ = tx.send(LeaseKeepAliveRequest { id }).await;
        let call = self.peer_call(tokio_stream::wrappers::ReceiverStream::new(rx), XFWD, None).await;
        let ask = async {
            let mut answers = LeaseClient::new(ch).lease_keep_alive(call).await?.into_inner();
            answers.message().await
        };
        let got = tokio::time::timeout(Duration::from_secs(5), ask).await
            .map_err(|_| Status::unavailable(tr!("sys.no_answer", timeout = "5s")))??;
        drop(tx);
        Ok(got.map(|a| a.ttl).unwrap_or(0))
    }

    /// [embed] Lease `id` ended on the leader, its keys deleted and its locks freed; a lease
    /// gone already is no error
    pub async fn embed_lease_revoke(&self, id: i64) -> Result<(), Status> {
        let r = self.peer_call(LeaseRevokeRequest { id }, "", None).await;
        match <Self as Lease>::lease_revoke(self, r).await {
            Ok(_) => Ok(()),
            Err(e) if e.code() == tonic::Code::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// [embed] The lock `name` under `lease`, on the leader: its holder key once it is this
    /// caller's - waiting in the queue - or with `try_only` at once or `None`. The same
    /// lease locking the same name again is the one acquisition (etcd's rule): an embedder
    /// whose callers share a lease keeps them apart itself.
    pub async fn embed_lock(&self, name: &[u8], lease: i64, try_only: bool) -> Result<Option<Vec<u8>>, Status> {
        let mut r = self.peer_call(LockRequest { name: name.to_vec(), lease }, "", None).await;
        if try_only {
            r.metadata_mut().insert(XTRY, tonic::metadata::MetadataValue::from_static("1"));
        }
        match <Self as Lock>::lock(self, r).await {
            Ok(a) => Ok(Some(a.into_inner().key)),
            Err(e) if try_only && e.code() == tonic::Code::Aborted => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// [embed] Frees the lock `key` holds and deletes the key, on the leader
    pub async fn embed_unlock(&self, key: &[u8]) -> Result<(), Status> {
        let r = self.peer_call(UnlockRequest { key: key.to_vec() }, "", None).await;
        <Self as Lock>::unlock(self, r).await.map(|_| ())
    }

    /// [embed] The leader the embedder names - YT Cluster's Boss - over the configured one
    /// and kept across a reconfigure; `""` the configured one again. A uuid that does not
    /// read is refused. Whether it changed: the next tick takes the change up - the new
    /// leader takes over, the one before stops.
    pub async fn set_leader(&self, leader: &str) -> Result<bool, String> {
        let id = if leader.trim().is_empty() { None } else { Some(crate::cluster::parse_uuid(&leader.trim().to_string())?) };
        let mut named = self.coord.named.lock().unwrap_or_else(|p| p.into_inner());
        let changed = *named != id;
        *named = id;
        Ok(changed)
    }

    /// [embed] the global lock timeout, seconds: a lock asked with no lease holds one of it
    pub async fn lock_ttl(&self) -> u64 {
        self.cfg.read().await.lock_ttl_secs()
    }
}

#[cfg(test)]
#[path = "embed_test.rs"]
mod tests;
