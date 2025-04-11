use crate::cli::EtcdConfig;
use crate::etcdpb::etcdserverpb::kv_server::{Kv, KvServer};
use crate::queue::{Queue};
use crate::{EtcdEvents, EtcdMgmtEvent, KvEvent};
use slog::{error, info, Logger};
use std::collections::{HashMap};
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::mpsc::Sender;
use tokio::sync::{mpsc, Mutex, RwLock};
use tonic::Status;
use tonic::transport::Server;
use tonic::transport::server::Router;
use uuid::Uuid;
use crate::etcdpb::etcdserverpb::auth_server::AuthServer;
use crate::etcdpb::etcdserverpb::cluster_server::ClusterServer;
use crate::etcdpb::etcdserverpb::lease_server::LeaseServer;
use crate::etcdpb::etcdserverpb::maintenance_server::MaintenanceServer;
use crate::etcdpb::etcdserverpb::watch_server::{ WatchServer};
use crate::etcdpb::etcdserverpb::WatchResponse;
use crate::etcdpb::v3electionpb::election_server::ElectionServer;
use crate::etcdpb::v3lockpb::lock_server::LockServer;
use crate::peer::EtcdCluster;
pub(crate) use crate::peer::EtcdPeerNode;


impl EtcdNode {
    pub async fn init(cfg: EtcdConfig, log: Logger) -> Result<Self, String> {
        let node_id = parse_uuid(&cfg.node)?; 
        let cluster = EtcdCluster::connect(&cfg, node_id, parse_uuid(&cfg.cluster)?, &log).await?;

        let (event, mut rsvr) = mpsc::channel(10);
        let (watcher, watcher_rv) = mpsc::channel(10);

        let c = EtcdNode {
            cfg: Arc::new(RwLock::new(cfg)),
            vault: Arc::new(Default::default()),
            queues: Arc::new(Default::default()),
            observers: Arc::new(Default::default()),
            watchers: Arc::new(Default::default()),
            watch_notify: watcher,
            peers: Arc::new(RwLock::new(cluster)),
            node_id,
            event,
            log: log.clone(),
        };
        c.watch_notify(watcher_rv).await;
        let grpc_client = c.clone();

        tokio::spawn(async move {
            while let Some(r) = rsvr.recv().await {
                match r {
                    EtcdEvents::Data(kv) => match kv {
                        KvEvent::Put(kv) => {
                            let _ = grpc_client.put(tonic::Request::new(kv)).await;
                        },
                        KvEvent::Delete(kv) => {
                            let _ = grpc_client.delete_range(tonic::Request::new(kv)).await;
                        }
                        KvEvent::Txn(kv) => {
                            let _ = grpc_client.txn(tonic::Request::new(kv)).await;
                        }
                    }
                    EtcdEvents::Mgmt(e) => match e {
                        EtcdMgmtEvent::Config(c) => {
                            let _ = grpc_client.reconfigure(c).await;
                        }
                        e @ _ => {
                            // TODO
                            error!(grpc_client.log, "Not implemented: {:?}", e);
                        }
                    }
                }
            }
        });

        Ok(c)
    }


    // TODO add peers and metric separate listener
    // TODO add multiple bind adr/port for listener
    pub async fn serve(&self) -> Result<(), String> {
        let mut srv = Server::builder();

        let srv = self.add_services(srv.add_service(AuthServer::new(self.clone())), false);
        
        let addrs = self.cfg.read().await.listen_client_urls.clone();
        let addrsv:Vec<&str> = addrs.split(",").collect();
        let adr: Vec<&str> = if addrsv[0].starts_with("http") {
            let url: Vec<&str> = addrsv[0].split("//").collect();
            url[1].split(":").collect()
        } else {
            addrsv[0].split(":").collect()
        };
        let bind = adr[0];
        let adr = if bind.chars().next().unwrap_or(' ').is_numeric() {
            addrsv[0].parse::<SocketAddr>().map_err(|e| format!("parsing {}: {}", addrs, e))?
        } else {
            let port = adr[1].parse::<u16>().map_err(|e| format!("expected port in {}: {}", addrs, e))?;

            let ips: Vec<std::net::IpAddr> = dns_lookup::lookup_host(bind).expect(format!("Binding to {}", bind).as_str());
            if ips.len() == 0 {
                eprintln!("No IpAddr found {}", bind);
                std::process::exit(9);
            }
            SocketAddr::new(ips[0], port)
        };
        let name = self.cfg.read().await.name.clone();

        info!(self.log, "Starting server [{}] at: {}", name, addrs);
        tokio::spawn(async move {
            match srv.serve(adr) // .serve_with_incoming_shutdown(uds_stream, rx.map(drop) )
                .await {
                Ok(()) => {
                    println!("bye");
                }
                Err(e) => {
                    // eprintln!("{} {}", fl!("error"), e);
                    eprintln!("{} {}", "error", e);
                    println!();
                    // println!("{}", arg_config::usage());
                    std::process::exit(10);
                }
            }
        });
        
        Ok(())
    }

