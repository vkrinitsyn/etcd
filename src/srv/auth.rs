//! [auth] The Auth service (owner, 2026-11-21). With the `auth` feature and
//! `auth_db_url`, signon's accounts, roles and permissions (`auth.rs`); without them,
//! every call answers `unimplemented`, as before.
use crate::cluster::EtcdNode;
use crate::etcdpb::etcdserverpb::auth_server::Auth;
use crate::etcdpb::etcdserverpb::{AuthDisableRequest, AuthDisableResponse, AuthEnableRequest, AuthEnableResponse, AuthRoleAddRequest, AuthRoleAddResponse, AuthRoleDeleteRequest, AuthRoleDeleteResponse, AuthRoleGetRequest, AuthRoleGetResponse, AuthRoleGrantPermissionRequest, AuthRoleGrantPermissionResponse, AuthRoleListRequest, AuthRoleListResponse, AuthRoleRevokePermissionRequest, AuthRoleRevokePermissionResponse, AuthUserAddRequest, AuthUserAddResponse, AuthUserChangePasswordRequest, AuthUserChangePasswordResponse, AuthUserDeleteRequest, AuthUserDeleteResponse, AuthUserGetRequest, AuthUserGetResponse, AuthUserGrantRoleRequest, AuthUserGrantRoleResponse, AuthUserListRequest, AuthUserListResponse, AuthUserRevokeRoleRequest, AuthUserRevokeRoleResponse, AuthStatusRequest, AuthStatusResponse, AuthenticateRequest, AuthenticateResponse};
use tonic::{async_trait, Request, Response, Status};

#[cfg(feature = "auth")]
use crate::auth::{from_etcd, to_etcd, Auth as Held, ROOT, SCOPE};
#[cfg(feature = "auth")]
use crate::coord::{has, XREP};
#[cfg(feature = "auth")]
use crate::etcdpb::authpb::Permission as EtcdPermission;
#[cfg(feature = "auth")]
use std::sync::atomic::Ordering;

#[cfg(feature = "auth")]
impl EtcdNode {
    /// signon, or `unimplemented` for a node with no `auth_db_url`
    fn signon(&self) -> Result<&std::sync::Arc<Held>, Status> {
        self.auth.as_ref().ok_or_else(|| Status::unimplemented(tr!("srv.unimplemented")))
    }

    fn db_err(e: impl std::fmt::Display) -> Status {
        Status::internal(tr!("auth.db", err = e))
    }

    /// Turns authentication on or off here and has every peer it writes to do the same
    async fn switch_auth(&self, on: bool) {
        use crate::etcdpb::etcdserverpb::auth_client::AuthClient;
        if let Some(a) = &self.auth {
            a.enabled.store(on, Ordering::Relaxed);
        }
        let targets: Vec<tonic::transport::Channel> = {
            let peers = self.peers.read().await;
            let mut v = Vec::new();
            for (pid, _, state) in peers.peer_info().await {
                if state != crate::peer::PeerState::Spare
                    && let Some(h) = peers.handle(pid).await {
                        v.push(h.lock().await.channel.clone());
                    }
            }
            v
        };
        for ch in targets {
            let mut c = AuthClient::new(ch);
            let sent = if on {
                c.auth_enable(self.peer_call(AuthEnableRequest {}, XREP, None).await).await.map(|_| ())
            } else {
                c.auth_disable(self.peer_call(AuthDisableRequest {}, XREP, None).await).await.map(|_| ())
            };
            if let Err(e) = sent {
                slog::warn!(self.log, "{}{}", crate::LP, tr!("auth.not_switched", err = e.message()));
            }
        }
    }

    /// signon's role, refused as etcd refuses an unknown one
    async fn role_exists(&self, a: &Held, role: &str) -> Result<(), Status> {
        let all = signon::api::roles(&a.state.pool).await.map_err(Self::db_err)?;
        if all.iter().any(|r| r == role) {
            Ok(())
        } else {
            Err(Status::failed_precondition(tr!("auth.no_role", role = role)))
        }
    }
}

