use std::collections::HashSet;
use std::pin::Pin;
use tokio_stream::Stream;
use tonic::{async_trait, Request, Response, Status};
use crate::cluster::EtcdNode;
use crate::etcdpb::etcdserverpb::auth_server::Auth;
use crate::etcdpb::etcdserverpb::{ AuthDisableRequest, AuthDisableResponse, AuthEnableRequest, AuthEnableResponse, AuthRoleAddRequest, AuthRoleAddResponse, AuthRoleDeleteRequest, AuthRoleDeleteResponse, AuthRoleGetRequest, AuthRoleGetResponse, AuthRoleGrantPermissionRequest, AuthRoleGrantPermissionResponse, AuthRoleListRequest, AuthRoleListResponse, AuthRoleRevokePermissionRequest, AuthRoleRevokePermissionResponse, AuthUserAddRequest, AuthUserAddResponse, AuthUserChangePasswordRequest, AuthUserChangePasswordResponse, AuthUserDeleteRequest, AuthUserDeleteResponse, AuthUserGetRequest, AuthUserGetResponse, AuthUserGrantRoleRequest, AuthUserGrantRoleResponse, AuthUserListRequest, AuthUserListResponse, AuthUserRevokeRoleRequest, AuthUserRevokeRoleResponse, AuthenticateRequest, AuthenticateResponse, MemberAddRequest, MemberAddResponse, MemberListRequest, MemberListResponse, MemberPromoteRequest, MemberPromoteResponse, MemberRemoveRequest, MemberRemoveResponse, MemberUpdateRequest, MemberUpdateResponse};
use crate::etcdpb::etcdserverpb::cluster_server::Cluster;
use crate::etcdpb::v3electionpb::{CampaignRequest, CampaignResponse, LeaderRequest, LeaderResponse, ProclaimRequest, ProclaimResponse, ResignRequest, ResignResponse};
use crate::etcdpb::v3electionpb::election_server::Election;
use crate::etcdpb::v3lockpb::lock_server::Lock;
use crate::etcdpb::v3lockpb::*;
use crate::srv::{ UNIMPL};

#[async_trait]
impl Lock for EtcdNode {
    async fn lock(&self, _request: Request<LockRequest>) -> Result<Response<LockResponse>, Status> {
        Err(Status::unimplemented(UNIMPL))
    }

    async fn unlock(&self, _request: Request<UnlockRequest>) -> Result<Response<UnlockResponse>, Status> {
        Err(Status::unimplemented(UNIMPL))
    }
}

type ObserveResult<T> = Result<Response<T>, Status>;

type ObserveResponseStream = Pin<Box<dyn Stream<Item = Result<LeaderResponse, Status>> + Send>>;

#[async_trait]
impl Election for EtcdNode {
    async fn campaign(&self, _request: Request<CampaignRequest>) -> Result<Response<CampaignResponse>, Status> {
        Err(Status::unimplemented(UNIMPL))
    }

    async fn proclaim(&self, _request: Request<ProclaimRequest>) -> Result<Response<ProclaimResponse>, Status> {
        Err(Status::unimplemented(UNIMPL))
    }

    async fn leader(&self, _request: Request<LeaderRequest>) -> Result<Response<LeaderResponse>, Status> {
        Err(Status::unimplemented(UNIMPL))
    }

    type ObserveStream = ObserveResponseStream;

    async fn observe(&self, _request: Request<LeaderRequest>) -> ObserveResult<ObserveResponseStream> {
        Err(Status::unimplemented(UNIMPL))
    }

    async fn resign(&self, _request: Request<ResignRequest>) -> Result<Response<ResignResponse>, Status> {
        Err(Status::unimplemented(UNIMPL))
    }
    //
}
#[async_trait]
impl Auth for EtcdNode {
    async fn auth_enable(&self, _request: Request<AuthEnableRequest>) -> Result<Response<AuthEnableResponse>, Status> {
        Err(Status::unimplemented(UNIMPL))
    }

    async fn auth_disable(&self, _request: Request<AuthDisableRequest>) -> Result<Response<AuthDisableResponse>, Status> {
        Err(Status::unimplemented(UNIMPL))
    }

