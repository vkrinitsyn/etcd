use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;
use slog::*;
use tokio::{
    sync::Mutex,
    sync::mpsc::{channel},
    sync::RwLock,
    time::Instant,
};
use tonic::Status;
use tonic::metadata::MetadataValue;
use tonic::transport::{Channel, Endpoint};
use crate::{etcdpb::etcdserverpb::maintenance_client::MaintenanceClient, etcdpb::etcdserverpb::kv_client::KvClient, KvEvent, cluster::{EtcdPeerNodeType, KvKey, NodeId}, LP};
use crate::cli::EtcdConfig;
use crate::etcdpb::etcdserverpb::{cluster_client::ClusterClient, MemberAddRequest, RangeRequest, StatusRequest};
use crate::kv::Kv;

const RECENT_WINDOW_SECS: u64 = 30;

/// [lockup] The longest one peer call may take - a `status` while dialling, a
/// put, delete or txn while replicating. A peer that never answers is a failed
/// call, not a wait: before this the calls had no limit, and a broadcast sets
/// its adaptive deadline only once most `Online` peers replied - never while
/// every peer is still `InProgress`, which is every peer after a start.
pub(crate) const PEER_RPC_TIMEOUT: Duration = Duration::from_secs(5);

/// represent cluster structure
/// hold configs and capable to update
#[derive(Clone)]
pub struct EtcdCluster {
    /// cluster nodes (not clients, see node watchers for clients)
    peers: Vec<EtcdPeerNodeType>,
    /// this node's own zone, for comparing against a peer's
    my_zone: String,
    /// 0 means no timeout
    connect_timeout_ms: u64,
    /// track recently added peer_ids to prevent rapid re-addition
    recently_added: HashMap<NodeId, Instant>,

    node_id: NodeId,
    cluster_id: NodeId,
    log: Logger,
}

/// Lifecycle state of a peer node in the cluster.
/// Spare → InProgress → Online, or Online → Spare on timeout/instability.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PeerState {
    /// Connected but not participating in broadcasts.
    /// Newly added nodes or nodes demoted due to timeout/instability.
    Spare,
    /// Syncing data from the cluster (learner).
    /// Receives writes but its acknowledgment is not counted for quorum.
    InProgress,
    /// Fully operational, participates in broadcast quorum.
    Online,
}

#[allow(dead_code)]
pub(crate) struct EtcdPeerNode {
    peer_id: NodeId,
    pub(crate) conn: String,
    /// This peer's zone, from `peer_zones`. Empty means unzoned.
    pub(crate) zone: String,
    pub(crate) state: PeerState,
    added_at: Instant,
    pub(crate) kv_client: Arc<Mutex<KvClient<Channel>>>,
    mt_client: Arc<Mutex<MaintenanceClient<Channel>>>,
    cluster_client: Arc<Mutex<ClusterClient<Channel>>>,
}

/// request and corresponding api calls to perform a broadcast
#[derive(Clone)]
pub(crate) enum BroadcastRequest {
    Kv(KvEvent),
    // Lease?
}


/// The host part of a peer URL, for matching against a `peer_zones` entry.
///
/// Entries may be written `host:port` or bare `host`, and a peer's `conn` is a
/// URL — so both sides are reduced to the same shape before comparing rather
/// than requiring an operator to guess the stored form.
fn host_key(s: &str) -> String {
    let s = s.trim();
    let s = s.strip_prefix("http://").or_else(|| s.strip_prefix("https://")).unwrap_or(s);
    s.split('/').next().unwrap_or(s).trim().to_string()
}

/// Look up a peer's zone in a `host:port=zone,...` list.
///
/// Matches on `host:port` first, then on the bare host — so one entry can
/// cover a node whose port an operator did not write down, without a
/// host-only entry silently shadowing a more specific one.
fn zone_of(conn: &str, peer_zones: &str) -> String {
    let want = host_key(conn);
    let bare = want.rsplit_once(':').map(|(h, _)| h.to_string()).unwrap_or_else(|| want.clone());
    let mut fallback = String::new();
    for e in peer_zones.split(',') {
        let Some((h, z)) = e.split_once('=') else { continue };
        let h = host_key(h);
        if h == want {
            return z.trim().to_string();
        }
        if h == bare {
            fallback = z.trim().to_string();
        }
    }
    fallback
}

