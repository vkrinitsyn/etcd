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

    pub(crate) async fn add_connections(&mut self, mut clients: HashSet<&str>) -> std::result::Result<usize, String> {
        let connect_timeout_ms = self.connect_timeout_ms;
        for p in &self.peers {
            clients.remove(p.lock().await.conn.as_str());
        }
        self.recently_added.retain(|_, t| t.elapsed() < Duration::from_secs(RECENT_WINDOW_SECS));
        let mut cnt = 0;
        for url in clients {
            if url.starts_with("http") {
                let connect_timeout_ms = if connect_timeout_ms > 0 { connect_timeout_ms } else { 1000 };
                match Endpoint::from_str(&url) {
                    Ok(conn) => {
                       let conn = if connect_timeout_ms > 0 {
                           conn.connect_timeout(Duration::from_millis(connect_timeout_ms))
                       } else {
                           conn
                       };
                       match conn.connect().await {
                           Ok(conn) => {
                               let mut mt = MaintenanceClient::new(conn.clone());
                               match mt.status(StatusRequest::default()).await {
                                   Ok(node) => {
                                       let status = node.into_inner();
                                       match status.header {
                                           None => {
                                               error!(self.log, "{}connecting maintenance {} - no header in response", LP, url);
                                           }
                                           Some(s) => {
                                               if s.cluster_id != self.cluster_id {
                                                   error!(self.log, "{}connecting maintenance {} - wrong cluster,\
                                                    running on ClusterID [{}], but connecting node from {}", LP,
                                                       url, self.cluster_id, s.cluster_id);
                                                   continue;
                                               }
                                               // dedup by peer_id
                                               let mut exists = false;
                                               for p in &self.peers {
                                                   if p.lock().await.peer_id == s.member_id {
                                                       info!(self.log, "{}peer_id {} already connected, skipping {}", LP, s.member_id, url);
                                                       exists = true;
                                                       break;
                                                   }
                                               }
                                               if exists { continue; }
                                               // recently added check
                                               if let Some(t) = self.recently_added.get(&s.member_id) {
                                                   if t.elapsed() < Duration::from_secs(RECENT_WINDOW_SECS) {
                                                       info!(self.log, "{}peer_id {} recently added ({:?} ago), skipping {}", LP, s.member_id, t.elapsed(), url);
                                                       continue;
                                                   }
                                               }
                                               self.recently_added.insert(s.member_id, Instant::now());
                                               self.peers.push(Arc::new(Mutex::new(
                                                   EtcdPeerNode {
                                                       peer_id: s.member_id,
                                                       conn: url.to_string(),
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
                                       }
                                   }
                                   Err(e) => {
                                       error!(self.log, "{}connecting maintenance {} with error {}", LP, url, e);
                                   }
                               }
                           }
                           Err(e) => {
                               error!(self.log, "{}connecting endpoint {} with error {}", LP, url, e);
                           }
                       }
                    }
                    Err(e) => {
                        error!(self.log, "{}making endpoint to {} with error {}", LP, url, e);
                    }
                }
            }
        }
        Ok(cnt)
    }

    /// Broadcast request to cluster peers with adaptive timeout.
    /// Only Online peers count toward quorum. InProgress peers receive
    /// the write (for sync) but their result is ignored for quorum.
    /// Spare peers are skipped entirely.
    /// Peers that timeout are demoted to Spare.
    pub(crate) async fn broadcast(&self, request: BroadcastRequest) -> std::result::Result<(), Status> {
        self.broadcast_scoped(request, None).await
    }

    /// Broadcast, optionally confined to peers sharing this node's zone.
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
    pub(crate) async fn broadcast_scoped(&self, request: BroadcastRequest, zone_scoped: Option<&str>)
        -> std::result::Result<(), Status>
    {
        if self.peers.is_empty() {
            return Ok(());
        }

        let mut online_indices = Vec::new();
        let mut syncing_indices = Vec::new();
        for (i, p) in self.peers.iter().enumerate() {
            let p = p.lock().await;
            if let Some(my_zone) = zone_scoped {
                if p.zone.trim() != my_zone.trim() {
                    continue;
                }
            }
            match p.state {
                PeerState::Online => online_indices.push(i),
                PeerState::InProgress => syncing_indices.push(i),
                PeerState::Spare => {}
            }
        }

        let quorum_total = online_indices.len();
        let send_indices: Vec<usize> = online_indices.iter().chain(syncing_indices.iter()).copied().collect();
        if send_indices.is_empty() {
            return Ok(());
        }

        let start = Instant::now();
        let deadline_ms = Arc::new(AtomicU16::new(0));
        // peer index + result
        let (reply, mut receiver) = channel(send_indices.len());
        let peer_id = Some(MetadataValue::from_str(self.node_id.to_string().as_str())
            .map_err(|e| Status::invalid_argument(format!("{}", e)))?);

        for &idx in &send_indices {
            let reply_c = reply.clone();
            let r = request.clone();
            let p = self.peers[idx].clone();
            let peer_id = peer_id.clone();
            let deadline_ms = deadline_ms.clone();
            let start = start;
            let is_online = online_indices.contains(&idx);

            tokio::spawn(async move {
                let result = tokio::select! {
                    ok = Self::peer_request(r, p, peer_id) => ok,
                    _ = Self::await_deadline(&deadline_ms, start) => false,
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
            let mut peer = self.peers[*idx].lock().await;
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
            let mut peer = self.peers[*idx].lock().await;
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
    async fn peer_request(request: BroadcastRequest, peer: EtcdPeerNodeType, peer_id: Option<MetadataValue<tonic::metadata::Ascii>>) -> bool {
        let mut kv = {
            let node = peer.lock().await;
            let c = node.kv_client.lock().await.clone();
            c
        };
        match request {
            BroadcastRequest::Kv(br) => {
                match br {
                    KvEvent::Put(kr) => kv.put(kr, peer_id).await.is_ok(),
                    KvEvent::Delete(kr) => kv.delete_range(kr, peer_id).await.is_ok(),
                    KvEvent::Txn(kr) => kv.txn(kr, peer_id).await.is_ok(),
                }
            }
        }
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

    /// Send one request to one peer.
    ///
    /// **Applies the same zone test as `broadcast_scoped`, and that is not
    /// optional.** A unicast is still a write leaving this node, so a
    /// zone-scoped key must not reach a peer in another zone — or one whose
    /// zone is *unknown*, which is the fail-safe direction: a peer added since
    /// the last reconfigure is unlabelled, and treating "no label" as "same
    /// zone" is the leak zoning exists to prevent. Being targeted rather than
    /// broadcast changes nothing about that.
    ///
    /// Returns `false` when the peer is unreachable, not Online, or excluded by
    /// the zone test — the caller falls back to a broadcast and says so, rather
    /// than silently dropping the message.
    pub(crate) async fn unicast(&self, request: BroadcastRequest, peer_id: NodeId,
        zone_scoped: Option<&str>) -> bool
    {
        let target = {
            let mut found = None;
            for p in &self.peers {
                let n = p.lock().await;
                if n.peer_id != peer_id {
                    continue;
                }
                if n.state != PeerState::Online {
                    return false;
                }
                if let Some(my_zone) = zone_scoped {
                    if n.zone.trim() != my_zone.trim() {
                        return false;
                    }
                }
                found = Some(p.clone());
                break;
            }
            found
        };
        let Some(target) = target else { return false };
        let id = match MetadataValue::from_str(self.node_id.to_string().as_str()) {
            Ok(v) => Some(v),
            Err(_) => None,
        };
        Self::peer_request(request, target, id).await
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