#[cfg(feature = "auth")]
#[async_trait]
impl Auth for EtcdNode {
    async fn auth_enable(&self, request: Request<AuthEnableRequest>) -> Result<Response<AuthEnableResponse>, Status> {
        let a = self.signon()?;
        if has(request.metadata(), XREP) {
            self.check_root(request.metadata()).await?;
            a.enabled.store(true, Ordering::Relaxed);
            return Ok(Response::new(AuthEnableResponse { header: Some(self.response_header()) }));
        }
        self.check_root(request.metadata()).await?;
        // as etcd: on only with an account that can turn it off again
        let roots = signon::api::accounts(&a.state.pool).await.map_err(Self::db_err)?;
        let mut any_root = false;
        for name in roots {
            if signon::api::account(&a.state.pool, &name).await.map_err(Self::db_err)?
                .is_some_and(|i| i.roles.iter().any(|r| r == ROOT)) {
                any_root = true;
                break;
            }
        }
        if !any_root {
            return Err(Status::failed_precondition(tr!("auth.no_root")));
        }
        self.switch_auth(true).await;
        Ok(Response::new(AuthEnableResponse { header: Some(self.response_header()) }))
    }

    async fn auth_disable(&self, request: Request<AuthDisableRequest>) -> Result<Response<AuthDisableResponse>, Status> {
        let a = self.signon()?;
        self.check_root(request.metadata()).await?;
        if has(request.metadata(), XREP) {
            a.enabled.store(false, Ordering::Relaxed);
        } else {
            self.switch_auth(false).await;
        }
        Ok(Response::new(AuthDisableResponse { header: Some(self.response_header()) }))
    }

    async fn auth_status(&self, _request: Request<AuthStatusRequest>) -> Result<Response<AuthStatusResponse>, Status> {
        let a = self.signon()?;
        Ok(Response::new(AuthStatusResponse { header: Some(self.response_header()), enabled: a.is_on(), auth_revision: 0 }))
    }

    /// signon's sign-in: its limits and SSO rules hold here too; the token is a session
    async fn authenticate(&self, request: Request<AuthenticateRequest>) -> Result<Response<AuthenticateResponse>, Status> {
        let a = self.signon()?;
        if !a.is_on() {
            return Err(Status::failed_precondition(tr!("auth.off")));
        }
        let client = request.remote_addr().map(|r| r.ip().to_string()).unwrap_or_else(|| "unknown".to_string());
        let r = request.into_inner();
        match signon::api::sign_in(&a.state, &r.name, &r.password, &client, "etcd").await {
            Ok(s) => Ok(Response::new(AuthenticateResponse { header: Some(self.response_header()), token: s.token })),
            Err(signon::api::SignInRefusal::Credentials) => Err(Status::invalid_argument(tr!("auth.failed"))),
            Err(signon::api::SignInRefusal::TooMany) => Err(Status::resource_exhausted(tr!("auth.too_many"))),
            Err(signon::api::SignInRefusal::SsoRequired) => Err(Status::failed_precondition(tr!("auth.sso"))),
            Err(signon::api::SignInRefusal::Internal(e)) => Err(Self::db_err(e)),
        }
    }

    async fn user_add(&self, request: Request<AuthUserAddRequest>) -> Result<Response<AuthUserAddResponse>, Status> {
        self.signon()?;
        self.check_root(request.metadata()).await?;
        Err(Status::failed_precondition(tr!("auth.in_signon")))
    }

    async fn user_get(&self, request: Request<AuthUserGetRequest>) -> Result<Response<AuthUserGetResponse>, Status> {
        let a = self.signon()?;
        self.check_root(request.metadata()).await?;
        let name = request.into_inner().name;
        match signon::api::account(&a.state.pool, &name).await.map_err(Self::db_err)? {
            Some(i) => Ok(Response::new(AuthUserGetResponse { header: Some(self.response_header()), roles: i.roles })),
            None => Err(Status::failed_precondition(tr!("auth.no_user", user = name))),
        }
    }

    async fn user_list(&self, request: Request<AuthUserListRequest>) -> Result<Response<AuthUserListResponse>, Status> {
        let a = self.signon()?;
        self.check_root(request.metadata()).await?;
        let users = signon::api::accounts(&a.state.pool).await.map_err(Self::db_err)?;
        Ok(Response::new(AuthUserListResponse { header: Some(self.response_header()), users }))
    }

    async fn user_delete(&self, request: Request<AuthUserDeleteRequest>) -> Result<Response<AuthUserDeleteResponse>, Status> {
        self.signon()?;
        self.check_root(request.metadata()).await?;
        Err(Status::failed_precondition(tr!("auth.in_signon")))
    }

    async fn user_change_password(&self, request: Request<AuthUserChangePasswordRequest>) -> Result<Response<AuthUserChangePasswordResponse>, Status> {
        self.signon()?;
        self.check_root(request.metadata()).await?;
        Err(Status::failed_precondition(tr!("auth.in_signon")))
    }