impl EtcdCluster {
    /// This node's zone, as last configured.
    pub(crate) fn my_zone(&self) -> &str { &self.my_zone }

    /// Re-label every peer from configuration.
    ///
    /// Called wherever the config is applied. The etcd member protocol carries
    /// no zone, and extending it would break API compatibility, so the labels
    /// come from the embedder — which already knows the topology.
    pub(crate) fn apply_zones(&mut self, my_zone: &str, peer_zones: &str) {
        self.my_zone = my_zone.trim().to_string();
        for p in &self.peers {
            if let Ok(mut n) = p.try_lock() {
                n.zone = zone_of(&n.conn.clone(), peer_zones);
            }
        }
    }

    /// send request to the clusters peer and get success response from more than half nodes 
    pub(crate) async fn connect(cfg: &EtcdConfig, node_id: NodeId, cluster_id: NodeId, log: &Logger) -> std::result::Result<Self, String> {
        let timeout_ms = cfg.election_timeout;


        let mut cluster = EtcdCluster {
            peers: vec![],
            my_zone: cfg.zone.clone(),
            connect_timeout_ms: timeout_ms as u64,
            recently_added: HashMap::new(),
            node_id,
            cluster_id,
            log: log.clone(),
        };
        
        let peers = cfg.peers();
        let half = peers.len() as f32 / 2f32;
        let input_size = peers.len();
        let connected = cluster.add_connections(peers).await?;
        if input_size == 0 || connected as f32 > half {
            Ok(cluster)
        } else {
            Err(format!("cant connect to more than half peers [{}/{}]", connected, input_size))
        }
    }
    
    /// [peer-retry] How many peers are currently connected.
    pub(crate) fn connected(&self) -> usize {
        self.peers.len()
    }

    /// [lockup] The urls in `clients` this node does not hold a peer for yet.
    pub(crate) async fn not_held(&self, mut clients: HashSet<&str>) -> Vec<String> {
        for p in &self.peers {
            clients.remove(p.lock().await.conn.as_str());
        }
        clients.into_iter().filter(|u| u.starts_with("http")).map(|u| u.to_string()).collect()
    }

    /// [lockup] What `dial` needs, so it can run with the peer lock released.
    pub(crate) fn dial_params(&self) -> (NodeId, u64, Logger) {
        (self.cluster_id, self.connect_timeout_ms, self.log.clone())
    }

    /// [lockup] Connect to each url and ask its `status`: a peer is one that
    /// answers with this cluster's id.
    ///
    /// Network only, and it takes no lock - a caller must not hold the peer lock
    /// around it either. Each url is a connect and a round trip, and while a
    /// writer holds the lock every broadcast, put and member list on this node
    /// waits behind it. `add_connections` used to do exactly this inside
    /// `peers.write()`, with no limit on `status`.
    pub(crate) async fn dial(urls: Vec<String>, cluster_id: NodeId, connect_timeout_ms: u64, log: &Logger)
        -> Vec<(NodeId, String, Channel, MaintenanceClient<Channel>)>
    {
        let connect_timeout_ms = if connect_timeout_ms > 0 { connect_timeout_ms } else { 1000 };
        let mut found = Vec::new();
        for url in urls {
            let endpoint = match Endpoint::from_str(&url) {
                Ok(e) => e.connect_timeout(Duration::from_millis(connect_timeout_ms)),
                Err(e) => {
                    error!(log, "{}making endpoint to {} with error {}", LP, url, e);
                    continue;
                }
            };
            let conn = match endpoint.connect().await {
                Ok(c) => c,
                Err(e) => {
                    error!(log, "{}connecting endpoint {} with error {}", LP, url, e);
                    continue;
                }
            };
            let mut mt = MaintenanceClient::new(conn.clone());
            let status = match tokio::time::timeout(PEER_RPC_TIMEOUT, mt.status(StatusRequest::default())).await {
                Ok(Ok(s)) => s.into_inner(),
                Ok(Err(e)) => {
                    error!(log, "{}connecting maintenance {} with error {}", LP, url, e);
                    continue;
                }
                Err(_) => {
                    error!(log, "{}connecting maintenance {}: no answer in {:?}", LP, url, PEER_RPC_TIMEOUT);
                    continue;
                }
            };
            match status.header {
                None => error!(log, "{}connecting maintenance {} - no header in response", LP, url),
                Some(h) if h.cluster_id != cluster_id => error!(log, "{}connecting maintenance {} - wrong cluster,\
                    running on ClusterID [{}], but connecting node from {}", LP, url, cluster_id, h.cluster_id),
                Some(h) => found.push((h.member_id, url, conn, mt)),
            }
        }
        found
    }

