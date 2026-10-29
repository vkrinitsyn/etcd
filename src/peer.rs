use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;
use slog::*;
use tokio::{
    sync::Mutex,
    sync::mpsc::{channel},
    sync::Notify,
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

    /// [q-registry] Signalled when a peer starts to receive this node's writes:
    /// admitted, or promoted. The queue registry is written once, when a
    /// consumer registers or a dispatcher is claimed, and a peer that was not
    /// receiving then never learns it - see `EtcdNode::republish_registry`.
    joined: Arc<Notify>,
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
    /// [q-dead] since when this peer has not answered - a call or a probe
    /// (`EtcdCluster::probe`) - or `None` while it answers
    pub(crate) down_since: Option<Instant>,
    /// [q-dead] the queue registry's format this peer reads (`registry_of` its
    /// `status` version): 1 an older node's, the bare node id
    pub(crate) registry: u32,
    /// [q-dead] it answered again after it was down, and the probe has not yet
    /// made that a join (`EtcdCluster::probe`)
    pub(crate) recovered: bool,
}

/// [q-dead] The queue registry's format this node writes and reads:
/// `<node>;g=<generation>;s=<start>`. 1 is the bare node id of an older node.
pub(crate) const REGISTRY: u32 = 2;

/// [q-dead] What this node answers `status` with: `rust` as before, then its
/// crate version and its registry's format - how a peer tells an older node.
pub(crate) fn status_version() -> String {
    format!("rust etcds/{} registry/{}", env!("CARGO_PKG_VERSION"), REGISTRY)
}

/// [q-dead] The registry format a peer reads, from its `status` version. An
/// older node answers `rust` and nothing more: 1.
///
/// The cheapest reliable signal: every node of every version answers `status`,
/// a node asks it of each peer it connects to, and the probe asks it again
/// (`EtcdCluster::probe`) - so an upgraded peer is seen as such once it is back.
pub(crate) fn registry_of(version: &str) -> u32 {
    version.split_whitespace()
        .find_map(|w| w.strip_prefix("registry/"))
        .and_then(|v| v.parse().ok())
        .unwrap_or(1)
}

/// A peer that answered `status` for this cluster: its id, url, channel, client,
/// and [q-dead] the version it answered with.
pub(crate) type Dialled = (NodeId, String, Channel, MaintenanceClient<Channel>, String);

/// [q-dead] How one request to one peer went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Sent {
    /// it answered yes
    Taken,
    /// it answered no, or could not be reached, or is no target: not carried out
    Refused,
    /// no answer within `PEER_RPC_TIMEOUT`: carried out or not, nobody here knows
    NoAnswer,
    /// not connected: not carried out (`peer_request_outcome` reports it as
    /// `Refused`, and marks the peer down)
    Unreachable,
}