    async fn authenticate(&self, _request: Request<AuthenticateRequest>) -> Result<Response<AuthenticateResponse>, Status> {
        Err(Status::unimplemented(UNIMPL))
    }

    async fn user_add(&self, _request: Request<AuthUserAddRequest>) -> Result<Response<AuthUserAddResponse>, Status> {
        Err(Status::unimplemented(UNIMPL))
    }

    async fn user_get(&self, _request: Request<AuthUserGetRequest>) -> Result<Response<AuthUserGetResponse>, Status> {
        Err(Status::unimplemented(UNIMPL))
    }

    async fn user_list(&self, _request: Request<AuthUserListRequest>) -> Result<Response<AuthUserListResponse>, Status> {
        Err(Status::unimplemented(UNIMPL))
    }

    async fn user_delete(&self, _request: Request<AuthUserDeleteRequest>) -> Result<Response<AuthUserDeleteResponse>, Status> {
        Err(Status::unimplemented(UNIMPL))
    }

    async fn user_change_password(&self, _request: Request<AuthUserChangePasswordRequest>) -> Result<Response<AuthUserChangePasswordResponse>, Status> {
        Err(Status::unimplemented(UNIMPL))
    }

    async fn user_grant_role(&self, _request: Request<AuthUserGrantRoleRequest>) -> Result<Response<AuthUserGrantRoleResponse>, Status> {
        Err(Status::unimplemented(UNIMPL))
    }

    async fn user_revoke_role(&self, _request: Request<AuthUserRevokeRoleRequest>) -> Result<Response<AuthUserRevokeRoleResponse>, Status> {
        Err(Status::unimplemented(UNIMPL))
    }

    async fn role_add(&self, _request: Request<AuthRoleAddRequest>) -> Result<Response<AuthRoleAddResponse>, Status> {
        Err(Status::unimplemented(UNIMPL))
    }

    async fn role_get(&self, _request: Request<AuthRoleGetRequest>) -> Result<Response<AuthRoleGetResponse>, Status> {
        Err(Status::unimplemented(UNIMPL))
    }

    async fn role_list(&self, _request: Request<AuthRoleListRequest>) -> Result<Response<AuthRoleListResponse>, Status> {
        Err(Status::unimplemented(UNIMPL))
    }

    async fn role_delete(&self, _request: Request<AuthRoleDeleteRequest>) -> Result<Response<AuthRoleDeleteResponse>, Status> {
        Err(Status::unimplemented(UNIMPL))
    }

    async fn role_grant_permission(&self, _request: Request<AuthRoleGrantPermissionRequest>) -> Result<Response<AuthRoleGrantPermissionResponse>, Status> {
        Err(Status::unimplemented(UNIMPL))
    }

    async fn role_revoke_permission(&self, _request: Request<AuthRoleRevokePermissionRequest>) -> Result<Response<AuthRoleRevokePermissionResponse>, Status> {
        Err(Status::unimplemented(UNIMPL))
    }
    //
}

#[async_trait]
impl Cluster for EtcdNode {
    async fn member_add(&self, request: Request<MemberAddRequest>) -> Result<Response<MemberAddResponse>, Status> {
        let ma = request.into_inner();
        let peers: HashSet<&str> = HashSet::from_iter(ma.peer_ur_ls.iter().map(|p| p.as_str()));
        self.peers.write().await.add_connections(peers).await
            .map(|_r| Response::new(MemberAddResponse::default()))
            .map_err(|e| Status::invalid_argument(e))
    }

    async fn member_remove(&self, _request: Request<MemberRemoveRequest>) -> Result<Response<MemberRemoveResponse>, Status> {
        Err(Status::unimplemented(UNIMPL))
    }

    async fn member_update(&self, _request: Request<MemberUpdateRequest>) -> Result<Response<MemberUpdateResponse>, Status> {
        Err(Status::unimplemented(UNIMPL))
    }

    async fn member_list(&self, _request: Request<MemberListRequest>) -> Result<Response<MemberListResponse>, Status> {
        Err(Status::unimplemented(UNIMPL))
    }

    async fn member_promote(&self, _request: Request<MemberPromoteRequest>) -> Result<Response<MemberPromoteResponse>, Status> {
        Err(Status::unimplemented(UNIMPL))
    }
}