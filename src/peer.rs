use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use slog::*;
use tokio::{
    sync::Mutex,
    sync::mpsc::{channel},
};
use tonic::{ Status};
use tonic::metadata::MetadataValue;
use tonic::transport::{Channel, Endpoint};
use crate::{
    etcdpb::etcdserverpb::maintenance_client::MaintenanceClient,
    etcdpb::etcdserverpb::kv_client::KvClient,
    KvEvent,
    cluster::{EtcdPeerNodeType, NodeId},
};
use crate::cli::Config;
use crate::etcdpb::etcdserverpb::StatusRequest;

/// represent cluster structure
/// hold configs and capable to update
#[derive(Clone)]
pub struct EtcdCluster {
    /// cluster nodes (not clients, see node watchers for clients)
    peers: Vec<EtcdPeerNodeType>,
    /// 0 means no timeout
    connect_timeout_ms: u64,

    node_id: NodeId,
    cluster_id: NodeId,
    log: Logger,
}

pub(crate) struct EtcdPeerNode {
    pub(crate) peer_id: NodeId,
    pub(crate) conn: String,
    pub(crate) kv_client: Arc<Mutex<KvClient<Channel>>>,
    pub(crate) mt_client: Arc<Mutex<MaintenanceClient<Channel>>>,
    // pub(crate) lease_client: Arc<Mutex<LeaseClient<Channel>>>,
    
}

/// request and corresponding api calls to perform a broadcast
#[derive(Clone)]
pub(crate) enum BroadcastRequest {
    Kv(KvEvent),
    // Lease?
}


impl EtcdCluster {
    /// send request to the clusters peer and get success response from more than half nodes 
    pub(crate) async fn connect(cfg: &Config, node_id: NodeId, cluster_id: NodeId, log: &Logger) -> std::result::Result<Self, String> {
        let timeout_ms = cfg.election_timeout;


        let mut cluster = EtcdCluster {
            peers: vec![],
            connect_timeout_ms: timeout_ms as u64,
            node_id,
            cluster_id,
            log: log.clone(),
        };
        
        // let mut urls: HashSet<String> = HashSet::from_iter(clients.iter().cloned());
        let urls: HashSet<&str> = HashSet::from_iter(cfg.listen_client_urls.split(",")); //.map(|s| s.to_string()));
        
        let mut clients: HashSet<&str> = HashSet::from_iter(cfg.initial_advertise_peer_urls.split(",")); //.map(|s| s.to_string()));
        for h in urls {
            if h.contains("//") {
                clients.remove(h);
            } else {
                clients.remove(format!("http:://{}",h).as_str());
            }
        }

        let half = clients.len() as f32 / 2f32;
        if cluster.add_connections(clients).await? as f32 > half {
            Ok(cluster)
        } else {
            Err("cant connect to more than half peers".to_string())
        }
    }
    pub(crate) async fn add_connections(&mut self, mut clients: HashSet<&str>) -> std::result::Result<usize, String> {
        let connect_timeout_ms = self.connect_timeout_ms;
        for p in &self.peers {
            clients.remove(p.lock().await.conn.as_str());
        }
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
                                               error!(self.log, "connecting maintenance {} - no header in response", url);
                                           }
                                           Some(s) => {
                                               if s.cluster_id == self.cluster_id {
                                                   self.peers.push(Arc::new(Mutex::new(
                                                       EtcdPeerNode {
                                                           peer_id: s.member_id,
                                                           conn: url.to_string(),
                                                           kv_client: Arc::new(Mutex::new(KvClient::new(conn))),

                                                           mt_client: Arc::new(Mutex::new(mt)),
                                                       })));
                                                   cnt += 1;
                                               } else {
                                                   error!(self.log, "connecting maintenance {} - wrong cluster,\
                                                    running on ClusterID [{}], but connecting node from {}", 
                                                       url, self.cluster_id, s.cluster_id);
                                               }
                                           }
                                       }
                                   }
                                   Err(e) => {
                                       error!(self.log, "connecting maintenance {} with error {}", url, e);
                                   }
                               }
                           }
                           Err(e) => {
                               error!(self.log, "connecting endpoint {} with error {}", url, e);
                           }
                       }
                    }
                    Err(e) => {
                        error!(self.log, "making endpoint to {} with error {}", url, e);
                    }
                }
            }
        }
        Ok(cnt)
    }

    /// send request to the clusters peer and get success response from more than half nodes 
    pub(crate) async fn broadcast(&self, request: BroadcastRequest) -> std::result::Result<(), Status> {
        let (reply, mut receiver) = channel(10);
        let peer_id = Some(MetadataValue::from_str(self.node_id.to_string().as_str())
            .map_err(|e| Status::invalid_argument(format!("{}", e)))?);
        for peer in self.peers.iter() {
            let reply_c = reply.clone();
            let r = request.clone();
            let p = peer.clone();
            let peer_id = peer_id.clone();
            tokio::spawn(async move {
                let resp_ok = match r {
                    BroadcastRequest::Kv(br) => {
                        let node = p.lock().await;
                        let mut kv = node.kv_client.lock().await;
                        match br {
                            KvEvent::Put(kr) => kv.put(kr, peer_id).await.is_ok(),
                            KvEvent::Delete(kr) => kv.delete_range(kr, peer_id).await.is_ok(),
                            KvEvent::Txn(kr) => kv.txn(kr, peer_id).await.is_ok(),
                        }
                    }
                };
                reply_c.send(resp_ok).await
            });
        }
        let total_cnt = self.peers.len() as f32;

        let mut received_ok = 0f32;
        let mut received_total = 0f32;
        while !(received_ok > total_cnt / 2f32 
                ||  received_total >= total_cnt ) {
            
            if let Some(ok) = receiver.recv().await {
                received_total += 1f32;
                if ok {
                    received_ok += 1f32;
                }
            }
        }
        if received_ok > total_cnt / 2f32 {
            Ok(())
        } else {
            Err(Status::aborted("wont commit more than half"))
        }
    }
}