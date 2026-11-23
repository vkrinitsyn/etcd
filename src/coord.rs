//! [coord] What one node decides for the whole cluster (owner, 2026-11-21): the leader,
//! and on it the leases and the locks.
//!
//! - **The leader.** `EtcdConfig::leader` names it - YT Cluster's ytserv sets its own Raft
//!   leader there, fenced by its term. Unset, it is the lowest node id among this node
//!   and the peers it sends its writes to (not Spare): each node agrees on it once they
//!   see the same peers, and a network split may have one on each side (etcds has no
//!   consensus of its own).
//! - **Leases** are granted, kept alive and expired on the leader alone, one clock. A
//!   grant and a revoke are replicated to every peer (a `LeaseGrant`/`LeaseRevoke` sent
//!   with [`XREP`]), so a new leader holds the same leases and gives each a fresh full
//!   TTL as it takes over, as etcd's does. A revoked or expired lease's keys are deleted
//!   through the ordinary delete, replicated as any.
//! - **Locks** are queued on the leader, first come first served. The holder's key -
//!   `name/<lease in hex>`, etcd's own layout - is put with its lease when the lock is
//!   granted, not while waiting, so the keys in the store are the holders: a new leader
//!   takes them over from there. The lease ending frees the lock.
//!
//! Another node sends a lease or lock call on to the leader with [`XFWD`]; a leader that
//! is not one any more answers `unavailable`, and the client asks again.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as SyncMutex};

use tokio::sync::{Notify, RwLock};
use tokio::time::{Duration, Instant};

use crate::cluster::NodeId;

/// a call another node sends on to the leader
pub const XFWD: &str = "x-etcds-fwd";
/// a lease the leader replicates to its peers
pub const XREP: &str = "x-etcds-rep";
/// [auth] the cluster's `peer_token`, on every call one node makes to another
pub const XPEER_TOKEN: &str = "x-etcds-peer-token";

/// [auth] what a call wants of a key range: read, write (signon's permission values)
pub const READ: i16 = 1;
pub const WRITE: i16 = 2;

/// the fewest seconds a lease lives, as etcd's minimum TTL
pub const MIN_TTL: i64 = 5;

/// how often the leader looks for expired leases
pub(crate) const TICK: Duration = Duration::from_millis(250);

/// A lease as this node holds it: granted here as the leader, or replicated here
#[derive(Debug, Clone)]
pub(crate) struct Lease {
    pub(crate) granted_ttl: i64,
    /// when it expires - kept on the leader; a replica's is set as it takes over
    pub(crate) deadline: Instant,
}

/// One lock name's holder and the tickets waiting for it, in order
#[derive(Debug, Default)]
pub(crate) struct LockQueue {
    pub(crate) holder: Option<(Vec<u8>, i64)>,
    pub(crate) waiting: VecDeque<u64>,
}

/// The cluster-wide state this node keeps: the leases (every node, replicated), the
/// locks (the leader's), the alarms, and whether it was the leader at the last tick
pub(crate) struct Coord {
    pub(crate) leases: RwLock<HashMap<i64, Lease>>,
    pub(crate) locks: SyncMutex<HashMap<Vec<u8>, LockQueue>>,
    /// a lease or a lock changed: the waiters look again
    pub(crate) changed: Notify,
    pub(crate) alarms: SyncMutex<Vec<crate::etcdpb::etcdserverpb::AlarmMember>>,
    pub(crate) was_leader: AtomicBool,
    /// one takeover at a time: a call and the tick may both see this node lead first
    pub(crate) takeover: tokio::sync::Mutex<()>,
    ticket: AtomicU64,
    /// [embed] the leader the embedder names (`set_leader`): over the configured one, and
    /// kept across a reconfigure
    pub(crate) named: SyncMutex<Option<NodeId>>,
}

