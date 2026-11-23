use std::hash::{Hash, Hasher};
use std::pin::Pin;
use prost::Message;
use tokio_stream::Stream;
use tonic::{async_trait, Request, Response, Status};
use crate::cluster::EtcdNode;
use crate::etcdpb::etcdserverpb::maintenance_server::Maintenance;
use crate::etcdpb::etcdserverpb::{DowngradeRequest, DowngradeResponse, AlarmRequest, AlarmResponse, DefragmentRequest, DefragmentResponse, HashKvRequest, HashKvResponse, HashRequest, HashResponse, MoveLeaderRequest, MoveLeaderResponse, SnapshotRequest, SnapshotResponse, StatusRequest, StatusResponse};
use crate::etcdpb::mvccpb::KeyValue;

type SnapshotResultStream = Pin<Box<dyn Stream<Item = Result<SnapshotResponse, Status>> + Send>>;
type SnapshotResult<T> = Result<Response<T>, Status>;

#[async_trait]
impl Maintenance for EtcdNode {
    /// [coord] etcd's alarms, kept on this node: `GET` lists them, `ACTIVATE` and
    /// `DEACTIVATE` raise and clear one (NOSPACE, CORRUPT) for a member - none raises
    /// itself here, etcds has no disk to fill
    async fn alarm(&self, request: Request<AlarmRequest>) -> Result<Response<AlarmResponse>, Status> {
        use crate::etcdpb::etcdserverpb::alarm_request::AlarmAction;
        use crate::etcdpb::etcdserverpb::AlarmMember;
        let r = *request.get_ref();
        if r.action == AlarmAction::Get as i32 {
            self.check_call(request.metadata(), None).await?;
        } else {
            self.check_root(request.metadata()).await?;
        }
        let mut alarms = self.coord.alarms.lock().unwrap_or_else(|p| p.into_inner());
        let member = if r.member_id == 0 { self.node_id } else { r.member_id };
        let one = AlarmMember { member_id: member, alarm: r.alarm };
        match AlarmAction::try_from(r.action) {
            Ok(AlarmAction::Get) => {}
            Ok(AlarmAction::Activate) => {
                if r.alarm == 0 {
                    return Err(Status::invalid_argument(tr!("srv.alarm_none")));
                }
                if !alarms.contains(&one) {
                    alarms.push(one);
                }
            }
            Ok(AlarmAction::Deactivate) => alarms.retain(|a| *a != one),
            Err(_) => return Err(Status::invalid_argument(tr!("srv.alarm_action", action = r.action))),
        }
        let alarms = alarms.iter().filter(|a| r.alarm == 0 || a.alarm == r.alarm).cloned().collect();
        Ok(Response::new(AlarmResponse { header: Some(self.response_header()), alarms }))
    }

    /// Open to any caller, as a reachability probe: a peer dials with it before it holds
    /// a token. [coord] the leader and the alarms raised here
    async fn status(&self, _request: Request<StatusRequest>) -> Result<Response<StatusResponse>, Status> {
        let leader = self.leader().await;
        let errors: Vec<String> = self.coord.alarms.lock().unwrap_or_else(|p| p.into_inner()).iter()
            .map(|a| format!("alarm:{}", crate::etcdpb::etcdserverpb::AlarmType::try_from(a.alarm).map(|t| t.as_str_name()).unwrap_or("UNKNOWN")))
            .collect();
        let vault_size = self.vault.read().await.len() as i64;
        let header = self.response_header();
        Ok(Response::new(StatusResponse {
            header: Some(header),
            // [q-dead] its crate version and registry format, for a peer to tell
            // an older node (`peer::registry_of`)
            version: crate::peer::status_version(),
            db_size: vault_size,
            leader,
            raft_index: 0,
            raft_term: header.raft_term,
            raft_applied_index: 0,
            errors,
            db_size_in_use: vault_size,
            is_learner: false,
            // etcds has neither a storage version nor a quota, and nothing to downgrade
            storage_version: String::new(),
            db_size_quota: 0,
            downgrade_info: None,
        }))
    }

    /// etcds keeps nothing on disk: there is no storage version to downgrade. `VALIDATE`
    /// and `CANCEL` answer with this node's version, `ENABLE` is refused
    async fn downgrade(&self, request: Request<DowngradeRequest>) -> Result<Response<DowngradeResponse>, Status> {
        use crate::etcdpb::etcdserverpb::downgrade_request::DowngradeAction;
        self.check_root(request.metadata()).await?;
        match DowngradeAction::try_from(request.get_ref().action) {
            Ok(DowngradeAction::Enable) => Err(Status::failed_precondition(tr!("srv.no_downgrade"))),
            _ => Ok(Response::new(DowngradeResponse { header: Some(self.response_header()), version: env!("CARGO_PKG_VERSION").to_string() })),
        }
    }

    async fn defragment(&self, request: Request<DefragmentRequest>) -> Result<Response<DefragmentResponse>, Status> {
        self.check_root(request.metadata()).await?;
        Ok(Response::new(DefragmentResponse {
            header: Some(self.response_header()),
        }))
    }

    async fn hash(&self, request: Request<HashRequest>) -> Result<Response<HashResponse>, Status> {
        self.check_root(request.metadata()).await?;
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
        self.check_root(request.metadata()).await?;
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
            // etcds has no revisions: the hash is of the store as it is now
            hash_revision: 0,
        }))
    }

    type SnapshotStream = SnapshotResultStream;

    async fn snapshot(&self, request: Request<SnapshotRequest>) -> SnapshotResult<SnapshotResultStream> {
        self.check_root(request.metadata()).await?;
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
            // no etcd server's version: this is not an etcd snapshot to restore with one
            version: String::new(),
        };

        let stream = tokio_stream::once(Ok(response));
        Ok(Response::new(Box::pin(stream)))
    }

    async fn move_leader(&self, request: Request<MoveLeaderRequest>) -> Result<Response<MoveLeaderResponse>, Status> {
        self.check_root(request.metadata()).await?;
        Ok(Response::new(MoveLeaderResponse {
            header: Some(self.response_header()),
        }))
    }

}
