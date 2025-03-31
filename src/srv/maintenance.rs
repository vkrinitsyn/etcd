use std::pin::Pin;
use tokio_stream::Stream;
use tonic::{async_trait, Request, Response, Status};
use crate::cluster::EtcdNode;
use crate::etcdpb::etcdserverpb::maintenance_server::Maintenance;
use crate::etcdpb::etcdserverpb::{AlarmRequest, AlarmResponse, DefragmentRequest, DefragmentResponse, HashKvRequest, HashKvResponse, HashRequest, HashResponse, MoveLeaderRequest, MoveLeaderResponse, ResponseHeader, SnapshotRequest, SnapshotResponse, StatusRequest, StatusResponse};
use crate::srv::UNIMPL;

type SnapshotResultStream = Pin<Box<dyn Stream<Item = Result<SnapshotResponse, Status>> + Send>>;
type SnapshotResult<T> = Result<Response<T>, Status>;

#[async_trait]
impl Maintenance for EtcdNode {
    async fn alarm(&self, _request: Request<AlarmRequest>) -> Result<Response<AlarmResponse>, Status> {
        Err(Status::unimplemented(UNIMPL))
    }

    /// .
    async fn status(&self, _request: Request<StatusRequest>) -> Result<Response<StatusResponse>, Status> {
        
        Ok(Response::new(StatusResponse {
            header: Some(ResponseHeader {
                cluster_id: 0,
                member_id: self.node_id,
                revision: 0,
                raft_term: 0,
            }),
            version: "rust".to_string(),
            db_size: 0,
            leader: 0,
            raft_index: 0,
            raft_term: 0,
            raft_applied_index: 0,
            errors: vec![],
            db_size_in_use: 0,
            is_learner: false,
        }))
    }

    async fn defragment(&self, _request: Request<DefragmentRequest>) -> Result<Response<DefragmentResponse>, Status> {
        Err(Status::unimplemented(UNIMPL))
    }

    async fn hash(&self, _request: Request<HashRequest>) -> Result<Response<HashResponse>, Status> {
        Err(Status::unimplemented(UNIMPL))
    }

    async fn hash_kv(&self, _request: Request<HashKvRequest>) -> Result<Response<HashKvResponse>, Status> {
        Err(Status::unimplemented(UNIMPL))
    }

    type SnapshotStream = SnapshotResultStream;

    async fn snapshot(&self, _request: Request<SnapshotRequest>) -> SnapshotResult<SnapshotResultStream> {
        Err(Status::unimplemented(UNIMPL))
    }

    async fn move_leader(&self, _request: Request<MoveLeaderRequest>) -> Result<Response<MoveLeaderResponse>, Status> {
        Err(Status::unimplemented(UNIMPL))
    }

}
