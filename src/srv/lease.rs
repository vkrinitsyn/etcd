use crate::cluster::EtcdNode;
use crate::etcdpb::etcdserverpb::lease_server::Lease;
use crate::etcdpb::etcdserverpb::{LeaseGrantRequest, LeaseGrantResponse, LeaseKeepAliveRequest, LeaseKeepAliveResponse, LeaseLeasesRequest, LeaseLeasesResponse, LeaseRevokeRequest, LeaseRevokeResponse, LeaseTimeToLiveRequest, LeaseTimeToLiveResponse};
use crate::srv::UNIMPL;
use std::pin::Pin;
use tokio_stream::Stream;
use tonic::{async_trait, Request, Response, Status, Streaming};

type LeaseKeepAliveResult<T> = Result<Response<T>, Status>;
type ResponseStream = Pin<Box<dyn Stream<Item = Result<LeaseKeepAliveResponse, Status>> + Send>>;

#[async_trait]
impl Lease for EtcdNode {

    /// TODO
    async fn lease_grant(&self, request: Request<LeaseGrantRequest>) -> Result<Response<LeaseGrantResponse>, Status> {
        Err(Status::unimplemented("TODO"))
    }

    /// TODO
    async fn lease_revoke(&self, request: Request<LeaseRevokeRequest>) -> Result<Response<LeaseRevokeResponse>, Status> {
        Err(Status::unimplemented("TODO"))
    }

    type LeaseKeepAliveStream = ResponseStream;

    async fn lease_keep_alive(&self, request: Request<Streaming<LeaseKeepAliveRequest>>) -> LeaseKeepAliveResult<ResponseStream> {
        Err(Status::unimplemented(UNIMPL))
    }

    /// TODO
    async fn lease_time_to_live(&self, request: Request<LeaseTimeToLiveRequest>) -> Result<Response<LeaseTimeToLiveResponse>, Status> {
        Err(Status::unimplemented("TODO"))
    }

    /// TODO
    async fn lease_leases(&self, request: Request<LeaseLeasesRequest>) -> Result<Response<LeaseLeasesResponse>, Status> {
        Err(Status::unimplemented("TODO"))
    }
}
