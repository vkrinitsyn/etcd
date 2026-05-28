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


impl EtcdCluster {
    /// send request to the clusters peer and get success response from more than half nodes 
    pub(crate) async fn connect(cfg: &EtcdConfig, node_id: NodeId, cluster_id: NodeId, log: &Logger) -> std::result::Result<Self, String> {
        let timeout_ms = cfg.election_timeout;


        let mut cluster = EtcdCluster {
            peers: vec![],
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
                                                       state: PeerState::Spare,
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
        if self.peers.is_empty() {
            return Ok(());
        }

        let mut online_indices = Vec::new();
        let mut syncing_indices = Vec::new();
        for (i, p) in self.peers.iter().enumerate() {
            match p.lock().await.state {
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

        while let Some((idx, is_online, ok, elapsed_ms)) = receiver.recv().await {
            if is_online {
                online_reply_count += 1;
                total_ms += elapsed_ms as u64;
                if ok {
                    ok_count += 1;
                } else {
                    timed_out_peers.push(idx);
                }
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

        if quorum_total == 0 || ok_count as f32 > half {
            Ok(())
        } else {
            Err(Status::aborted("wont commit more than half"))
        }
    }

    async fn peer_request(request: BroadcastRequest, peer: EtcdPeerNodeType, peer_id: Option<MetadataValue<tonic::metadata::Ascii>>) -> bool {
        match request {
            BroadcastRequest::Kv(br) => {
                let node = peer.lock().await;
                let mut kv = node.kv_client.lock().await;
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
    pub(crate) async fn online_count(&self) -> usize {
        let mut count = 0;
        for p in &self.peers {
            if p.lock().await.state == PeerState::Online {
                count += 1;
            }
        }
        count
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