impl Coord {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Coord {
            leases: RwLock::new(HashMap::new()),
            locks: SyncMutex::new(HashMap::new()),
            changed: Notify::new(),
            alarms: SyncMutex::new(Vec::new()),
            was_leader: AtomicBool::new(false),
            takeover: tokio::sync::Mutex::new(()),
            ticket: AtomicU64::new(1),
            named: SyncMutex::new(None),
        })
    }

    pub(crate) fn next_ticket(&self) -> u64 {
        self.ticket.fetch_add(1, Ordering::Relaxed)
    }
}

/// The leader: `named` (a node uuid's id, from `EtcdConfig::leader`) when given, else
/// the lowest of this node's id and its Online peers' ids
pub(crate) fn leader_of(named: Option<NodeId>, me: NodeId, online: &[NodeId]) -> NodeId {
    named.unwrap_or_else(|| online.iter().copied().chain(std::iter::once(me)).min().unwrap_or(me))
}

/// A lock's holder key: `name/` and the lease in lowercase hex, as etcd's
pub(crate) fn lock_key(name: &[u8], lease: i64) -> Vec<u8> {
    let mut k = name.to_vec();
    k.push(b'/');
    k.extend_from_slice(format!("{:x}", lease).as_bytes());
    k
}

/// The lock name a holder key is of, when `key` ends in `/` and `lease` in hex - how a
/// new leader tells the lock holders among the leased keys it takes over
/// [review 2026-11-21] the holders are told by their records now; the layout is tested
#[cfg(test)]
pub(crate) fn lock_name_of(key: &[u8], lease: i64) -> Option<Vec<u8>> {
    let suffix = format!("/{:x}", lease);
    key.strip_suffix(suffix.as_bytes()).filter(|n| !n.is_empty()).map(|n| n.to_vec())
}

/// A waiting `Lock` call's place in its queue: taken out when the call ends however it
/// ends - granted, refused, or its client gone, which drops the call mid-wait
pub(crate) struct Ticket {
    pub(crate) coord: Arc<Coord>,
    pub(crate) name: Vec<u8>,
    pub(crate) id: u64,
}

impl Drop for Ticket {
    fn drop(&mut self) {
        let mut locks = self.coord.locks.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(q) = locks.get_mut(&self.name) {
            q.waiting.retain(|t| *t != self.id);
            if q.holder.is_none() && q.waiting.is_empty() {
                locks.remove(&self.name);
            }
        }
        drop(locks);
        self.coord.changed.notify_waiters();
    }
}

// ---- on the node ---------------------------------------------------------------

use tonic::metadata::{MetadataMap, MetadataValue};
use tonic::transport::Channel;
use tonic::{Request, Status};

use crate::cluster::EtcdNode;
use crate::peer::PeerState;

/// whether a peer marked the call `mark` ([`XFWD`], [`XREP`]): a client's mark is no
/// mark - the call is a peer's only with `x-peer`, which `guard::peer_proof` checks
/// against the cluster's token before anything else
pub(crate) fn has(meta: &MetadataMap, mark: &str) -> bool {
    meta.get(crate::etcdpb::XPEER).is_some() && meta.get(mark).is_some()
}

impl EtcdNode {
    /// [coord] The leader's node id: `EtcdConfig::leader` when set, else the lowest of
    /// this node's id and those of the peers it sends its writes to - the ones not Spare,
    /// as a broadcast's: the binary never promotes a peer past InProgress, an embedder
    /// does (ytserv's etcd_peers)
    pub(crate) async fn leader(&self) -> NodeId {
        // a leader that is no uuid is refused at start and at a reconfigure
        let embedder = *self.coord.named.lock().unwrap_or_else(|p| p.into_inner());
        let named = match embedder {
            Some(id) => Some(id),
            None => {
                let l = self.cfg.read().await.leader.clone();
                if l.trim().is_empty() { None } else { crate::cluster::parse_uuid(&l).ok() }
            }
        };
        let online: Vec<NodeId> = self.peers.read().await.peer_info().await.into_iter()
            .filter(|(_, _, s)| *s != PeerState::Spare)
            .map(|(id, _, _)| id)
            .collect();
        leader_of(named, self.node_id, &online)
    }