    async fn user_grant_role(&self, request: Request<AuthUserGrantRoleRequest>) -> Result<Response<AuthUserGrantRoleResponse>, Status> {
        let a = self.signon()?;
        self.check_root(request.metadata()).await?;
        let r = request.into_inner();
        self.role_exists(a, &r.role).await?;
        let Some(i) = signon::api::account(&a.state.pool, &r.user).await.map_err(Self::db_err)? else {
            return Err(Status::failed_precondition(tr!("auth.no_user", user = r.user)));
        };
        signon::grant_role(&a.state.pool, i.user_id, &r.role).await.map_err(Self::db_err)?;
        a.forget().await;
        Ok(Response::new(AuthUserGrantRoleResponse { header: Some(self.response_header()) }))
    }

    async fn user_revoke_role(&self, request: Request<AuthUserRevokeRoleRequest>) -> Result<Response<AuthUserRevokeRoleResponse>, Status> {
        let a = self.signon()?;
        self.check_root(request.metadata()).await?;
        let r = request.into_inner();
        let Some(i) = signon::api::account(&a.state.pool, &r.name).await.map_err(Self::db_err)? else {
            return Err(Status::failed_precondition(tr!("auth.no_user", user = r.name)));
        };
        if !signon::api::revoke_role(&a.state.pool, i.user_id, &r.role).await.map_err(Self::db_err)? {
            return Err(Status::failed_precondition(tr!("auth.role_not_granted", role = r.role)));
        }
        a.forget().await;
        Ok(Response::new(AuthUserRevokeRoleResponse { header: Some(self.response_header()) }))
    }

    async fn role_add(&self, request: Request<AuthRoleAddRequest>) -> Result<Response<AuthRoleAddResponse>, Status> {
        let a = self.signon()?;
        self.check_root(request.metadata()).await?;
        let name = request.into_inner().name;
        if name.trim().is_empty() {
            return Err(Status::invalid_argument(tr!("auth.empty_role")));
        }
        if !signon::api::add_role(&a.state.pool, &name).await.map_err(Self::db_err)? {
            return Err(Status::failed_precondition(tr!("auth.role_exists", role = name)));
        }
        Ok(Response::new(AuthRoleAddResponse { header: Some(self.response_header()) }))
    }

    async fn role_get(&self, request: Request<AuthRoleGetRequest>) -> Result<Response<AuthRoleGetResponse>, Status> {
        let a = self.signon()?;
        self.check_root(request.metadata()).await?;
        let role = request.into_inner().role;
        self.role_exists(a, &role).await?;
        let perm = signon::permissions::of_role(&a.state.pool, &role, SCOPE).await.map_err(Self::db_err)?
            .into_iter()
            .map(|p| EtcdPermission { perm_type: to_etcd(p.perm), key: p.key, range_end: p.range_end })
            .collect();
        Ok(Response::new(AuthRoleGetResponse { header: Some(self.response_header()), perm }))
    }

    async fn role_list(&self, request: Request<AuthRoleListRequest>) -> Result<Response<AuthRoleListResponse>, Status> {
        let a = self.signon()?;
        self.check_root(request.metadata()).await?;
        let roles = signon::api::roles(&a.state.pool).await.map_err(Self::db_err)?;
        Ok(Response::new(AuthRoleListResponse { header: Some(self.response_header()), roles }))
    }

    async fn role_delete(&self, request: Request<AuthRoleDeleteRequest>) -> Result<Response<AuthRoleDeleteResponse>, Status> {
        let a = self.signon()?;
        self.check_root(request.metadata()).await?;
        let role = request.into_inner().role;
        if role == ROOT {
            return Err(Status::failed_precondition(tr!("auth.keep_root")));
        }
        if !signon::api::delete_role(&a.state.pool, &role).await.map_err(Self::db_err)? {
            return Err(Status::failed_precondition(tr!("auth.no_role", role = role)));
        }
        a.forget().await;
        Ok(Response::new(AuthRoleDeleteResponse { header: Some(self.response_header()) }))
    }

