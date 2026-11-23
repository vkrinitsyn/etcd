//! [coord] The Lease service (owner, 2026-11-21): granted, kept alive and expired on the
//! leader (`coord.rs`); another node sends the call on to it. A grant and a revoke are
//! replicated to every peer, so any node checks a put's lease and a new leader holds
//! them all.
use crate::cluster::EtcdNode;
use crate::coord::{has, Lease as Held, MIN_TTL, XFWD, XREP};
use crate::etcdpb::etcdserverpb::lease_client::LeaseClient;
use crate::etcdpb::etcdserverpb::lease_server::Lease;
use crate::etcdpb::etcdserverpb::{LeaseGrantRequest, LeaseGrantResponse, LeaseKeepAliveRequest, LeaseKeepAliveResponse, LeaseLeasesRequest, LeaseLeasesResponse, LeaseRevokeRequest, LeaseRevokeResponse, LeaseStatus, LeaseTimeToLiveRequest, LeaseTimeToLiveResponse};
use std::pin::Pin;
use tokio::time::{Duration, Instant};
use tokio_stream::{Stream, StreamExt};
use tonic::{async_trait, Request, Response, Status, Streaming};

type LeaseKeepAliveResult<T> = Result<Response<T>, Status>;
type ResponseStream = Pin<Box<dyn Stream<Item = Result<LeaseKeepAliveResponse, Status>> + Send>>;

/// a fresh lease id: positive, random - a new leader's grants never meet an old one's
fn fresh_id(taken: &std::collections::HashMap<i64, Held>) -> i64 {
    loop {
        let id = (uuid::Uuid::new_v4().as_u64_pair().0 >> 2) as i64;
        if id > 0 && !taken.contains_key(&id) {
            return id;
        }
    }
}

impl EtcdNode {
    /// [coord] A lease granted on the leader: `id` asked for or a fresh one, `ttl` at least
    /// [`MIN_TTL`]. [sys] Its record written before the answer, so a new leader keeps it,
    /// then replicated to each peer. The id and the TTL granted.
    pub(crate) async fn grant_lease_here(&self, id: i64, ttl: i64) -> Result<(i64, i64), Status> {
        let ttl = ttl.max(MIN_TTL);
        let id = {
            let mut leases = self.coord.leases.write().await;
            let id = if id != 0 {
                if id < 0 {
                    return Err(Status::invalid_argument(tr!("lease.bad_id", id = id)));
                }
                if leases.contains_key(&id) {
                    return Err(Status::failed_precondition(tr!("lease.exists", id = id)));
                }
                id
            } else {
                fresh_id(&leases)
            };
            leases.insert(id, Held { granted_ttl: ttl, deadline: Instant::now() + Duration::from_secs(ttl as u64) });
            id
        };
        if let Err(e) = self.sys_write(&crate::sys::lease_key(id), Some(serde_json::to_string(&crate::sys::LeaseRec { ttl }).unwrap_or_default())).await {
            self.coord.leases.write().await.remove(&id);
            // [review 2026-11-21] the store may hold it though it failed to say so: its
            // delete tried too, or a new leader would bring back a lease no one was given
            let _ = self.sys_write(&crate::sys::lease_key(id), None).await;
            return Err(e);
        }
        self.replicate_lease(id, Some(ttl)).await;
        Ok((id, ttl))
    }

    /// [embed] Lease `id` renewed here, the leader: the TTL it was granted, 0 for a lease gone
    pub(crate) async fn renew_lease_here(&self, id: i64) -> i64 {
        let mut leases = self.coord.leases.write().await;
        match leases.get_mut(&id) {
            Some(l) => {
                l.deadline = Instant::now() + Duration::from_secs(l.granted_ttl.max(1) as u64);
                l.granted_ttl
            }
            None => 0,
        }
    }
}

#[async_trait]
impl Lease for EtcdNode {

    async fn lease_grant(&self, request: Request<LeaseGrantRequest>) -> Result<Response<LeaseGrantResponse>, Status> {
        self.check_call(request.metadata(), None).await?;
        let (meta, _, r) = request.into_parts();
        // a replica of the leader's grant. [review 2026-11-21] never on the leader: a
        // call passing for a peer (no peer_token set) must not rewrite the leader's leases
        if has(&meta, XREP) {
            if self.is_leader().await {
                return Err(Status::failed_precondition(tr!("coord.replica_on_leader")));
            }
            let deadline = Instant::now() + Duration::from_secs(r.ttl.max(1) as u64);
            self.coord.leases.write().await.insert(r.id, Held { granted_ttl: r.ttl, deadline });
            return Ok(Response::new(LeaseGrantResponse { header: Some(self.response_header()), id: r.id, ttl: r.ttl, error: String::new() }));
        }
        if let Some(ch) = self.route_to_leader(has(&meta, XFWD)).await? {
            return LeaseClient::new(ch).lease_grant(self.peer_call(r, XFWD, Some(&meta)).await).await;
        }
        let (id, ttl) = self.grant_lease_here(r.id, r.ttl).await?;
        Ok(Response::new(LeaseGrantResponse { header: Some(self.response_header()), id, ttl, error: String::new() }))
    }