    pub(crate) async fn is_leader(&self) -> bool {
        self.leader().await == self.node_id
    }

    /// [coord] Where a leader's call goes: `None` when this node is the leader, else the
    /// leader's channel. A call sent on here already (`fwd`) is not sent on again: a
    /// node that is not the leader any more answers `unavailable`, and the client asks
    /// again. So is a leader this node holds no peer for, or only a Spare one.
    pub(crate) async fn route_to_leader(&self, fwd: bool) -> Result<Option<Channel>, Status> {
        let leader = self.leader().await;
        if leader == self.node_id {
            // a node that has just become the leader takes over before it answers, not
            // at its next tick: a lock it does not know the holder of yet would be granted
            self.took_over().await?;
            return Ok(None);
        }
        if fwd {
            return Err(Status::unavailable(tr!("coord.not_leader")));
        }
        let peers = self.peers.read().await;
        if let Some(p) = peers.handle(leader).await {
            let p = p.lock().await;
            if p.state != PeerState::Spare {
                return Ok(Some(p.channel.clone()));
            }
        }
        Err(Status::unavailable(tr!("coord.leader_down", id = leader)))
    }

    /// [coord] `msg` as a call from this node to a peer, marked `mark` ([`XFWD`] or
    /// [`XREP`]), with the cluster's `peer_token` and - sent on for a client - the
    /// client's own token
    pub(crate) async fn peer_call<T>(&self, msg: T, mark: &str, client: Option<&MetadataMap>) -> Request<T> {
        let mut r = Request::new(msg);
        let m = r.metadata_mut();
        if let Ok(v) = MetadataValue::try_from(self.node_id.to_string()) {
            m.insert(crate::etcdpb::XPEER, v);
        }
        if let Ok(k) = tonic::metadata::MetadataKey::from_bytes(mark.as_bytes()) {
            m.insert(k, MetadataValue::from_static("1"));
        }
        let token = self.cfg.read().await.peer_token.clone();
        if !token.is_empty()
            && let Ok(v) = MetadataValue::try_from(token) {
                m.insert(crate::coord::XPEER_TOKEN, v);
            }
        if let Some(c) = client
            && let Some(t) = c.get("token") {
                m.insert("token", t.clone());
            }
        r
    }