/// [q-dead] A failed call that did not reach the peer, as opposed to one it
/// answered with an error.
fn unreachable(e: &Status) -> bool {
    matches!(e.code(), tonic::Code::Unavailable) || e.message().contains("transport error")
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
/// URL - so both sides are reduced to the same shape before comparing rather
/// than requiring an operator to guess the stored form.
fn host_key(s: &str) -> String {
    let s = s.trim();
    let s = s.strip_prefix("http://").or_else(|| s.strip_prefix("https://")).unwrap_or(s);
    s.split('/').next().unwrap_or(s).trim().to_string()
}

/// Look up a peer's zone in a `host:port=zone,...` list.
///
/// Matches on `host:port` first, then on the bare host - so one entry can
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
    /// come from the embedder - which already knows the topology.
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
            joined: Arc::new(Notify::new()),
        };
        
        let peers = cfg.peers();
        let half = peers.len() as f32 / 2f32;
        let input_size = peers.len();
        let connected = cluster.add_connections(peers).await?;
        if input_size == 0 || connected as f32 > half {
            Ok(cluster)
        } else {
            Err(tr!("peer.half", connected = connected, total = input_size))
        }
    }
    
    /// [peer-retry] How many peers are currently connected.
    pub(crate) fn connected(&self) -> usize {
        self.peers.len()
    }

    /// [q-registry] The signal that a peer joined: admitted, or promoted.
    pub(crate) fn joined(&self) -> Arc<Notify> {
        self.joined.clone()
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
        -> Vec<Dialled>
    {
        let connect_timeout_ms = if connect_timeout_ms > 0 { connect_timeout_ms } else { 1000 };
        let mut found = Vec::new();
        for url in urls {
            let endpoint = match Endpoint::from_str(&url) {
                Ok(e) => e.connect_timeout(Duration::from_millis(connect_timeout_ms)),
                Err(e) => {
                    error!(log, "{}{}", LP, tr!("peer.endpoint", url = url, err = e));
                    continue;
                }
            };
            let conn = match endpoint.connect().await {
                Ok(c) => c,
                Err(e) => {
                    error!(log, "{}{}", LP, tr!("peer.connect", url = url, err = e));
                    continue;
                }
            };
            let mut mt = MaintenanceClient::new(conn.clone());
            let status = match tokio::time::timeout(PEER_RPC_TIMEOUT, mt.status(StatusRequest::default())).await {
                Ok(Ok(s)) => s.into_inner(),
                Ok(Err(e)) => {
                    error!(log, "{}{}", LP, tr!("peer.status", url = url, err = e));
                    continue;
                }
                Err(_) => {
                    error!(log, "{}{}", LP, tr!("peer.status_timeout", url = url, timeout = format!("{:?}", PEER_RPC_TIMEOUT)));
                    continue;
                }
            };
            match status.header {
                None => error!(log, "{}{}", LP, tr!("peer.no_header", url = url)),
                Some(h) if h.cluster_id != cluster_id => error!(log, "{}{}", LP,
                    tr!("peer.wrong_cluster", url = url, ours = cluster_id, theirs = h.cluster_id)),
                Some(h) => found.push((h.member_id, url, conn, mt, status.version.clone())),
            }
        }
        found
    }

    /// [lockup] Hold the dialled peers this node does not hold yet - by peer id,
    /// since one node can be reached under more than one url. Short and free of
    /// network waits: the only part of adding a peer that needs the lock.
    pub(crate) async fn admit(&mut self, dialled: Vec<Dialled>) -> usize {
        self.admit_fresh(dialled).await.len()
    }

    /// [q-registry] `admit`, returning the peers it added, so a caller can
    /// announce this node to them.
    pub(crate) async fn admit_fresh(&mut self, dialled: Vec<Dialled>)
        -> Vec<EtcdPeerNodeType>
    {
        self.recently_added.retain(|_, t| t.elapsed() < Duration::from_secs(RECENT_WINDOW_SECS));
        let mut fresh = Vec::new();
        for (member_id, url, conn, mt, version) in dialled {
            // [q-registry] Never this node itself: an announced url such as
            // `localhost:2379` or `0.0.0.0:2379` dials back to the node that got it.
            if member_id == self.node_id {
                continue;
            }
            let mut exists = false;
            for p in &self.peers {
                if p.lock().await.peer_id == member_id {
                    info!(self.log, "{}{}", LP, tr!("peer.already", id = member_id, url = url));
                    exists = true;
                    break;
                }
            }
            if exists { continue; }
            if let Some(t) = self.recently_added.get(&member_id) {
                if t.elapsed() < Duration::from_secs(RECENT_WINDOW_SECS) {
                    info!(self.log, "{}{}", LP, tr!("peer.recent", id = member_id, ago = format!("{:?}", t.elapsed()), url = url));
                    continue;
                }
            }
            self.recently_added.insert(member_id, Instant::now());
            let peer = Arc::new(Mutex::new(EtcdPeerNode {
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
                down_since: None,
                registry: registry_of(&version),
                recovered: false,
            }));
            self.peers.push(peer.clone());
            fresh.push(peer);
        }
        // [q-registry] a new peer receives writes from now on, and none of the
        // queue registry written before
        if !fresh.is_empty() {
            self.joined.notify_one();
        }
        fresh
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
    /// the key is what carries the policy - a peer cannot be asked whether it
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
        BroadcastPlan { targets, node_id: self.node_id, log: self.log.clone(), joined: self.joined.clone() }
    }

    /// One request to one peer.
    ///
    /// [q-route Phase 4] **The client is CLONED and both locks released before
    /// the call.** It used to hold `peer.lock()` *and* `kv_client.lock()` for
    /// the whole round trip, so every concurrent message to the same peer
    /// serialized on them - and a hot p2p pair is exactly the case that sends
    /// concurrent messages to one peer. The outer lock was the worse of the
    /// two: it also blocked every *state* read about that peer, so a slow RPC
    /// stalled `is_online`, `unicast` and `broadcast_scoped` for it.
    ///
    /// Cloning is cheap and correct: a tonic client is a thin handle over a
    /// `Channel`, which multiplexes concurrent HTTP/2 streams. This removes a
    /// serialization point, not an ordering guarantee - the queue's ordering
    /// comes from `idx`, assigned by the dispatcher, never from the order two
    /// peer RPCs happen to complete in.
    ///
    /// [lockup] Bounded by `PEER_RPC_TIMEOUT`: a peer that never answers is a
    /// failed send, which the broadcast then counts as one.
    pub(crate) async fn peer_request(request: BroadcastRequest, peer: EtcdPeerNodeType, peer_id: Option<MetadataValue<tonic::metadata::Ascii>>) -> bool {
        Self::peer_request_outcome(request, peer, peer_id).await == Sent::Taken
    }

    /// [q-dead] `peer_request`, telling a refusal from no answer: a call that
    /// timed out may have been carried out on the other side.
    pub(crate) async fn peer_request_outcome(request: BroadcastRequest, peer: EtcdPeerNodeType,
        peer_id: Option<MetadataValue<tonic::metadata::Ascii>>) -> Sent
    {
        let mut kv = {
            let node = peer.lock().await;
            let c = node.kv_client.lock().await.clone();
            c
        };
        // Ok(answered), Err(status) for a call that failed
        let call = async move {
            match request {
                BroadcastRequest::Kv(br) => {
                    match br {
                        KvEvent::Put(kr) => kv.put(kr, peer_id).await.map(|_| ()),
                        KvEvent::Delete(kr) => kv.delete_range(kr, peer_id).await.map(|_| ()),
                        KvEvent::Txn(kr) => kv.txn(kr, peer_id).await.map(|_| ()),
                    }
                }
            }
        };
        let sent = match tokio::time::timeout(PEER_RPC_TIMEOUT, call).await {
            Ok(Ok(())) => Sent::Taken,
            // [q-dead] it answered no: it is there
            Ok(Err(e)) if !unreachable(&e) => Sent::Refused,
            Ok(Err(_)) => Sent::Unreachable,
            Err(_) => Sent::NoAnswer,
        };
        Self::seen(&peer, sent != Sent::Unreachable && sent != Sent::NoAnswer).await;
        match sent {
            Sent::Unreachable => Sent::Refused,
            s => s,
        }
    }

    /// [q-dead] Note whether `peer` answered: down from its first call or
    /// probe that got no answer, up again at the next that does.
    pub(crate) async fn seen(peer: &EtcdPeerNodeType, answered: bool) {
        let mut p = peer.lock().await;
        if answered {
            if p.down_since.take().is_some() {
                p.recovered = true;
            }
        } else if p.down_since.is_none() {
            p.down_since = Some(Instant::now());
        }
    }

    /// [q-dead] Ask each peer its `status`: whether it answers - how long a
    /// queue consumer's node has been down (`down_for`) - and the registry
    /// format it reads (`legacy_registry`). Each peer's lock is held only to
    /// clone its client. Returns whether the registry format to write changed,
    /// and the peers that answer again after they were down - noted by this
    /// probe or by a call since the last: a partition healed, after which each
    /// side's registry has to be written to the other again (`probe_peers`).
    pub(crate) async fn probe(peers: Vec<EtcdPeerNodeType>) -> (bool, Vec<EtcdPeerNodeType>) {
        let before = Self::legacy_of(&peers).await;
        let mut back = Vec::new();
        for p in &peers {
            let mut mt = p.lock().await.mt_client.lock().await.clone();
            match tokio::time::timeout(PEER_RPC_TIMEOUT, mt.status(StatusRequest::default())).await {
                Ok(Ok(s)) => {
                    Self::seen(p, true).await;
                    p.lock().await.registry = registry_of(&s.into_inner().version);
                }
                _ => Self::seen(p, false).await,
            }
            let mut n = p.lock().await;
            if std::mem::take(&mut n.recovered) {
                back.push(p.clone());
            }
        }
        (before != Self::legacy_of(&peers).await, back)
    }

    /// [q-dead] Every peer held, for `probe` - which runs with the lock released.
    pub(crate) fn handles(&self) -> Vec<EtcdPeerNodeType> {
        self.peers.clone()
    }

    /// [q-dead] One peer, by id.
    pub(crate) async fn handle(&self, peer_id: NodeId) -> Option<EtcdPeerNodeType> {
        for p in &self.peers {
            if p.lock().await.peer_id == peer_id {
                return Some(p.clone());
            }
        }
        None
    }

    /// [q-dead] How long `peer_id` has not answered; `None` while it answers,
    /// or for a node not held.
    pub(crate) async fn down_for(&self, peer_id: NodeId) -> Option<Duration> {
        for p in &self.peers {
            let n = p.lock().await;
            if n.peer_id == peer_id {
                return n.down_since.map(|t| t.elapsed());
            }
        }
        None
    }

    /// [q-dead] Is a member of an older version here - one that reads only the
    /// bare node id in the queue registry? Then this node writes that form.
    pub(crate) async fn legacy_registry(&self) -> bool {
        Self::legacy_of(&self.peers).await
    }

    async fn legacy_of(peers: &[EtcdPeerNodeType]) -> bool {
        for p in peers {
            if p.lock().await.registry < REGISTRY {
                return true;
            }
        }
        false
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
                info!(log, "{}{}", LP, tr!("peer.state", peer = node.conn, id = peer_id, state = "InProgress"));
                node.state = PeerState::InProgress;
                // [q-registry] receives writes again, and missed those made meanwhile
                self.joined.notify_one();
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
                info!(log, "{}{}", LP, tr!("peer.state", peer = node.conn, id = peer_id, state = "Online"));
                node.state = PeerState::Online;
                self.joined.notify_one();
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
                info!(log, "{}{}", LP, tr!("peer.state", peer = node.conn, id = peer_id, state = "Spare"));
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
    /// and *route*, not merely store - electing one that is still catching up
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
    /// zone-scoped key must not reach a peer in another zone - or one whose
    /// zone is *unknown*, which is the fail-safe direction: a peer added since
    /// the last reconfigure is unlabelled, and treating "no label" as "same
    /// zone" is the leak zoning exists to prevent. Being targeted rather than
    /// broadcast changes nothing about that.
    ///
    /// `None` when the peer is not held, not Online, or excluded by the zone test
    /// - the caller falls back to a broadcast and says so, rather than silently
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
        policy: &crate::prefix::PrefixPolicy,
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
                Ok(_) => info!(log, "{}{}", LP, tr!("peer.announced", peer = node.conn)),
                Err(e) => error!(log, "{}{}", LP, tr!("peer.announce_failed", peer = node.conn, err = e)),
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
            None => return Err(tr!("peer.no_status")),
        };

        // Phase C: pull all KV data from chosen peer
        let peer = &self.peers[peer_idx];
        let peer_conn = peer.lock().await.conn.clone();
        info!(log, "{}{}", LP, tr!("peer.syncing", peer = peer_conn));

        let range_req = RangeRequest {
            key: vec![],
            range_end: vec![0],
            ..Default::default()
        };
        let resp = {
            let node = peer.lock().await;
            let mut kv = node.kv_client.lock().await;
            kv.range(range_req).await
                .map_err(|e| tr!("peer.range", peer = peer_conn, err = e))?
        };
        // [E2] only what propagates: a peer's `local` keys are its own, and copying
        // them made them this node's too
        let kvs: Vec<_> = resp.into_inner().kvs.into_iter().filter(|kv| policy.propagates(&kv.key)).collect();
        let count = kvs.len();
        if count > 0 {
            let mut vault = vault.write().await;
            for kv in kvs {
                let key = kv.key.clone();
                vault.insert(key, Kv::from(kv));
            }
        }
        info!(log, "{}{}", LP, tr!("peer.synced", count = count, peer = peer_conn));
        Ok(count)
    }

    /// [q-registry] Announce this node to `peers`, as `announce_and_sync` does
    /// at its start - for a peer connected since, which has not heard of it.
    ///
    /// `announce_and_sync` runs once. A peer that was not up then, or that
    /// restarted and could not reach this node at its own start, is connected
    /// later by the retry, and nothing told it this node is here: its queue
    /// registry stayed where it was written. Its `member_add` answers by
    /// writing that registry again (`srv::parking`).
    ///
    /// Network only: each peer's lock is held to clone its client and released
    /// before the call, and the call is bounded by `PEER_RPC_TIMEOUT`.
    pub(crate) async fn announce(peers: Vec<EtcdPeerNodeType>, my_client_urls: Vec<String>, log: &Logger) {
        let add_req = MemberAddRequest {
            peer_ur_ls: my_client_urls,
            is_learner: true,
        };
        for p in peers {
            let (mut cc, conn) = {
                let node = p.lock().await;
                let cc = node.cluster_client.lock().await.clone();
                (cc, node.conn.clone())
            };
            match tokio::time::timeout(PEER_RPC_TIMEOUT, cc.member_add(add_req.clone())).await {
                Ok(Ok(_)) => info!(log, "{}{}", LP, tr!("peer.announced", peer = conn)),
                Ok(Err(e)) => error!(log, "{}{}", LP, tr!("peer.announce_failed", peer = conn, err = e)),
                Err(e) => error!(log, "{}{}", LP, tr!("peer.announce_failed", peer = conn, err = e)),
            }
        }
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
    /// [q-registry] `EtcdCluster::joined`, for a peer this send promotes
    joined: Arc<Notify>,
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
                // [q-dead] `peer_request` notes whether the peer answered
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
                info!(self.log, "{}{}", LP, tr!("peer.timeout_spare", peer = peer.conn));
                peer.state = PeerState::Spare;
            }
        }

        // The mirror of the demotion above, and the half that was missing: a
        // peer only ever left `InProgress` through the `member_promote` admin
        // API, so in an embedded deployment it never left it at all. Replicating
        // one message successfully is the evidence the state machine wanted.
        let mut promoted = false;
        for idx in &synced_peers {
            let mut peer = self.targets[*idx].0.lock().await;
            if peer.state == PeerState::InProgress {
                info!(self.log, "{}{}", LP, tr!("peer.replicated_online", peer = peer.conn));
                peer.state = PeerState::Online;
                promoted = true;
            }
        }
        // [q-registry] A peer that comes Online gets the queue registry again.
        // Its first writes went out while it was InProgress, and a failed one
        // there is not counted - so the registry may be exactly what it missed.
        if promoted {
            self.joined.notify_one();
        }

        if quorum_total == 0 || ok_count as f32 > half {
            Ok(())
        } else {
            Err(Status::aborted(tr!("peer.quorum")))
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

#[cfg(test)]
mod state_tests {
    //! The peer state machine and the network edges of peering: who is accepted,
    //! what a new node loads from a peer, what a zone keeps in, and what a peer
    //! that fails a write costs it.
    use super::*;
    use crate::cluster::EtcdNode;
    use crate::etcdpb::etcdserverpb::kv_server::Kv as _;
    use crate::etcdpb::etcdserverpb::PutRequest;

    const CLUSTER: &str = "11111111-1111-5111-8111-111111111111";

    fn discard() -> Logger {
        Logger::root(slog::Discard, slog::o!())
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }

    async fn until<F, Fut>(f: F) -> bool where F: Fn() -> Fut, Fut: std::future::Future<Output = bool> {
        for _ in 0..150 {
            if f().await { return true; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    fn cfg(node: &str, listen: &str, peers: &str) -> EtcdConfig {
        let mut c = EtcdConfig::with_defaults();
        c.cluster = CLUSTER.to_string();
        c.node = node.to_string();
        c.listen_client_urls = listen.to_string();
        c.initial_advertise_peer_urls = peers.to_string();
        c
    }

    async fn init(c: EtcdConfig) -> EtcdNode {
        EtcdNode::init(c, discard(), #[cfg(feature = "tracer")] None).await.unwrap()
    }

    /// A node served on a port of its own; returns it and its url.
    async fn served(node: &str, mut c: EtcdConfig) -> (EtcdNode, String) {
        let port = free_port();
        c.node = node.to_string();
        c.listen_client_urls = format!("127.0.0.1:{}", port);
        let n = init(c).await;
        n.serve().await.unwrap();
        assert!(until(|| async { tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() }).await);
        (n, format!("http://127.0.0.1:{}", port))
    }

    const A: &str = "88888888-8888-5888-8888-888888888881";
    const B: &str = "88888888-8888-5888-8888-888888888882";

    /// A starting node loads the keys of the peer that answers - except those
    /// its own policy keeps local.
    #[tokio::test]
    async fn a_starting_node_loads_its_peers_keys() {
        let (a, a_url) = served(A, cfg(A, "", "")).await;
        a.kv_put(b"/shared/k".to_vec(), b"1".to_vec()).await.unwrap();
        a.kv_put(b"/mine/k".to_vec(), b"2".to_vec()).await.unwrap();

        let mut c = cfg(B, "127.0.0.1:0", &a_url);
        c.local_prefix = "/mine/".to_string();
        let b = init(c).await;
        assert_eq!(b.kv_get(b"/shared/k").await.as_deref(), Some(&b"1"[..]), "loaded at start");
        assert_eq!(b.kv_get(b"/mine/k").await, None, "a local key is not copied");
        assert_eq!(b.get_peer_urls().await, a_url);
    }

    /// A node that can reach no more than half its peers refuses to start.
    #[tokio::test]
    async fn too_few_reachable_peers_fail_the_start() {
        let closed = format!("http://127.0.0.1:{}", free_port());
        let e = EtcdNode::init(cfg(B, "127.0.0.1:0", &closed), discard(),
            #[cfg(feature = "tracer")] None).await.err().expect("refused");
        assert!(e.starts_with("ETCD-E030 ") && e.contains("[0/1]"), "{}", e);
    }

    /// Only a url that answers `status` with this cluster's id is a peer.
    #[tokio::test]
    async fn dial_keeps_only_members_of_this_cluster() {
        let (_a, a_url) = served(A, cfg(A, "", "")).await;
        let mut other = cfg(B, "", "");
        other.cluster = "99999999-9999-5999-8999-999999999999".to_string();
        let (_s, stranger_url) = served(B, other).await;
        let cluster_id = {
            let (a, b) = uuid::Uuid::parse_str(CLUSTER).unwrap().as_u64_pair();
            a ^ b
        };
        let closed = format!("http://127.0.0.1:{}", free_port());
        let found = EtcdCluster::dial(
            vec!["not a url".to_string(), closed, stranger_url, a_url.clone()], cluster_id, 0, &discard()).await;
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1, a_url);
    }

    /// One peer is admitted once, by id, whatever url it is dialled under; and
    /// one that was added moments ago is not re-added.
    #[tokio::test]
    async fn a_peer_is_admitted_once() {
        let (a, a_url) = served(A, cfg(A, "", "")).await;
        let b = init(cfg(B, "127.0.0.1:0", &a_url)).await;
        let mut peers = b.peers.write().await;
        assert_eq!(peers.connected(), 1);
        let (cluster_id, ms, log) = peers.dial_params();
        let alias = a_url.replace("127.0.0.1", "localhost");
        let again = EtcdCluster::dial(vec![alias.clone()], cluster_id, ms, &log).await;
        assert_eq!(again[0].0, a.node_id, "the same node under another url");
        assert_eq!(peers.admit(again).await, 0, "already held");

        peers.peers.clear(); // as if it had been dropped
        let again = EtcdCluster::dial(vec![alias], cluster_id, ms, &log).await;
        assert_eq!(peers.admit(again).await, 0, "added within RECENT_WINDOW_SECS: not again");
        assert_eq!(peers.connected(), 0);
    }

    /// [q-dead] The probe tells a peer that answers from one that does not, and
    /// reads the registry format each reads: 1 for an older node's `rust`.
    #[tokio::test]
    async fn the_probe_tells_who_answers_and_what_it_reads() {
        assert_eq!(registry_of("rust"), 1, "an older node");
        assert_eq!(registry_of(&status_version()), REGISTRY);
        let (_a, a_url) = served(A, cfg(A, "", "")).await;
        let b = init(cfg(B, "127.0.0.1:0", &a_url)).await;
        let ch = tonic::transport::Endpoint::from_static("http://127.0.0.1:9").connect_lazy();
        let mt = MaintenanceClient::new(ch.clone());
        b.peers.write().await.admit_fresh(vec![(987654321, "http://127.0.0.1:9".to_string(), ch, mt, "rust".to_string())]).await;
        assert!(b.peers.read().await.legacy_registry().await, "an older member");

        let held = b.peers.read().await.handles();
        let _ = EtcdCluster::probe(held).await;
        let a_id = b.peers.read().await.peer_info().await.into_iter().find(|(id, _, _)| *id != 987654321).unwrap().0;
        assert_eq!(b.peers.read().await.down_for(a_id).await, None, "A answers");
        assert!(b.peers.read().await.down_for(987654321).await.is_some(), "the one at a closed port does not");
    }

    /// [q-registry] A node never holds itself as a peer, though a url it is
    /// told - another node's `localhost:2379` - may reach it.
    #[tokio::test]
    async fn a_node_does_not_admit_itself() {
        let (a, a_url) = served(A, cfg(A, "", "")).await;
        let (cluster_id, ms, log) = a.peers.read().await.dial_params();
        let me = EtcdCluster::dial(vec![a_url], cluster_id, ms, &log).await;
        assert_eq!(me[0].0, a.node_id, "the url reaches this node");
        assert_eq!(a.peers.write().await.admit(me).await, 0);
        assert_eq!(a.peers.read().await.connected(), 0);
    }

    /// Spare -> InProgress -> Online through `member_promote`, the admin call;
    /// Online cannot be promoted, and an unknown id is not found.
    #[tokio::test]
    async fn member_promote_walks_the_states() {
        use crate::etcdpb::etcdserverpb::cluster_server::Cluster;
        use crate::etcdpb::etcdserverpb::{MemberListRequest, MemberPromoteRequest};
        let (a, a_url) = served(A, cfg(A, "", "")).await;
        let b = init(cfg(B, "127.0.0.1:0", &a_url)).await;
        let id = a.node_id;
        let state = || async { b.peers.read().await.peer_state(id).await };
        let promote = |i: u64| { let b = b.clone(); async move {
            b.member_promote(tonic::Request::new(MemberPromoteRequest { id: i })).await } };

        assert_eq!(state().await, Some(PeerState::InProgress), "a verified peer starts InProgress");
        let list = b.member_list(tonic::Request::new(MemberListRequest::default())).await.unwrap().into_inner();
        assert_eq!(list.members.len(), 1);
        assert!(list.members[0].is_learner, "not Online yet: a learner");
        assert_eq!(list.members[0].peer_ur_ls, vec![a_url.clone()]);

        promote(id).await.unwrap();
        assert_eq!(state().await, Some(PeerState::Online));
        assert_eq!(b.peers.read().await.online_count().await, 1);
        assert_eq!(promote(id).await.unwrap_err().code(), tonic::Code::FailedPrecondition);

        assert!(b.peers.read().await.demote_to_spare(id, &b.log).await);
        assert!(!b.peers.read().await.demote_to_spare(id, &b.log).await, "already Spare");
        assert_eq!(b.peers.read().await.online_count().await, 0);
        promote(id).await.unwrap();
        assert_eq!(state().await, Some(PeerState::InProgress), "Spare goes back to syncing first");
        assert!(!b.peers.read().await.promote_to_syncing(id, &b.log).await, "only from Spare");
        assert!(!b.peers.read().await.promote_to_online(12345, &b.log).await);
        assert_eq!(promote(12345).await.unwrap_err().code(), tonic::Code::NotFound);
    }

    /// An Online peer that fails a write is demoted to Spare, and with no
    /// quorum the put answers `aborted` - after applying it locally.
    #[tokio::test]
    async fn an_online_peer_that_fails_a_write_is_demoted_and_the_put_aborted() {
        let mut ac = cfg(A, "", "");
        ac.readonly_prefix = "/ro/".to_string(); // A refuses a write there, from anyone
        let (a, a_url) = served(A, ac).await;
        let b = init(cfg(B, "127.0.0.1:0", &a_url)).await;
        assert!(b.peers.read().await.promote_to_online(a.node_id, &b.log).await);

        let e = b.put(tonic::Request::new(PutRequest { key: b"/ro/k".to_vec(), value: b"v".to_vec(),
            ..Default::default() })).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::Aborted);
        assert!(b.kv_get(b"/ro/k").await.is_some(), "applied here before the broadcast");
        assert_eq!(b.peers.read().await.peer_state(a.node_id).await, Some(PeerState::Spare));

        // a Spare peer is sent nothing, so the next put has no quorum to miss
        b.put(tonic::Request::new(PutRequest { key: b"/ok".to_vec(), value: b"v".to_vec(),
            ..Default::default() })).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(a.kv_get(b"/ok").await.is_none(), "not replicated to a Spare peer");
    }

    /// A key under `zone_prefix` goes only to peers labelled with this node's
    /// zone; an unlabelled or other-zone peer gets nothing, by broadcast or by
    /// unicast.
    #[tokio::test]
    async fn a_zone_scoped_key_reaches_only_the_same_zone() {
        use crate::{EtcdEvents, EtcdMgmtEvent};
        let (a, a_url) = served(A, cfg(A, "", "")).await;
        let mut bc = cfg(B, "127.0.0.1:0", &a_url);
        bc.zone = "east".to_string();
        bc.zone_prefix = "/z/".to_string();
        let b = init(bc.clone()).await;

        // A unlabelled: excluded from a zone-scoped write, fail-safe
        b.kv_put(b"/z/1".to_vec(), b"v".to_vec()).await.unwrap();
        b.kv_put(b"/flat".to_vec(), b"v".to_vec()).await.unwrap();
        assert!(until(|| async { a.kv_get(b"/flat").await.is_some() }).await, "an unscoped key travels");
        assert!(a.kv_get(b"/z/1").await.is_none());
        let plan = b.peers.read().await.plan(Some("east")).await;
        assert!(plan.targets.is_empty());
        assert!(b.peers.read().await.unicast_target(a.node_id, Some("east")).await.is_none());

        // labelled east, as B is: now it does
        bc.peer_zones = format!("{}=east", a_url.trim_start_matches("http://"));
        b.event.send(EtcdEvents::Mgmt(EtcdMgmtEvent::Config(bc))).await.unwrap();
        assert!(until(|| async { b.peers.read().await.plan(Some("east")).await.targets.len() == 1 }).await);
        b.kv_put(b"/z/2".to_vec(), b"v".to_vec()).await.unwrap();
        assert!(until(|| async { a.kv_get(b"/z/2").await.is_some() }).await);
        assert!(b.peers.read().await.unicast_target(a.node_id, Some("west")).await.is_none(), "not west");
        assert!(b.peers.read().await.unicast_target(a.node_id, Some("east")).await.is_some());
        b.kv_delete(b"/z/2".to_vec()).await.unwrap();
        assert!(until(|| async { a.kv_get(b"/z/2").await.is_none() }).await, "and so does its delete");
    }

    /// A txn is broadcast whole, so it propagates only when every key it
    /// touches may - a nested txn's included. One that mixes local and
    /// replicated keys stays here.
    #[tokio::test]
    async fn a_txn_propagates_only_when_all_its_keys_do() {
        use crate::etcdpb::etcdserverpb::{request_op, RequestOp, TxnRequest};
        let (a, a_url) = served(A, cfg(A, "", "")).await;
        let mut bc = cfg(B, "127.0.0.1:0", &a_url);
        bc.local_prefix = "/mine/".to_string();
        let b = init(bc).await;
        let put = |k: &str| RequestOp { request: Some(request_op::Request::RequestPut(
            PutRequest { key: k.as_bytes().to_vec(), value: b"v".to_vec(), ..Default::default() })) };
        let txn = |ops: Vec<RequestOp>| TxnRequest { success: ops, ..Default::default() };

        b.txn(tonic::Request::new(txn(vec![put("/mine/1"), put("/shared/1")]))).await.unwrap();
        b.txn(tonic::Request::new(txn(vec![put("/shared/2"),
            RequestOp { request: Some(request_op::Request::RequestTxn(txn(vec![put("/mine/2")]))) }]))).await.unwrap();
        b.txn(tonic::Request::new(txn(vec![put("/mine/3")]))).await.unwrap();
        b.txn(tonic::Request::new(txn(vec![put("/shared/4")]))).await.unwrap();

        assert!(until(|| async { a.kv_get(b"/shared/4").await.is_some() }).await, "an all-replicated txn travels");
        for k in ["/mine/1", "/shared/1", "/shared/2", "/mine/2", "/mine/3"] {
            assert!(b.kv_get(k.as_bytes()).await.is_some(), "{} applied here", k);
            assert!(a.kv_get(k.as_bytes()).await.is_none(), "{} must not reach the peer", k);
        }
    }
}