    /// [lockup] Hold the dialled peers this node does not hold yet - by peer id,
    /// since one node can be reached under more than one url. Short and free of
    /// network waits: the only part of adding a peer that needs the lock.
    pub(crate) async fn admit(&mut self, dialled: Vec<(NodeId, String, Channel, MaintenanceClient<Channel>)>) -> usize {
        self.recently_added.retain(|_, t| t.elapsed() < Duration::from_secs(RECENT_WINDOW_SECS));
        let mut cnt = 0;
        for (member_id, url, conn, mt) in dialled {
            let mut exists = false;
            for p in &self.peers {
                if p.lock().await.peer_id == member_id {
                    info!(self.log, "{}peer_id {} already connected, skipping {}", LP, member_id, url);
                    exists = true;
                    break;
                }
            }
            if exists { continue; }
            if let Some(t) = self.recently_added.get(&member_id) {
                if t.elapsed() < Duration::from_secs(RECENT_WINDOW_SECS) {
                    info!(self.log, "{}peer_id {} recently added ({:?} ago), skipping {}", LP, member_id, t.elapsed(), url);
                    continue;
                }
            }
            self.recently_added.insert(member_id, Instant::now());
            self.peers.push(Arc::new(Mutex::new(EtcdPeerNode {
                peer_id: member_id,
                conn: url,
                // Unzoned until `apply_zones` labels it on
                // the next reconfigure. That window is
                // fail-safe: on a zoned node an unlabelled
                // peer compares unequal to my zone, so it
                // receives no zone-scoped key until it is
                // known to share one.
                zone: String::new(),
                // NOT `Spare`. A peer reaches this line
                // only after it answered `status` with a
                // cluster id matching ours, so it is a
                // verified member of this cluster - and
                // `broadcast_scoped` skips `Spare`
                // entirely, so a connected peer left
                // `Spare` receives nothing, forever.
                //
                // The only thing that ever promoted one
                // was the `member_promote` gRPC admin
                // call, which ytserv never makes: every
                // peer connected, every put succeeded
                // locally, `send_indices` was empty so
                // the broadcast returned `Ok(())`, and
                // the kv was per-node with nothing
                // logged anywhere.
                //
                // `InProgress` is the state that means
                // "receives writes, does not yet gate the
                // commit" (`quorum_total` counts only
                // `Online`), which is exactly right for a
                // peer that is connected but has not yet
                // proven it can replicate. The first
                // successful broadcast promotes it to
                // `Online`, mirroring the demote-on-
                // timeout below it.
                state: PeerState::InProgress,
                added_at: Instant::now(),
                kv_client: Arc::new(Mutex::new(KvClient::new(conn.clone()))),
                mt_client: Arc::new(Mutex::new(mt)),
                cluster_client: Arc::new(Mutex::new(ClusterClient::new(conn))),
            })));
            cnt += 1;
        }
        cnt
    }

    /// Dial `clients` and hold the ones that answered, in one go - for `connect`
    /// at start, and for tests. A running node adds peers through
    /// `EtcdNode::add_peers`, which keeps the lock off the network.
    pub(crate) async fn add_connections(&mut self, clients: HashSet<&str>) -> std::result::Result<usize, String> {
        let urls = self.not_held(clients).await;
        let dialled = Self::dial(urls, self.cluster_id, self.connect_timeout_ms, &self.log).await;
        Ok(self.admit(dialled).await)
    }