    /// [coord] The leader's clock, every [`TICK`]: on becoming the leader it takes the
    /// leases and locks over - every lease a fresh full TTL, the lock holders from the
    /// leased keys - and while it is one, it revokes the leases past their deadline.
    /// A node that stops being the leader drops its lock queues: their waiters are told
    /// to ask again, the holders' keys stay.
    pub(crate) fn spawn_coord_tick(&self) {
        let node = self.clone();
        tokio::spawn(async move {
            let mut every = tokio::time::interval(TICK);
            every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                every.tick().await;
                if node.event.is_closed() {
                    break;
                }
                let leader = node.is_leader().await;
                if leader {
                    let _ = node.took_over().await;
                } else if node.coord.was_leader.swap(false, Ordering::Relaxed) {
                    node.coord.locks.lock().unwrap_or_else(|p| p.into_inner()).clear();
                    node.coord.changed.notify_waiters();
                    // [sys] the next leader's to answer; what this one held goes to it
                    node.seq_consumer_stop().await;
                }
                if leader && node.coord.was_leader.load(Ordering::Relaxed) {
                    let now = Instant::now();
                    let expired: Vec<i64> = node.coord.leases.read().await.iter()
                        .filter(|(_, l)| l.deadline <= now)
                        .map(|(id, _)| *id)
                        .collect();
                    for id in expired {
                        // [review 2026-11-21] one not revoked (its record not written) is
                        // tried again at the next tick, still past its deadline
                        match node.revoke_lease(id).await {
                            Ok(n) => slog::info!(node.log, "{}{}", crate::LP, tr!("lease.expired", id = id, keys = n)),
                            Err(e) => slog::warn!(node.log, "{}{}", crate::LP, tr!("sys.not_written", key = crate::sys::lease_key(id), err = e.message())),
                        }
                    }
                }
            }
        });
    }

    /// [coord] Takes over, once, as this node becomes the leader: by the first call it
    /// leads or by its tick, whichever comes first
    ///
    /// [review 2026-11-21] A takeover that cannot read the records - the store down, no
    /// majority answering - is not one: this node answers `unavailable` and tries again at
    /// its next tick, rather than lead from nothing.
    pub(crate) async fn took_over(&self) -> Result<(), Status> {
        let _one = self.coord.takeover.lock().await;
        if !self.coord.was_leader.load(Ordering::Relaxed) {
            if let Err(e) = self.take_over().await {
                slog::error!(self.log, "{}{}", crate::LP, tr!("sys.not_taken_over", err = e.message()));
                return Err(Status::unavailable(tr!("coord.taking_over", err = e.message())));
            }
            self.coord.was_leader.store(true, Ordering::Relaxed);
        }
        Ok(())
    }

    /// [coord] This node became the leader: [sys] it goes from the system records - the
    /// embedder's store, or the map replicated to it - each lease living a full TTL from
    /// now, each lock held by the holder its record names while that lease lives
    async fn take_over(&self) -> Result<(), Status> {
        let now = Instant::now();
        let all = self.sys_load().await?;
        let loaded = crate::sys::read_records(all.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        let leases: HashMap<i64, i64> = {
            let mut l = self.coord.leases.write().await;
            // [review 2026-11-21] the records decide: a lease revoked by the leader before
            // (its record gone) is not this one's to keep
            l.retain(|id, _| loaded.leases.contains_key(id));
            for (id, ttl) in &loaded.leases {
                l.entry(*id).or_insert(Lease { granted_ttl: *ttl, deadline: now });
            }
            for v in l.values_mut() {
                v.deadline = now + Duration::from_secs(v.granted_ttl.max(1) as u64);
            }
            l.iter().map(|(id, v)| (*id, v.granted_ttl)).collect()
        };
        let mut locks = self.coord.locks.lock().unwrap_or_else(|p| p.into_inner());
        locks.clear();
        for (name, (key, lease)) in loaded.locks {
            if leases.contains_key(&lease) {
                locks.entry(name).or_default().holder = Some((key, lease));
            }
        }
        drop(locks);
        slog::info!(self.log, "{}{}", crate::LP, tr!("coord.took_over", leases = leases.len()));
        self.coord.changed.notify_waiters();
        Ok(())
    }

    /// [coord] Ends lease `id` on the leader: forgets it, deletes its keys (replicated as
    /// any delete), frees the locks it held, and has every peer forget it. The number
    /// of keys deleted; 0 for a lease already gone.
    pub(crate) async fn revoke_lease(&self, id: i64) -> Result<usize, Status> {
        if !self.coord.leases.read().await.contains_key(&id) {
            return Ok(0);
        }
        // [sys] the record first: a leader after this one must not bring the lease back.
        // [review 2026-11-21] Not written, nothing is revoked - the caller is told
        self.sys_write(&crate::sys::lease_key(id), None).await?;
        if self.coord.leases.write().await.remove(&id).is_none() {
            return Ok(0);
        }
        let keys: Vec<Vec<u8>> = self.vault.read().await.values()
            .filter(|kv| kv.lease == id)
            .map(|kv| kv.key.clone())
            .collect();
        for k in &keys {
            if let Err(e) = self.kv_delete(k.clone()).await {
                slog::warn!(self.log, "{}{}", crate::LP, tr!("lease.key_not_deleted", id = id, err = e.message()));
            }
        }
        // [review 2026-11-21] a delete that failed to replicate still leaves nothing here;
        // the peers drop theirs with the revoke's replica
        self.drop_leased_keys(id).await;
        let freed: Vec<Vec<u8>> = {
            let mut locks = self.coord.locks.lock().unwrap_or_else(|p| p.into_inner());
            let mut freed = Vec::new();
            for (name, q) in locks.iter_mut() {
                if q.holder.as_ref().is_some_and(|(_, l)| *l == id) {
                    q.holder = None;
                    freed.push(name.clone());
                }
            }
            locks.retain(|_, q| q.holder.is_some() || !q.waiting.is_empty());
            freed
        };
        for name in freed {
            let _ = self.sys_write(&crate::sys::lock_rec_key(&name), None).await;
        }
        self.coord.changed.notify_waiters();
        self.replicate_lease(id, None).await;
        Ok(keys.len())
    }

    /// [review 2026-11-21] The checks of a call wanting `want` on every key lease `id`
    /// holds here: a lease's keys are what its revoke deletes and its keep-alive keeps
    pub(crate) async fn check_lease_keys(&self, meta: &MetadataMap, id: i64, want: i16) -> Result<(), Status> {
        let keys: Vec<Vec<u8>> = self.vault.read().await.values().filter(|kv| kv.lease == id).map(|kv| kv.key.clone()).collect();
        for k in &keys {
            self.check_call(meta, Some((k, &[], want))).await?;
        }
        Ok(())
    }

    /// [review 2026-11-21] The keys of lease `id` taken out of this node's store, not
    /// broadcast: what a replica of a revoke does, and what a revoke does for a key whose
    /// delete failed - none is left behind on a dead lease
    pub(crate) async fn drop_leased_keys(&self, id: i64) {
        let gone: Vec<crate::kv::Kv> = {
            let mut vault = self.vault.write().await;
            let keys: Vec<Vec<u8>> = vault.values().filter(|kv| kv.lease == id).map(|kv| kv.key.clone()).collect();
            keys.iter().filter_map(|k| vault.remove(k)).collect()
        };
        for kv in gone {
            let _ = self.watch_notify.send((kv, true)).await;
        }
    }

    /// [coord] Has every peer this node writes to (not Spare) hold lease `id` (`Some(ttl)`) or forget it
    /// (`None`), each answer awaited a little: the grant answers once its peers hold it,
    /// so a put with it right after, on any node, finds it. A peer that does not answer
    /// is said and left: it takes the leases over from a peer's sync when it is back.
    pub(crate) async fn replicate_lease(&self, id: i64, ttl: Option<i64>) {
        use crate::etcdpb::etcdserverpb::lease_client::LeaseClient;
        use crate::etcdpb::etcdserverpb::{LeaseGrantRequest, LeaseRevokeRequest};
        let targets: Vec<(NodeId, Channel)> = {
            let peers = self.peers.read().await;
            let mut v = Vec::new();
            for (pid, _, state) in peers.peer_info().await {
                if state != PeerState::Spare
                    && let Some(h) = peers.handle(pid).await {
                        v.push((pid, h.lock().await.channel.clone()));
                    }
            }
            v
        };
        let mut calls = Vec::new();
        for (pid, ch) in targets {
            let node = self.clone();
            calls.push(tokio::spawn(async move {
                let mut c = LeaseClient::new(ch);
                let sent = match ttl {
                    Some(ttl) => {
                        let r = node.peer_call(LeaseGrantRequest { ttl, id }, XREP, None).await;
                        tokio::time::timeout(Duration::from_secs(2), c.lease_grant(r)).await.map(|r| r.map(|_| ()))
                    }
                    None => {
                        let r = node.peer_call(LeaseRevokeRequest { id }, XREP, None).await;
                        tokio::time::timeout(Duration::from_secs(2), c.lease_revoke(r)).await.map(|r| r.map(|_| ()))
                    }
                };
                let err = match sent {
                    Ok(Ok(())) => return,
                    Ok(Err(e)) => e.message().to_string(),
                    Err(_) => "timeout".to_string(),
                };
                slog::warn!(node.log, "{}{}", crate::LP, tr!("lease.not_replicated", id = id, peer = pid, err = err));
            }));
        }
        for c in calls {
            let _ = c.await;
        }
    }
}

#[cfg(test)]
#[path = "coord_test.rs"]
mod tests;