    /// in case of use as embedded lib bound to same port
    pub fn add_all_services(&self, srv: Router) -> Router {
        self.add_services(srv, true)
    }

    fn add_services(&self, srv: Router, all: bool) -> Router {
        let srv = if all {
            srv.add_service(AuthServer::new(self.clone()))
        } else {
            srv
        };
        srv.add_service(KvServer::new(self.clone()))
            .add_service(WatchServer::new(self.clone()))
            .add_service(LockServer::new(self.clone()))
            .add_service(ElectionServer::new(self.clone()))
            .add_service(MaintenanceServer::new(self.clone()))
            .add_service(ClusterServer::new(self.clone()))
            .add_service(LeaseServer::new(self.clone()))
    }

    async fn reconfigure(&self, cfg: EtcdConfig) {
        let peers = cfg.peers();
        match self.peers.write().await.add_connections(peers).await {
            Ok(cnt) => if cnt > 0 {
                self.cfg.write().await.initial_advertise_peer_urls = cfg.initial_advertise_peer_urls;
            },
            Err(e) => {
                error!(self.log, "Error adding peers: {}", e);
            }
        }
    }
}

pub type KvKey = Vec<u8>;

pub type NodeId = u64;
pub type ClientId = Uuid;
pub type WatcherId = i64;

/// this node
#[derive(Clone)]
pub struct EtcdNode {
    pub(crate) node_id: NodeId,
    pub(crate) cfg: Arc<RwLock<EtcdConfig>>,
    pub(crate) vault: Arc<RwLock<HashMap<KvKey, crate::kv::Kv>>>,
    pub(crate) queues: Arc<RwLock<HashMap<String, Queue>>>,

    /// watchers links
    pub(crate) observers: Arc<RwLock<HashMap<KvKey, EtcdObserverType>>>,

    /// queue consumers and reqular kv watchers
    pub(crate) watchers: Arc<RwLock<HashMap<ClientId, EtcdClientType>>>,
    pub(crate) watch_notify: Sender<crate::kv::Kv>,

    /// cluster nodes (not clients, see node watchers for clients)
    pub(crate) peers: Arc<RwLock<EtcdCluster>>,
    pub event: Sender<EtcdEvents>,
    pub(crate) log: Logger,

}

pub(crate) type EtcdObserverType = Vec<(ClientId, WatcherId)>;

pub(crate) type EtcdPeerNodeType = Arc<Mutex<EtcdPeerNode>>;

pub(crate) type EtcdClientType = Arc<RwLock<EtcdClientNode>>;
/// remote node info

pub struct EtcdClientNode {
    /// TODO take from auth
    #[allow(dead_code)]
    pub(crate) client_id: ClientId,

    pub(crate) watchers: HashMap<WatcherId, WatcherConsumer>,
    // TODO stat
    // pub(crate) stat: Histogram, //::new(Config::default())
}

/// watcher consumer
pub struct WatcherConsumer {
    pub(crate) key: KvKey,
    pub(crate) client: Sender<Result<WatchResponse, Status>>,

}


#[inline]
fn parse_uuid(value: &String) -> Result<u64, String> {
    let (a, b) = if value.len() > 0 {
        Uuid::from_str(value.as_str())
            .map_err(|e| format!("parsing to uuid: [{}]: {}", value, e))?
    } else {
        Uuid::new_v4()
    }.as_u64_pair();
    Ok(a^b)
}