    async fn role_grant_permission(&self, request: Request<AuthRoleGrantPermissionRequest>) -> Result<Response<AuthRoleGrantPermissionResponse>, Status> {
        let a = self.signon()?;
        self.check_root(request.metadata()).await?;
        let r = request.into_inner();
        let Some(p) = r.perm else {
            return Err(Status::invalid_argument(tr!("auth.no_perm")));
        };
        let Some(perm) = from_etcd(p.perm_type) else {
            return Err(Status::invalid_argument(tr!("auth.bad_perm", perm = p.perm_type)));
        };
        if !signon::permissions::grant(&a.state.pool, &r.name, SCOPE, &p.key, &p.range_end, perm).await.map_err(Self::db_err)? {
            return Err(Status::failed_precondition(tr!("auth.no_role", role = r.name)));
        }
        a.forget().await;
        Ok(Response::new(AuthRoleGrantPermissionResponse { header: Some(self.response_header()) }))
    }

    async fn role_revoke_permission(&self, request: Request<AuthRoleRevokePermissionRequest>) -> Result<Response<AuthRoleRevokePermissionResponse>, Status> {
        let a = self.signon()?;
        self.check_root(request.metadata()).await?;
        let r = request.into_inner();
        if !signon::permissions::revoke(&a.state.pool, &r.role, SCOPE, &r.key, &r.range_end).await.map_err(Self::db_err)? {
            return Err(Status::failed_precondition(tr!("auth.perm_not_granted", role = r.role)));
        }
        a.forget().await;
        Ok(Response::new(AuthRoleRevokePermissionResponse { header: Some(self.response_header()) }))
    }
}

/// Without the `auth` feature: every call `unimplemented`, as before
#[cfg(not(feature = "auth"))]
#[async_trait]
impl Auth for EtcdNode {
    async fn auth_enable(&self, _r: Request<AuthEnableRequest>) -> Result<Response<AuthEnableResponse>, Status> { Err(no()) }
    async fn auth_disable(&self, _r: Request<AuthDisableRequest>) -> Result<Response<AuthDisableResponse>, Status> { Err(no()) }
    async fn auth_status(&self, _r: Request<AuthStatusRequest>) -> Result<Response<AuthStatusResponse>, Status> { Err(no()) }
    async fn authenticate(&self, _r: Request<AuthenticateRequest>) -> Result<Response<AuthenticateResponse>, Status> { Err(no()) }
    async fn user_add(&self, _r: Request<AuthUserAddRequest>) -> Result<Response<AuthUserAddResponse>, Status> { Err(no()) }
    async fn user_get(&self, _r: Request<AuthUserGetRequest>) -> Result<Response<AuthUserGetResponse>, Status> { Err(no()) }
    async fn user_list(&self, _r: Request<AuthUserListRequest>) -> Result<Response<AuthUserListResponse>, Status> { Err(no()) }
    async fn user_delete(&self, _r: Request<AuthUserDeleteRequest>) -> Result<Response<AuthUserDeleteResponse>, Status> { Err(no()) }
    async fn user_change_password(&self, _r: Request<AuthUserChangePasswordRequest>) -> Result<Response<AuthUserChangePasswordResponse>, Status> { Err(no()) }
    async fn user_grant_role(&self, _r: Request<AuthUserGrantRoleRequest>) -> Result<Response<AuthUserGrantRoleResponse>, Status> { Err(no()) }
    async fn user_revoke_role(&self, _r: Request<AuthUserRevokeRoleRequest>) -> Result<Response<AuthUserRevokeRoleResponse>, Status> { Err(no()) }
    async fn role_add(&self, _r: Request<AuthRoleAddRequest>) -> Result<Response<AuthRoleAddResponse>, Status> { Err(no()) }
    async fn role_get(&self, _r: Request<AuthRoleGetRequest>) -> Result<Response<AuthRoleGetResponse>, Status> { Err(no()) }
    async fn role_list(&self, _r: Request<AuthRoleListRequest>) -> Result<Response<AuthRoleListResponse>, Status> { Err(no()) }
    async fn role_delete(&self, _r: Request<AuthRoleDeleteRequest>) -> Result<Response<AuthRoleDeleteResponse>, Status> { Err(no()) }
    async fn role_grant_permission(&self, _r: Request<AuthRoleGrantPermissionRequest>) -> Result<Response<AuthRoleGrantPermissionResponse>, Status> { Err(no()) }
    async fn role_revoke_permission(&self, _r: Request<AuthRoleRevokePermissionRequest>) -> Result<Response<AuthRoleRevokePermissionResponse>, Status> { Err(no()) }
}

#[cfg(not(feature = "auth"))]
fn no() -> Status {
    Status::unimplemented(tr!("srv.unimplemented"))
}
