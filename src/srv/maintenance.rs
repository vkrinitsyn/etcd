use std::hash::{Hash, Hasher};
use std::pin::Pin;
use prost::Message;
use tokio_stream::Stream;
use tonic::{async_trait, Request, Response, Status};
use crate::cluster::EtcdNode;
use crate::etcdpb::etcdserverpb::maintenance_server::Maintenance;
use crate::etcdpb::etcdserverpb::{AlarmRequest, AlarmResponse, DefragmentRequest, DefragmentResponse, HashKvRequest, HashKvResponse, HashRequest, HashResponse, MoveLeaderRequest, MoveLeaderResponse, SnapshotRequest, SnapshotResponse, StatusRequest, StatusResponse};
use crate::etcdpb::mvccpb::KeyValue;

type SnapshotResultStream = Pin<Box<dyn Stream<Item = Result<SnapshotResponse, Status>> + Send>>;
type SnapshotResult<T> = Result<Response<T>, Status>;

#[async_trait]
impl Maintenance for EtcdNode {
    async fn alarm(&self, _request: Request<AlarmRequest>) -> Result<Response<AlarmResponse>, Status> {
        Err(Status::unimplemented(tr!("srv.unimplemented")))
    }

    async fn status(&self, _request: Request<StatusRequest>) -> Result<Response<StatusResponse>, Status> {
        let vault_size = self.vault.read().await.len() as i64;
        let header = self.response_header();
        Ok(Response::new(StatusResponse {
            header: Some(header),
            version: "rust".to_string(),
            db_size: vault_size,
            leader: 0,
            raft_index: 0,
            raft_term: header.raft_term,
            raft_applied_index: 0,
            errors: vec![],
            db_size_in_use: vault_size,
            is_learner: false,
        }))
    }

    async fn defragment(&self, _request: Request<DefragmentRequest>) -> Result<Response<DefragmentResponse>, Status> {
        Ok(Response::new(DefragmentResponse {
            header: Some(self.response_header()),
        }))
    }

    async fn hash(&self, request: Request<HashRequest>) -> Result<Response<HashResponse>, Status> {
        let remote = request.remote_addr();
        let policy = self.policy.read().await.clone();
        let vault = self.vault.read().await;
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        // [E4] over what peers share and this caller may see: a local key differs
        // per node by design, and a local-only one is not a remote caller's
        for (k, v) in vault.iter().filter(|(k, _)| policy.propagates(k) && policy.may_serve(k, remote)) {
            k.hash(&mut hasher);
            v.value.hash(&mut hasher);
        }
        Ok(Response::new(HashResponse {
            header: Some(self.response_header()),
            hash: hasher.finish() as u32,
        }))
    }

    async fn hash_kv(&self, request: Request<HashKvRequest>) -> Result<Response<HashKvResponse>, Status> {
        let remote = request.remote_addr();
        let policy = self.policy.read().await.clone();
        let vault = self.vault.read().await;
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        // [E4] as `hash`
        for (k, v) in vault.iter().filter(|(k, _)| policy.propagates(k) && policy.may_serve(k, remote)) {
            k.hash(&mut hasher);
            v.value.hash(&mut hasher);
            v.version.hash(&mut hasher);
            v.create_revision.hash(&mut hasher);
            v.mod_revision.hash(&mut hasher);
        }
        Ok(Response::new(HashKvResponse {
            header: Some(self.response_header()),
            hash: hasher.finish() as u32,
            compact_revision: 0,
        }))
    }

    type SnapshotStream = SnapshotResultStream;

    async fn snapshot(&self, request: Request<SnapshotRequest>) -> SnapshotResult<SnapshotResultStream> {
        let remote = request.remote_addr();
        let policy = self.policy.read().await.clone();
        let vault = self.vault.read().await;
        let mut blob = Vec::new();
        // [E4] a remote caller's snapshot leaves out what it may not read
        for v in vault.values().filter(|v| policy.may_serve(&v.key, remote)) {
            let kv: KeyValue = v.into();
            let encoded = kv.encode_to_vec();
            let len = encoded.len() as u32;
            blob.extend_from_slice(&len.to_be_bytes());
            blob.extend_from_slice(&encoded);
        }
        drop(vault);

        let response = SnapshotResponse {
            header: Some(self.response_header()),
            remaining_bytes: 0,
            blob,
        };

        let stream = tokio_stream::once(Ok(response));
        Ok(Response::new(Box::pin(stream)))
    }

    async fn move_leader(&self, _request: Request<MoveLeaderRequest>) -> Result<Response<MoveLeaderResponse>, Status> {
        Ok(Response::new(MoveLeaderResponse {
            header: Some(self.response_header()),
        }))
    }

}