    /// [lockup] The peers one broadcast goes to: chosen under the peer lock, sent
    /// to after it is released (`BroadcastPlan::send`, via `EtcdNode::broadcast_scoped`).
    ///
    /// Optionally confined to peers sharing this node's zone.
    ///
    /// `zone_scoped` is decided by the caller from the key's prefix, because
    /// the key is what carries the policy — a peer cannot be asked whether it
    /// should receive something.
    ///
    /// **A peer whose zone is unknown is excluded** from a zone-scoped
    /// broadcast whenever this node is zoned. That is the fail-safe direction:
    /// a peer added since the last reconfigure is unlabelled, and sending it a
    /// zone-scoped key on the assumption that no label means "same zone" is
    /// exactly the leak zoning exists to prevent.
    pub(crate) async fn plan(&self, zone_scoped: Option<&str>) -> BroadcastPlan {
        let mut targets = Vec::new();
        for p in self.peers.iter() {
            let n = p.lock().await;
            if let Some(my_zone) = zone_scoped {
                if n.zone.trim() != my_zone.trim() {
                    continue;
                }
            }
            match n.state {
                PeerState::Online => targets.push((p.clone(), true)),
                PeerState::InProgress => targets.push((p.clone(), false)),
                PeerState::Spare => {}
            }
        }
        BroadcastPlan { targets, node_id: self.node_id, log: self.log.clone() }
    }

    /// One request to one peer.
    ///
    /// [q-route Phase 4] **The client is CLONED and both locks released before
    /// the call.** It used to hold `peer.lock()` *and* `kv_client.lock()` for
    /// the whole round trip, so every concurrent message to the same peer
    /// serialized on them — and a hot p2p pair is exactly the case that sends
    /// concurrent messages to one peer. The outer lock was the worse of the
    /// two: it also blocked every *state* read about that peer, so a slow RPC
    /// stalled `is_online`, `unicast` and `broadcast_scoped` for it.
    ///
    /// Cloning is cheap and correct: a tonic client is a thin handle over a
    /// `Channel`, which multiplexes concurrent HTTP/2 streams. This removes a
    /// serialization point, not an ordering guarantee — the queue's ordering
    /// comes from `idx`, assigned by the dispatcher, never from the order two
    /// peer RPCs happen to complete in.
    ///
    /// [lockup] Bounded by `PEER_RPC_TIMEOUT`: a peer that never answers is a
    /// failed send, which the broadcast then counts as one.
    pub(crate) async fn peer_request(request: BroadcastRequest, peer: EtcdPeerNodeType, peer_id: Option<MetadataValue<tonic::metadata::Ascii>>) -> bool {
        let mut kv = {
            let node = peer.lock().await;
            let c = node.kv_client.lock().await.clone();
            c
        };
        let call = async move {
            match request {
                BroadcastRequest::Kv(br) => {
                    match br {
                        KvEvent::Put(kr) => kv.put(kr, peer_id).await.is_ok(),
                        KvEvent::Delete(kr) => kv.delete_range(kr, peer_id).await.is_ok(),
                        KvEvent::Txn(kr) => kv.txn(kr, peer_id).await.is_ok(),
                    }
                }
            }
        };
        tokio::time::timeout(PEER_RPC_TIMEOUT, call).await.unwrap_or(false)
    }

