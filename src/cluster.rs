use crate::cli::Config;
use crate::etcdpb::etcdserverpb::kv_server::{Kv, KvServer};
use crate::queue::Queue;
use crate::{EtcdEvents, KvEvent};
use slog::{info, Logger};
use std::collections::{HashMap, HashSet};
use std::fmt::format;
use std::net::SocketAddr;
use std::string::FromUtf8Error;
use std::sync::Arc;
use i18n_embed_fl::fl;
use shims::Histogram;
use tokio::sync::mpsc::Sender;
use tokio::sync::{mpsc, Mutex, RwLock};
use tonic::service::Routes;
use tonic::Status;
use tonic::transport::Server;
use tonic::transport::server::Router;
use uuid::Uuid;
use crate::etcdpb::etcdserverpb::auth_server::AuthServer;
use crate::etcdpb::etcdserverpb::cluster_server::ClusterServer;
use crate::etcdpb::etcdserverpb::lease_server::LeaseServer;
use crate::etcdpb::etcdserverpb::maintenance_server::MaintenanceServer;
use crate::etcdpb::etcdserverpb::watch_server::{Watch, WatchServer};
use crate::etcdpb::etcdserverpb::{PutRequest, WatchResponse};
use crate::etcdpb::v3electionpb::election_server::ElectionServer;
use crate::etcdpb::v3lockpb::lock_server::LockServer;
use crate::peer::EtcdCluster;
pub(crate) use crate::peer::EtcdPeerNode;


impl EtcdNode {
    pub async fn init(cfg: Config, log: Logger) -> Result<Self, String> {
        let (a, b) = Uuid::new_v4().as_u64_pair(); // TODO use node name
        let node_id = a^b; 
        let cluster = EtcdCluster::connect(&cfg, node_id, &log).await?;

        let (sender, mut rsvr) = mpsc::channel(10);
        let (watcher, mut watcher_rv) = mpsc::channel(10);

        let c = EtcdNode {
            cfg: Arc::new(RwLock::new(cfg)),
            vault: Arc::new(Default::default()),
            queues: Arc::new(Default::default()),
            observers: Arc::new(Default::default()),
            watchers: Arc::new(Default::default()),
            watch_notify: watcher,
            peers: Arc::new(RwLock::new(cluster)),
            node_id, 
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
                    EtcdEvents::Mgmt(_e) => {
                        // todo! send EtcdMgmtEvent to main working loop
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
    
    /// process queue message from /producer/ (no watcher notify)
    /// will create queue bucket if not exists
    /// The returned queue will call for put() local or remote
    ///
    /// There no watcher will notify on this event because queue perform put with subsequently notification  
    /// 
    pub(crate) async fn queue(&self, r: &PutRequest) -> Result<(Queue, String), ()> {
        let key = String::from_utf8(r.key.clone()).map_err(|_|())?;
        if key.starts_with("/q") { // quick precheck
            let names: Vec<&str> = key.split("/").collect();
            if let Some((prefix, q_key)) = Queue::queue_name(&names) {
                return match self.queues.read().await.get(&q_key).map(|q| q.clone()) {
                    None => {
                        let q = Queue::new(&self, prefix, q_key.clone()).await;
                        self.queues.write().await.insert(q_key, q.clone());
                        // TODO return index on broadcast
                        Ok((q, Queue::get_producer_key(&names)?))
                    }
                    Some(q) => Ok((q, Queue::get_producer_key(&names)?))
                };
            }
        }
        Err(())
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


}

pub type KvKey = Vec<u8>;

pub type NodeId = u64;
pub type ClientId = Uuid;
pub type WatcherId = i64;

/// this node
#[derive(Clone)]
pub struct EtcdNode {
    pub(crate) node_id: NodeId,
    pub(crate) cfg: Arc<RwLock<Config>>,
    pub(crate) vault: Arc<RwLock<HashMap<KvKey, crate::kv::Kv>>>,
    pub(crate) queues: Arc<RwLock<HashMap<String, Queue>>>,

    /// watchers links
    pub(crate) observers: Arc<RwLock<HashMap<KvKey, EtcdObserverType>>>,

    /// queue consumers and reqular kv watchers
    pub(crate) watchers: Arc<RwLock<HashMap<ClientId, EtcdClientType>>>,
    pub(crate) watch_notify: Sender<crate::kv::Kv>,

    /// cluster nodes (not clients, see node watchers for clients)
    pub(crate) peers: Arc<RwLock<EtcdCluster>>,

    pub(crate) log: Logger,

}

pub(crate) type EtcdObserverType = Vec<(ClientId, WatcherId)>;

pub(crate) type EtcdPeerNodeType = Arc<Mutex<EtcdPeerNode>>;

pub(crate) type EtcdClientType = Arc<RwLock<EtcdClientNode>>;
/// remote node info

pub struct EtcdClientNode {
    // pub(crate) watch_id: ClientId,

    pub(crate) watchers: HashMap<WatcherId, WatcherConsumer>,
    // watcher notification
    // pub(crate) stat: Histogram, //::new(Config::default())
}

/// watcher consumer
pub struct WatcherConsumer {
    pub(crate) key: KvKey,
    pub(crate) client: Sender<Result<WatchResponse, Status>>,

}


#[inline]
fn uuid() -> String {
    let x = Uuid::new_v4();
    x.hyphenated().to_string()[..8].into()
}