    async fn lease_revoke(&self, request: Request<LeaseRevokeRequest>) -> Result<Response<LeaseRevokeResponse>, Status> {
        self.check_call(request.metadata(), None).await?;
        // [review 2026-11-21] a revoke deletes the lease's keys: writing each of them is
        // needed, as etcd asks - else an account could delete keys it may not touch
        self.check_lease_keys(request.metadata(), request.get_ref().id, crate::coord::WRITE).await?;
        let (meta, _, r) = request.into_parts();
        if has(&meta, XREP) {
            // [review 2026-11-21] never on the leader; and the lease's keys go here too,
            // locally - a delete the leader's broadcast missed leaves none on a dead lease
            if self.is_leader().await {
                return Err(Status::failed_precondition(tr!("coord.replica_on_leader")));
            }
            self.coord.leases.write().await.remove(&r.id);
            self.drop_leased_keys(r.id).await;
            self.coord.changed.notify_waiters();
            return Ok(Response::new(LeaseRevokeResponse { header: Some(self.response_header()) }));
        }
        if let Some(ch) = self.route_to_leader(has(&meta, XFWD)).await? {
            return LeaseClient::new(ch).lease_revoke(self.peer_call(r, XFWD, Some(&meta)).await).await;
        }
        if !self.coord.leases.read().await.contains_key(&r.id) {
            return Err(Status::not_found(tr!("lease.not_found", id = r.id)));
        }
        self.revoke_lease(r.id).await?;
        Ok(Response::new(LeaseRevokeResponse { header: Some(self.response_header()) }))
    }

    type LeaseKeepAliveStream = ResponseStream;

    /// Each request renews its lease on the leader: the answer carries the granted TTL,
    /// 0 for a lease gone. Another node pipes the stream through to the leader; a
    /// leader that changes ends the stream, and the client opens it again.
    async fn lease_keep_alive(&self, request: Request<Streaming<LeaseKeepAliveRequest>>) -> LeaseKeepAliveResult<ResponseStream> {
        self.check_call(request.metadata(), None).await?;
        let (meta, _, mut incoming) = request.into_parts();
        if let Some(ch) = self.route_to_leader(has(&meta, XFWD)).await? {
            let (tx, rx) = tokio::sync::mpsc::channel::<LeaseKeepAliveRequest>(16);
            tokio::spawn(async move {
                while let Some(Ok(r)) = incoming.next().await {
                    if tx.send(r).await.is_err() {
                        break;
                    }
                }
            });
            let call = self.peer_call(tokio_stream::wrappers::ReceiverStream::new(rx), XFWD, Some(&meta)).await;
            let answers = LeaseClient::new(ch).lease_keep_alive(call).await?.into_inner();
            return Ok(Response::new(Box::pin(answers) as ResponseStream));
        }
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<LeaseKeepAliveResponse, Status>>(16);
        let node = self.clone();
        tokio::spawn(async move {
            while let Some(Ok(r)) = incoming.next().await {
                if !node.is_leader().await {
                    let _ = tx.send(Err(Status::unavailable(tr!("coord.not_leader")))).await;
                    break;
                }
                // [review 2026-11-21] keeping a lease alive keeps its keys: writing them is needed
                if let Err(e) = node.check_lease_keys(&meta, r.id, crate::coord::WRITE).await {
                    let _ = tx.send(Err(e)).await;
                    break;
                }
                let ttl = node.renew_lease_here(r.id).await;
                let a = LeaseKeepAliveResponse { header: Some(node.response_header()), id: r.id, ttl };
                if tx.send(Ok(a)).await.is_err() {
                    break;
                }
            }
        });
        Ok(Response::new(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)) as ResponseStream))
    }

    /// The lease's time left on the leader, and its keys when asked; TTL -1 for a lease
    /// gone, as etcd answers
    async fn lease_time_to_live(&self, request: Request<LeaseTimeToLiveRequest>) -> Result<Response<LeaseTimeToLiveResponse>, Status> {
        self.check_call(request.metadata(), None).await?;
        let (meta, _, r) = request.into_parts();
        // [review 2026-11-21] its keys' names only to one who may read them
        if r.keys {
            self.check_lease_keys(&meta, r.id, crate::coord::READ).await?;
        }
        if let Some(ch) = self.route_to_leader(has(&meta, XFWD)).await? {
            return LeaseClient::new(ch).lease_time_to_live(self.peer_call(r, XFWD, Some(&meta)).await).await;
        }
        let held = self.coord.leases.read().await.get(&r.id).cloned();
        let Some(l) = held else {
            return Ok(Response::new(LeaseTimeToLiveResponse { header: Some(self.response_header()), id: r.id, ttl: -1, granted_ttl: 0, keys: vec![] }));
        };
        let left = l.deadline.saturating_duration_since(Instant::now()).as_secs() as i64;
        let keys = if r.keys {
            let mut k: Vec<Vec<u8>> = self.vault.read().await.values().filter(|kv| kv.lease == r.id).map(|kv| kv.key.clone()).collect();
            k.sort();
            k
        } else {
            vec![]
        };
        Ok(Response::new(LeaseTimeToLiveResponse { header: Some(self.response_header()), id: r.id, ttl: left, granted_ttl: l.granted_ttl, keys }))
    }

    /// Every lease this node holds - the leader's, replicated to it. [review 2026-11-21]
    /// With authentication on, the role `root`'s: every account's lease ids
    async fn lease_leases(&self, request: Request<LeaseLeasesRequest>) -> Result<Response<LeaseLeasesResponse>, Status> {
        self.check_root(request.metadata()).await?;
        let mut ids: Vec<i64> = self.coord.leases.read().await.keys().copied().collect();
        ids.sort();
        Ok(Response::new(LeaseLeasesResponse { header: Some(self.response_header()), leases: ids.into_iter().map(|id| LeaseStatus { id }).collect() }))
    }
}