    /// Poll the shared deadline until it's set, then sleep until it expires.
    async fn await_deadline(deadline_ms: &AtomicU16, start: Instant) {
        loop {
            let ms = deadline_ms.load(Ordering::Relaxed);
            if ms > 0 {
                let deadline = Duration::from_millis(ms as u64);
                let elapsed = start.elapsed();
                if elapsed >= deadline {
                    return;
                }
                tokio::time::sleep(deadline - elapsed).await;
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
    
    /// Promote a Spare peer to InProgress (start syncing).
    pub(crate) async fn promote_to_syncing(&self, peer_id: NodeId, log: &Logger) -> bool {
        for p in &self.peers {
            let mut node = p.lock().await;
            if node.peer_id == peer_id && node.state == PeerState::Spare {
                info!(log, "{}peer {} ({}) promoted to InProgress", LP, node.conn, peer_id);
                node.state = PeerState::InProgress;
                return true;
            }
        }
        false
    }

    /// Promote an InProgress peer to Online (fully synced).
    pub(crate) async fn promote_to_online(&self, peer_id: NodeId, log: &Logger) -> bool {
        for p in &self.peers {
            let mut node = p.lock().await;
            if node.peer_id == peer_id && node.state == PeerState::InProgress {
                info!(log, "{}peer {} ({}) promoted to Online", LP, node.conn, peer_id);
                node.state = PeerState::Online;
                return true;
            }
        }
        false
    }

    /// Demote a peer back to Spare.
    #[allow(dead_code)] // part of the peer state machine, not wired up yet
    pub(crate) async fn demote_to_spare(&self, peer_id: NodeId, log: &Logger) -> bool {
        for p in &self.peers {
            let mut node = p.lock().await;
            if node.peer_id == peer_id && node.state != PeerState::Spare {
                info!(log, "{}peer {} ({}) demoted to Spare", LP, node.conn, peer_id);
                node.state = PeerState::Spare;
                return true;
            }
        }
        false
    }

    /// Return peer state by id.
    pub(crate) async fn peer_state(&self, peer_id: NodeId) -> Option<PeerState> {
        for p in &self.peers {
            let node = p.lock().await;
            if node.peer_id == peer_id {
                return Some(node.state);
            }
        }
        None
    }

    /// Number of Online peers.
    #[allow(dead_code)] // part of the peer state machine, not wired up yet
    pub(crate) async fn online_count(&self) -> usize {
        let mut count = 0;
        for p in &self.peers {
            if p.lock().await.state == PeerState::Online {
                count += 1;
            }
        }
        count
    }

    // ─── [q-route] unicast, for the queue routing in queue-p2p-route.md ─────

    /// This node's own id, so a caller can tell `Local` from `Remote`.
    pub(crate) fn me(&self) -> NodeId { self.node_id }

    /// Is this peer `Online`, and therefore usable as a dispatcher?
    ///
    /// `InProgress` deliberately does **not** count. Such a peer receives
    /// writes but is still syncing, and a dispatcher has to be able to *index*
    /// and *route*, not merely store — electing one that is still catching up
    /// puts the queue's ordering behind its recovery.
    pub(crate) async fn is_online(&self, peer_id: NodeId) -> bool {
        for p in &self.peers {
            let n = p.lock().await;
            if n.peer_id == peer_id {
                return n.state == PeerState::Online;
            }
        }
        false
    }

    /// [lockup] The peer a unicast goes to, chosen under the peer lock -
    /// `EtcdNode::unicast` sends after releasing it.
    ///
    /// **Applies the same zone test as `broadcast_scoped`, and that is not
    /// optional.** A unicast is still a write leaving this node, so a
    /// zone-scoped key must not reach a peer in another zone — or one whose
    /// zone is *unknown*, which is the fail-safe direction: a peer added since
    /// the last reconfigure is unlabelled, and treating "no label" as "same
    /// zone" is the leak zoning exists to prevent. Being targeted rather than
    /// broadcast changes nothing about that.
    ///
    /// `None` when the peer is not held, not Online, or excluded by the zone test
    /// — the caller falls back to a broadcast and says so, rather than silently
    /// dropping the message.
    pub(crate) async fn unicast_target(&self, peer_id: NodeId, zone_scoped: Option<&str>) -> Option<EtcdPeerNodeType> {
        for p in &self.peers {
            let n = p.lock().await;
            if n.peer_id != peer_id {
                continue;
            }
            if n.state != PeerState::Online {
                return None;
            }
            if let Some(my_zone) = zone_scoped {
                if n.zone.trim() != my_zone.trim() {
                    return None;
                }
            }
            return Some(p.clone());
        }
        None
    }

    /// Announce this node to all connected peers and sync KV data from the fastest peer.
    pub(crate) async fn announce_and_sync(
        &self,
        my_client_urls: Vec<String>,
        vault: &Arc<RwLock<HashMap<KvKey, Kv>>>,
        log: &Logger,
    ) -> std::result::Result<usize, String> {
        if self.peers.is_empty() {
            return Ok(0);
        }

        // Phase A: announce self to all peers (best-effort)
        let add_req = MemberAddRequest {
            peer_ur_ls: my_client_urls,
            is_learner: true,
        };
        for p in &self.peers {
            let node = p.lock().await;
            let mut cc = node.cluster_client.lock().await;
            match cc.member_add(add_req.clone()).await {
                Ok(_) => info!(log, "{}announced self to peer {}", LP, node.conn),
                Err(e) => error!(log, "{}failed to announce to peer {}: {}", LP, node.conn, e),
            }
        }

        // Phase B: race peers for fastest status response with data
        let (tx, mut rx) = channel(self.peers.len());
        for (i, p) in self.peers.iter().enumerate() {
            let tx = tx.clone();
            let p = p.clone();
            tokio::spawn(async move {
                let node = p.lock().await;
                let mut mt = node.mt_client.lock().await;
                if let Ok(resp) = mt.status(StatusRequest::default()).await {
                    let status = resp.into_inner();
                    let _ = tx.send((i, status.db_size)).await;
                }
            });
        }
        drop(tx);

        let mut best_peer: Option<usize> = None;
        while let Some((idx, db_size)) = rx.recv().await {
            if db_size > 0 {
                best_peer = Some(idx);
                break;
            }
            if best_peer.is_none() {
                best_peer = Some(idx);
            }
        }

        let peer_idx = match best_peer {
            Some(idx) => idx,
            None => return Err("no peer responded to status query".to_string()),
        };

        // Phase C: pull all KV data from chosen peer
        let peer = &self.peers[peer_idx];
        let peer_conn = peer.lock().await.conn.clone();
        info!(log, "{}syncing KV data from {}", LP, peer_conn);

        let range_req = RangeRequest {
            key: vec![],
            range_end: vec![0],
            ..Default::default()
        };
        let resp = {
            let node = peer.lock().await;
            let mut kv = node.kv_client.lock().await;
            kv.range(range_req).await
                .map_err(|e| format!("range query to {}: {}", peer_conn, e))?
        };
        let kvs = resp.into_inner().kvs;
        let count = kvs.len();
        if count > 0 {
            let mut vault = vault.write().await;
            for kv in kvs {
                let key = kv.key.clone();
                vault.insert(key, Kv::from(kv));
            }
        }
        info!(log, "{}synced {} keys from {}", LP, count, peer_conn);
        Ok(count)
    }

    /// Return (peer_id, conn, state) for all peers.
    pub(crate) async fn peer_info(&self) -> Vec<(NodeId, String, PeerState)> {
        let mut result = Vec::with_capacity(self.peers.len());
        for p in &self.peers {
            let node = p.lock().await;
            result.push((node.peer_id, node.conn.clone(), node.state));
        }
        result
    }

    pub(crate) async fn peer_urls(&self) -> String {
        let mut peers = Vec::new();
        for p in &self.peers {
            peers.push(p.lock().await.conn.clone());
        }
        peers.join(",")
    }
}


/// [lockup] A broadcast's targets, taken under the peer lock and sent to after it
/// is released.
///
/// Every network wait used to run INSIDE the caller's read lock
/// (`self.peers.read().await.broadcast_scoped(..)`). tokio's `RwLock` is fair: a
/// writer queued behind that reader - `reconfigure`, the peer retry, an incoming
/// `member_add` - makes every later reader wait too, so one slow or silent peer
/// stopped the whole node, and two nodes waiting on each other that way never
/// recovered. On the lab two of three nodes stopped answering after a
/// synchronized restart: member lists timed out and a put hung without reaching
/// anyone.
pub(crate) struct BroadcastPlan {
    /// the peer, and whether it is `Online` (counts toward the quorum)
    targets: Vec<(EtcdPeerNodeType, bool)>,
    node_id: NodeId,
    log: Logger,
}

impl BroadcastPlan {
    /// Send to every target with adaptive timeout. Only Online peers count
    /// toward quorum; InProgress peers receive the write (for sync) but their
    /// result is ignored for quorum. Peers that time out are demoted to Spare,
    /// and an InProgress peer that replicated is promoted to Online. Takes no
    /// lock but each peer's own.
    pub(crate) async fn send(self, request: BroadcastRequest) -> std::result::Result<(), Status> {
        if self.targets.is_empty() {
            return Ok(());
        }
        let quorum_total = self.targets.iter().filter(|(_, online)| *online).count();

        let start = Instant::now();
        let deadline_ms = Arc::new(AtomicU16::new(0));
        // target index + result
        let (reply, mut receiver) = channel(self.targets.len());
        let peer_id = Some(MetadataValue::from_str(self.node_id.to_string().as_str())
            .map_err(|e| Status::invalid_argument(format!("{}", e)))?);

        for (idx, (p, is_online)) in self.targets.iter().enumerate() {
            let reply_c = reply.clone();
            let r = request.clone();
            let p = p.clone();
            let peer_id = peer_id.clone();
            let deadline_ms = deadline_ms.clone();
            let is_online = *is_online;

            tokio::spawn(async move {
                let result = tokio::select! {
                    ok = EtcdCluster::peer_request(r, p, peer_id) => ok,
                    _ = EtcdCluster::await_deadline(&deadline_ms, start) => false,
                };
                let elapsed = start.elapsed().as_millis() as u16;
                let _ = reply_c.send((idx, is_online, result, elapsed)).await;
            });
        }
        drop(reply);

        let half = quorum_total as f32 / 2.0;
        let mut ok_count = 0u32;
        let mut online_reply_count = 0u32;
        let mut total_ms = 0u64;
        let mut timed_out_peers = Vec::new();
        // InProgress peers that replicated this message successfully: they have
        // now proven they can, which is what `Online` means.
        let mut synced_peers = Vec::new();

        while let Some((idx, is_online, ok, elapsed_ms)) = receiver.recv().await {
            if is_online {
                online_reply_count += 1;
                total_ms += elapsed_ms as u64;
                if ok {
                    ok_count += 1;
                } else {
                    timed_out_peers.push(idx);
                }
            } else if ok {
                synced_peers.push(idx);
            }
            if online_reply_count as f32 > half && deadline_ms.load(Ordering::Relaxed) == 0 {
                let avg = total_ms / online_reply_count as u64;
                let deadline = (avg * 2).min(u16::MAX as u64) as u16;
                deadline_ms.store(deadline.max(1), Ordering::Relaxed);
            }
        }

        for idx in &timed_out_peers {
            let mut peer = self.targets[*idx].0.lock().await;
            if peer.state == PeerState::Online {
                info!(self.log, "{}peer {} demoted to Spare (timeout)", LP, peer.conn);
                peer.state = PeerState::Spare;
            }
        }

        // The mirror of the demotion above, and the half that was missing: a
        // peer only ever left `InProgress` through the `member_promote` admin
        // API, so in an embedded deployment it never left it at all. Replicating
        // one message successfully is the evidence the state machine wanted.
        for idx in &synced_peers {
            let mut peer = self.targets[*idx].0.lock().await;
            if peer.state == PeerState::InProgress {
                info!(self.log, "{}peer {} promoted to Online (replicated successfully)",
                    LP, peer.conn);
                peer.state = PeerState::Online;
            }
        }

        if quorum_total == 0 || ok_count as f32 > half {
            Ok(())
        } else {
            Err(Status::aborted("wont commit more than half"))
        }
    }
}

#[cfg(test)]
mod zone_tests {
    use super::{host_key, zone_of};

    #[test]
    fn a_peer_url_reduces_to_its_host_and_port() {
        assert_eq!(host_key("http://10.0.0.7:2379"), "10.0.0.7:2379");
        assert_eq!(host_key("https://node1:2379/"), "node1:2379");
        assert_eq!(host_key(" 10.0.0.7:2379 "), "10.0.0.7:2379");
        assert_eq!(host_key("node1"), "node1");
    }

    #[test]
    fn a_zone_is_found_by_host_and_port_or_by_host_alone() {
        let z = "10.0.0.7:2379=east,node2=west";
        assert_eq!(zone_of("http://10.0.0.7:2379", z), "east");
        // an operator who did not write the port still gets a match
        assert_eq!(zone_of("http://node2:2379", z), "west");
    }

    #[test]
    fn a_specific_entry_wins_over_a_host_only_one() {
        // otherwise a broad entry could silently move a node between zones
        let z = "node1=west,node1:2379=east";
        assert_eq!(zone_of("http://node1:2379", z), "east");
    }

    #[test]
    fn an_unlisted_peer_is_unzoned_which_excludes_it() {
        // fail-safe: unlabelled compares unequal to a real zone, so a peer we
        // have not been told about receives no zone-scoped key
        assert_eq!(zone_of("http://node9:2379", "node1=east"), "");
        assert_eq!(zone_of("http://node9:2379", ""), "");
        // malformed entries are skipped, not guessed at
        assert_eq!(zone_of("http://node1:2379", "node1,node1=,=east"), "");
    }
}
