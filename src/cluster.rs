use crate::cli::EtcdConfig;
use crate::etcdpb::etcdserverpb::kv_server::{Kv, KvServer};
use crate::etcdpb::etcdserverpb::ResponseHeader;
use crate::queue::{Queue};
use crate::{EtcdEvents, EtcdMgmtEvent, KvEvent};
use rust_i18n::t;
use slog::{warn, error, info, Logger};
use std::collections::{HashMap};
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
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
use crate::LP;
use std::collections::HashSet;
use std::time::Duration;

/// [peer-retry] How often to re-attempt the peers that were not up at startup.
/// Short against a rolling restart, long enough that a permanently absent peer
/// costs one connect attempt a minute rather than a busy loop.
const PEER_RETRY_SECS: u64 = 20;
/// How often to repeat the "no peers configured" warning, in retry ticks:
/// once immediately, then every 10 minutes, so a misconfiguration is visible
/// without filling the log of a node deliberately running standalone.
const EMPTY_PEERS_EVERY: u32 = 30;

pub(crate) use crate::peer::EtcdPeerNode;


impl EtcdNode {
    pub async fn init(cfg: EtcdConfig, log: Logger,
      #[cfg(feature = "tracer")] tracer: Option<opentelemetry_sdk::trace::SdkTracer>,
    ) -> Result<Self, String> {
        let node_id = parse_uuid(&cfg.node)?;
        // [peering] An EMPTY cluster uuid is not "unconfigured", it is "a
        // cluster of one that nobody else can join".
        //
        // `parse_uuid("")` returns `Uuid::new_v4()` - a fresh random value per
        // node, per start. Peering compares cluster ids and refuses a mismatch,
        // so with this unset no two nodes can ever peer, every key stays on the
        // node that wrote it, and the only symptom is a `wrong cluster` line
        // that looks like somebody misconfigured something. Say what it means
        // instead, once, at startup.
        let cluster_id = if cfg.cluster.trim().is_empty() {
            let id = parse_uuid(&cfg.cluster)?;
            error!(log, "{}no cluster uuid configured: this node invented {} for itself, \
                and every other node invents its own - so NO peer will ever be accepted \
                (peering compares cluster ids) and this kv is local to this node. Set the \
                cluster uuid in the configuration to make the etcd surface cluster-wide.",
                LP, id);
            id
        } else {
            parse_uuid(&cfg.cluster)?
        };
        let cluster = EtcdCluster::connect(&cfg, node_id, cluster_id, &log).await?;

        let (event, mut rsvr) = mpsc::channel(10);
        let (watcher, watcher_rv) = mpsc::channel(10);

        let term = Arc::new(AtomicU64::new(cfg.term));
        let policy = crate::prefix::PrefixPolicy::new(
            &cfg.local_prefix, &cfg.local_prefix_except, &cfg.local_only_prefix,
            &cfg.zone_prefix);
        let c = EtcdNode {
            cfg: Arc::new(RwLock::new(cfg)),
            vault: Arc::new(Default::default()),
            queues: Arc::new(Default::default()),
            observers: Arc::new(Default::default()),
            observer_ranges: Arc::new(Default::default()),
            watchers: Arc::new(Default::default()),
            watch_notify: watcher,
            peers: Arc::new(RwLock::new(cluster)),
            policy: Arc::new(RwLock::new(policy)),
            node_id,
            cluster_id,
            term,
            event,
            log: log.clone(),
            #[cfg(feature = "tracer")] tracer: Arc::new(RwLock::new(tracer)),
        };
        // Announce self to peers and sync KV data from fastest responder
        {
            let cfg = c.cfg.read().await;
            let my_urls: Vec<String> = cfg.listen_client_urls
                .split(',')
                .filter(|u| !u.is_empty())
                .map(|u| {
                    if u.starts_with("http") { u.to_string() }
                    else { format!("http://{}", u) }
                })
                .collect();
            drop(cfg);

            match c.peers.read().await.announce_and_sync(my_urls, &c.vault, &log).await {
                Ok(n) => if n > 0 {
                    info!(log, "{}startup sync: loaded {} keys", LP, n);
                },
                Err(e) => info!(log, "{}startup sync skipped: {}", LP, e),
            }
        }

        c.watch_notify(watcher_rv).await;
        // [peer-retry] From `init`, NOT from `serve()`: the embedded path -
        // ytserv - never calls `serve()`. It mounts these services on its own
        // tonic server with `add_all_services`, so anything spawned in `serve()`
        // is dead code for every real cluster node.
        c.spawn_peer_retry();
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
                        #[cfg(feature = "tracer")]
                        EtcdMgmtEvent::Tracer(c) => {
                            *grpc_client.tracer.write().await = c;
                        }
                        EtcdMgmtEvent::Stop => {
                            info!(grpc_client.log, "{}received Stop event, shutting down event loop", LP);
                            break;
                        }
                        EtcdMgmtEvent::Restart => {
                            let new_term = grpc_client.term.fetch_add(1, Ordering::Relaxed) + 1;
                            info!(grpc_client.log, "{}restart: term incremented to {}", LP, new_term);
                            let cfg = grpc_client.cfg.read().await;
                            let my_urls: Vec<String> = cfg.listen_client_urls
                                .split(',')
                                .filter(|u| !u.is_empty())
                                .map(|u| {
                                    if u.starts_with("http") { u.to_string() }
                                    else { format!("http://{}", u) }
                                })
                                .collect();
                            drop(cfg);
                            match grpc_client.peers.read().await
                                .announce_and_sync(my_urls, &grpc_client.vault, &grpc_client.log).await {
                                Ok(n) => if n > 0 {
                                    info!(grpc_client.log, "{}restart sync: loaded {} keys", LP, n);
                                },
                                Err(e) => info!(grpc_client.log, "{}restart sync skipped: {}", LP, e),
                            }
                        }
                        EtcdMgmtEvent::Pause(secs) => {
                            info!(grpc_client.log, "{}pausing for {} seconds", LP, secs);
                            tokio::time::sleep(tokio::time::Duration::from_secs(secs as u64)).await;
                            info!(grpc_client.log, "{}resumed after pause", LP);
                        }
                    }
                }
            }
        });

        Ok(c)
    }


    // TODO add peers and metric separate listener
    // TODO add multiple bind adr/port for listener
    /// [peer-retry] Keep trying to reach the peers that were not up when this
    /// node started.
    ///
    /// `EtcdCluster::connect` runs ONCE, from `init`, and there was no retry of
    /// any kind - so a peer that was not listening at that instant was never
    /// connected again for the life of the process. Nodes of one cluster
    /// normally start TOGETHER (a coordinated restart, a host reboot, `docker
    /// restart` across the fixture), which is exactly the case where every node
    /// is still binding its port while every other node tries to reach it: all
    /// the connects fail with `transport error`, every peer list stays empty,
    /// and the kv is per-node forever. The symptom is silence - an empty peer
    /// list makes `broadcast_scoped` return `Ok(())` immediately.
    ///
    /// `add_connections` is already idempotent: it drops the urls it is holding
    /// and returns how many it added, so calling it on a tick is the whole fix.
    fn spawn_peer_retry(&self) {
        let node = self.clone();
        tokio::spawn(async move {
            // consecutive ticks with nothing configured to connect to
            let mut quiet: u32 = 0;
            loop {
                tokio::time::sleep(Duration::from_secs(PEER_RETRY_SECS)).await;
                let urls: Vec<String> = {
                    let cfg = node.cfg.read().await;
                    cfg.peers().iter().map(|s| s.to_string()).collect()
                };
                let have = node.peers.read().await.connected();
                // An EMPTY peer list is the one failure with no symptom at all:
                // `broadcast_scoped` returns Ok(()) immediately on it, so every
                // put succeeds locally and no key ever leaves the node. It is
                // also reachable by configuration alone - `peers()` reads
                // `initial_advertise_peer_urls` and keeps only entries that
                // start with `http`, so a peer list in the wrong field, or one
                // without a scheme, silently becomes no peers at all. Say so
                // rather than `continue`, but not every tick.
                if urls.is_empty() {
                    quiet = quiet.saturating_add(1);
                    if quiet == 1 || quiet % EMPTY_PEERS_EVERY == 0 {
                        let cfg = node.cfg.read().await;
                        warn!(node.log, "{}no etcd peers to connect to, so every key written \
                            here stays here. peers() takes initial_advertise_peer_urls \
                            [{}] minus listen_client_urls [{}] and keeps only http:// \
                            entries.", LP, cfg.initial_advertise_peer_urls, cfg.listen_client_urls);
                    }
                    continue;
                }
                quiet = 0;
                if have >= urls.len() {
                    continue;   // everyone connected
                }
                let set: HashSet<&str> = urls.iter().map(|s| s.as_str()).collect();
                // [lockup] dialled with the peer lock released - see `add_peers`
                match node.add_peers(set).await {
                    n if n > 0 => info!(node.log, "{}peer retry: connected {} more peer(s), \
                        {} of {} total", LP, n, node.peers.read().await.connected(), urls.len()),
                    // At WARN, not debug: this is a cluster that cannot share a
                    // key, and the fixture (and the guide) run at log_level
                    // "warning" - a diagnosis nobody can see is not one.
                    _ => warn!(node.log, "{}peer retry: still {} of {} peer(s) connected, \
                        none of {:?} answered", LP, have, urls.len(), urls),
                }
            }
        });
    }

    pub async fn serve(&self) -> Result<(), String> {
        let mut srv = Server::builder();

        let srv = self.add_services(srv.add_service(AuthServer::new(self.clone())), false);

        let addrs = self.cfg.read().await.listen_client_urls.clone();
        let addrsv: Vec<&str> = addrs.split(",").collect();
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

            let ips: Vec<std::net::IpAddr> = dns_lookup::lookup_host(bind).expect(format!("Binding to {}", bind).as_str()).collect();
            if ips.len() == 0 {
                return Err(format!("No IpAddr found {}", bind));
            }
            SocketAddr::new(ips[0], port)
        };
        let name = self.cfg.read().await.name.clone();

        info!(self.log, "{}Starting server [{}] at: {}", LP, name, addrs);
        tokio::spawn(async move {
            match srv.serve(adr) // .serve_with_incoming_shutdown(uds_stream, rx.map(drop) )
                .await {
                Ok(()) => {
                    println!("bye");
                }
                Err(e) => {
                    eprintln!("{} {}", t!("error"), e);
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
        // [lockup] dialled with the peer lock released - see `add_peers`
        let cnt = self.add_peers(peers).await;
        if cnt > 0 {
            info!(self.log, "{}reconfigure: added {} peer(s)", LP, cnt);
        }

        if cfg.term > 0 {
            self.term.store(cfg.term, Ordering::Relaxed);
        }

        let mut current = self.cfg.write().await;
        current.initial_advertise_peer_urls = cfg.initial_advertise_peer_urls;
        current.listen_client_urls = cfg.listen_client_urls;
        current.listen_peer_urls = cfg.listen_peer_urls;
        if !cfg.clickhouse_url.is_empty() {
            current.clickhouse_url = cfg.clickhouse_url;
            current.clickhouse_db = cfg.clickhouse_db;
        }
        current.initial_cluster = cfg.initial_cluster;
        current.initial_cluster_token = cfg.initial_cluster_token;
        current.name = cfg.name;
        current.max_txn_ops = cfg.max_txn_ops;
        current.max_request_bytes = cfg.max_request_bytes;
        current.log_level = cfg.log_level;
        current.term = cfg.term;
        current.local_prefix = cfg.local_prefix;
        current.local_prefix_except = cfg.local_prefix_except;
        current.local_only_prefix = cfg.local_only_prefix;
        // Rebuilt here and only here, so the cache cannot drift from the config
        // it was built from.
        current.zone_prefix = cfg.zone_prefix;
        current.zone = cfg.zone;
        current.peer_zones = cfg.peer_zones;
        *self.policy.write().await = crate::prefix::PrefixPolicy::new(
            &current.local_prefix, &current.local_prefix_except, &current.local_only_prefix,
            &current.zone_prefix);
        // Labels come from configuration, so they are re-applied wherever the
        // config is: a peer added after the last reconfigure would otherwise
        // carry no zone and be treated as sharing ours.
        self.peers.write().await.apply_zones(&current.zone, &current.peer_zones);
    }

    /// [lockup] Send to the peers, with the peer lock held only while choosing
    /// them - never across a network wait. See `peer::BroadcastPlan`.
    pub(crate) async fn broadcast_scoped(&self, request: crate::peer::BroadcastRequest, zone_scoped: Option<&str>)
        -> Result<(), Status>
    {
        let plan = self.peers.read().await.plan(zone_scoped).await;
        plan.send(request).await
    }

    /// [lockup] `broadcast_scoped` to every zone.
    pub(crate) async fn broadcast(&self, request: crate::peer::BroadcastRequest) -> Result<(), Status> {
        self.broadcast_scoped(request, None).await
    }

    /// [lockup] One request to one `Online` peer, the lock released before it
    /// is sent. `false` when the peer is not held, not Online, excluded by the
    /// zone test, or did not answer.
    pub(crate) async fn unicast(&self, request: crate::peer::BroadcastRequest, peer_id: NodeId,
        zone_scoped: Option<&str>) -> bool
    {
        let (target, me) = {
            let peers = self.peers.read().await;
            (peers.unicast_target(peer_id, zone_scoped).await, peers.me())
        };
        let Some(target) = target else { return false };
        let id = tonic::metadata::MetadataValue::from_str(me.to_string().as_str()).ok();
        EtcdCluster::peer_request(request, target, id).await
    }

    /// [lockup] Dial `clients` with the peer lock released, then hold the ones
    /// that answered - `add_connections` did the dialling inside `peers.write()`,
    /// so every broadcast, put and member list waited on the network. Returns how
    /// many were added.
    pub(crate) async fn add_peers(&self, clients: HashSet<&str>) -> usize {
        let (urls, (cluster_id, timeout_ms, log)) = {
            let peers = self.peers.read().await;
            (peers.not_held(clients).await, peers.dial_params())
        };
        if urls.is_empty() {
            return 0;
        }
        let dialled = EtcdCluster::dial(urls, cluster_id, timeout_ms, &log).await;
        self.peers.write().await.admit(dialled).await
    }

    pub(crate) fn response_header(&self) -> ResponseHeader {
        ResponseHeader {
            cluster_id: self.cluster_id,
            member_id: self.node_id,
            revision: 0,
            raft_term: self.term.load(Ordering::Relaxed),
        }
    }

    /// return current cluster connections
    // ── embedder API ─────────────────────────────────────────────────────
    //
    // In-process access for a host application. These go through the same
    // `*_impl` paths a gRPC call does, so the propagation and zone rules apply
    // identically — an embedder cannot bypass a policy by not using the wire.
    //
    // What they *do* bypass is `deny_remote`, and correctly: an in-process
    // caller has no socket address, which the policy already treats as local.

    /// Put a key. Propagation follows the prefix policy.
    pub async fn kv_put(&self, key: Vec<u8>, value: Vec<u8>) -> Result<(), tonic::Status> {
        let req = crate::etcdpb::etcdserverpb::PutRequest {
            key, value, lease: 0, prev_kv: false, ignore_value: false, ignore_lease: false,
        };
        self.put_impl(tonic::Request::new(req)).await.map(|_| ())
    }

    /// Read one key, or `None` when it is absent.
    ///
    /// Reads the vault directly rather than going through `get_impl`: an
    /// embedder wants the value, not a `RangeResponse` to unwrap.
    pub async fn kv_get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.vault.read().await.get(key).map(|kv| kv.value.clone())
    }

    /// [q-route Phase 3] Declare that `request_q` is answered on `reply_q`.
    ///
    /// The pair is then scheduled together: the reply queue's dispatcher is
    /// placed on the requester's node, so the reply leg is one hop instead of
    /// two. Idempotent, and an optimization only — an undeclared pair still
    /// works, at Phase 1/2 cost.
    ///
    /// An embedder API rather than something inferred from queue names: a
    /// convention like `<x>-reply-<client>` would be a guess, and a guess that
    /// moved a dispatcher is worse than no optimization at all.
    pub async fn queue_link(&self, request_q: &str, reply_q: &str) {
        let qn = crate::queue::QueueNameKey::new(request_q.to_string());
        let q = self.queues.read().await.get(&qn.queue_name).cloned();
        if let Some(q) = q {
            q.link_reply_to(reply_q, &self.log).await;
        }
    }

    /// [q-route] Which node dispatches this queue, if the registry has an
    /// answer. For `yt_info` and the topology tests: a routing decision nobody
    /// can observe is a routing decision nobody can debug.
    pub async fn queue_dispatcher(&self, q_name: &str) -> Option<NodeId> {
        let qn = crate::queue::QueueNameKey::new(q_name.to_string());
        let key = crate::route::dispatcher_key(&qn.name());
        self.kv_get(key.as_bytes()).await
            .and_then(|v| crate::route::DispatchRecord::parse(&String::from_utf8_lossy(&v)))
            .map(|r| r.node)
    }

    /// Delete a key. Propagation follows the prefix policy.
    pub async fn kv_delete(&self, key: Vec<u8>) -> Result<(), tonic::Status> {
        let req = crate::etcdpb::etcdserverpb::DeleteRangeRequest {
            key, range_end: vec![], prev_kv: false,
        };
        self.delete_impl(tonic::Request::new(req)).await.map(|_| ())
    }

    /// Every key under a prefix, with its value.
    ///
    /// A snapshot: the lock is released before returning, so a caller iterating
    /// the result is not holding the vault against every writer in the process.
    pub async fn kv_prefix(&self, prefix: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
        self.vault.read().await.iter()
            .filter(|(k, _)| k.starts_with(prefix))
            .map(|(k, v)| (k.clone(), v.value.clone()))
            .collect()
    }

    pub async fn get_peer_urls(&self) -> String {
        self.peers.read().await.peer_urls().await
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
    /// [peering] This node's cluster id, as reported in every ResponseHeader.
    ///
    /// It has to be REPORTED, not merely held: `EtcdPeers::connect` calls
    /// `status()` on a prospective peer and refuses it when
    /// `header.cluster_id != self.cluster_id`. `response_header()` returned a
    /// hardcoded `0`, so every peer answered "cluster 0", every comparison
    /// failed, and no two embedded etcds ever peered - logged once per peer per
    /// attempt as `wrong cluster, running on ClusterID [N], but connecting node
    /// from 0`, which reads as a misconfiguration rather than as the code
    /// always saying zero.
    ///
    /// The consequence is the whole "cluster kv" claim: with no peers,
    /// `broadcast_scoped` returns early on an empty peer list, a key written on
    /// one node is readable on that node alone, and service discovery or leader
    /// election built on it sees a different world per node.
    pub(crate) cluster_id: NodeId,
    pub(crate) term: Arc<AtomicU64>,
    pub(crate) cfg: Arc<RwLock<EtcdConfig>>,
    pub(crate) vault: Arc<RwLock<HashMap<KvKey, crate::kv::Kv>>>,
    pub(crate) queues: Arc<RwLock<HashMap<String, Queue>>>,

    /// watchers links
    pub(crate) observers: Arc<RwLock<HashMap<KvKey, EtcdObserverType>>>,

    /// [range] `start key -> range_end`, for the watchers registered over a
    /// RANGE rather than one key.
    ///
    /// `observers` is keyed by the exact key a watcher asked for, and
    /// `watch_notify` looked the changed key up in it directly - so a watch on
    /// the prefix `/q/` sat under the literal `/q/` and a put to `/q/x` matched
    /// nothing. Every prefix watch was silent, which is how a client asks for
    /// change streaming and how rppd registers a queue consumer (`schema_table`
    /// beginning with `/`).
    ///
    /// Kept beside `observers` rather than folded into it so the exact-key
    /// lookup stays the fast path it was; only ranges are scanned.
    pub(crate) observer_ranges: Arc<RwLock<HashMap<KvKey, Vec<u8>>>>,

    /// queue consumers and reqular kv watchers
    pub(crate) watchers: Arc<RwLock<HashMap<ClientId, EtcdClientType>>>,
    pub(crate) watch_notify: Sender<crate::kv::Kv>,

    /// cluster nodes (not clients, see node watchers for clients)
    pub(crate) peers: Arc<RwLock<EtcdCluster>>,

    /// Which keys stay on this node, and which are served only locally.
    ///
    /// Cached rather than re-parsed per request: `put_impl` is on the path of
    /// every key, queues included, and three allocations per put to answer a
    /// question whose answer changes only on reconfigure is a cost with no
    /// return. Rebuilt wherever the config is applied.
    pub(crate) policy: Arc<RwLock<crate::prefix::PrefixPolicy>>,
    pub event: Sender<EtcdEvents>,
    pub(crate) log: Logger,
    #[cfg(feature = "tracer")] pub(crate) tracer: Arc<RwLock<Option<opentelemetry_sdk::trace::SdkTracer>>>

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

#[cfg(test)]
mod header_tests {
	//! [peering] The status header is how a peer decides whether to join.
	//!
	//! `EtcdPeers::connect` refuses a peer when `header.cluster_id` differs from
	//! its own, and `response_header()` reported a hardcoded `0` - so every node
	//! advertised "cluster 0", every check failed, and no two embedded etcds ever
	//! peered. The log said `wrong cluster, running on ClusterID [N], but
	//! connecting node from 0` once per attempt, which reads as somebody else's
	//! misconfiguration.
	use super::*;

	async fn node(cluster: &str, node: &str) -> EtcdNode {
		let mut cfg = crate::cli::EtcdConfig::with_defaults();
		cfg.cluster = cluster.to_string();
		cfg.node = node.to_string();
		cfg.listen_client_urls = "localhost:0".to_string();
		EtcdNode::init(cfg, slog::Logger::root(slog::Discard, slog::o!()),
			#[cfg(feature = "tracer")] None).await.unwrap()
	}

	#[tokio::test]
	async fn the_header_reports_this_node_s_cluster() {
		let n = node("11111111-1111-5111-8111-111111111111",
		             "22222222-2222-5222-8222-222222222222").await;
		let h = n.response_header();
		assert_ne!(h.cluster_id, 0, "a zero cluster id is what made every peer refuse every peer");
		assert_eq!(h.cluster_id, n.cluster_id);
		assert_eq!(h.member_id, n.node_id, "and the member id stays this node's own");
	}

	#[tokio::test]
	async fn two_nodes_of_one_cluster_advertise_the_same_id() {
		// the comparison EtcdPeers::connect actually makes
		let a = node("11111111-1111-5111-8111-111111111111",
		             "22222222-2222-5222-8222-222222222222").await;
		let b = node("11111111-1111-5111-8111-111111111111",
		             "33333333-3333-5333-8333-333333333333").await;
		assert_eq!(a.response_header().cluster_id, b.response_header().cluster_id,
			"same cluster -> the peer check must pass");
		assert_ne!(a.response_header().member_id, b.response_header().member_id,
			"...while still being distinguishable nodes");
	}

	#[tokio::test]
	async fn a_different_cluster_still_reports_a_different_id() {
		// the check has to keep REFUSING a stranger, or the fix traded one bug
		// for a worse one
		let a = node("11111111-1111-5111-8111-111111111111",
		             "22222222-2222-5222-8222-222222222222").await;
		let c = node("44444444-4444-5444-8444-444444444444",
		             "55555555-5555-5555-8555-555555555555").await;
		assert_ne!(a.response_header().cluster_id, c.response_header().cluster_id);
	}
}

#[cfg(test)]
mod peering_tests {
	//! [peering] A connected peer has to end up in a state that `broadcast_scoped`
	//! actually sends to.
	//!
	//! `add_connections` pushed every verified peer as `PeerState::Spare`, and
	//! `broadcast_scoped` builds `send_indices` from `Online` + `InProgress`
	//! only. The single thing that promoted a peer was the `member_promote`
	//! gRPC admin call, which no embedded deployment makes - so every node
	//! connected to every other node, every put succeeded, `send_indices` was
	//! empty, the broadcast returned `Ok(())`, and the kv was per-node with
	//! nothing logged anywhere.
	//!
	//! The header tests above cover whether a peer is ACCEPTED. These cover
	//! what happens to it afterwards, which is where it went wrong.
	use super::*;
	use crate::etcdpb::etcdserverpb::PutRequest;
	use crate::peer::PeerState;

	const CLUSTER: &str = "11111111-1111-5111-8111-111111111111";

	/// Minimal drain: `add_connections` reports every refusal through `error!`
	/// and returns `Ok(0)`, so a Discard logger turns a diagnosable failure into
	/// an unexplained zero.
	struct Print;
	impl slog::Drain for Print {
		type Ok = ();
		type Err = slog::Never;
		fn log(&self, r: &slog::Record, _: &slog::OwnedKVList) -> Result<(), slog::Never> {
			eprintln!("[{}] {}", r.level(), r.msg());
			Ok(())
		}
	}

	fn test_log() -> slog::Logger {
		slog::Logger::root(Print, slog::o!())
	}

	fn free_port() -> u16 {
		let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
		let p = l.local_addr().unwrap().port();
		drop(l);
		p
	}

	async fn node(node_uuid: &str, listen: &str, peers: &str) -> EtcdNode {
		let mut cfg = crate::cli::EtcdConfig::with_defaults();
		cfg.cluster = CLUSTER.to_string();
		cfg.node = node_uuid.to_string();
		cfg.listen_client_urls = listen.to_string();
		cfg.initial_advertise_peer_urls = peers.to_string();
		EtcdNode::init(cfg, test_log(),
			#[cfg(feature = "tracer")] None).await.unwrap()
	}

	/// Bring up a served node A and a node B pointed at it, with B connected.
	async fn pair() -> (EtcdNode, EtcdNode, String) {
		let port = free_port();
		let a_url = format!("http://127.0.0.1:{}", port);
		let a = node("22222222-2222-5222-8222-222222222222",
		             &format!("127.0.0.1:{}", port), "").await;

		let served = a.clone();
		tokio::spawn(async move { let _ = served.serve().await; });
		// let the listener bind before anyone dials it
		for _ in 0..50 {
			if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() { break; }
			tokio::time::sleep(Duration::from_millis(20)).await;
		}

		// B connects to A during `init` itself, from initial_advertise_peer_urls -
		// so a second add_connections correctly returns 0 (it drops urls it
		// already holds). Assert the PEER, not the count.
		let b = node("33333333-3333-5333-8333-333333333333",
		             "127.0.0.1:0", &a_url).await;
		let _ = b.peers.write().await
			.add_connections(HashSet::from([a_url.as_str()])).await.unwrap();
		let held = b.peers.read().await.peer_info().await;
		assert_eq!(held.len(), 1, "B must hold the served A as a peer (same cluster uuid)");
		(a, b, a_url)
	}

	#[tokio::test]
	async fn a_connected_peer_is_not_left_in_a_state_nothing_sends_to() {
		let (_a, b, _) = pair().await;
		let states: Vec<PeerState> =
			b.peers.read().await.peer_info().await.into_iter().map(|(_, _, s)| s).collect();
		assert_eq!(states.len(), 1);
		assert_ne!(states[0], PeerState::Spare,
			"a verified, connected peer left Spare receives nothing from \
			 broadcast_scoped, forever - this is the bug");
	}

	#[tokio::test]
	async fn a_key_written_on_one_node_reaches_the_other() {
		// The whole point of peering, and the assertion 93_etcd.sh makes across
		// containers. If this passes and that fails, the difference is the
		// network, not the state machine.
		let (a, b, _) = pair().await;
		b.put(tonic::Request::new(PutRequest {
			key: b"/peering/k".to_vec(),
			value: b"v".to_vec(),
			..Default::default()
		})).await.unwrap();

		let mut seen = None;
		for _ in 0..50 {
			if let Some(kv) = a.vault.read().await.get(&b"/peering/k".to_vec()) {
				seen = Some(kv.value.clone());
				break;
			}
			tokio::time::sleep(Duration::from_millis(20)).await;
		}
		assert_eq!(seen.as_deref(), Some(&b"v"[..]),
			"a put on B must replicate to A - it did not, so broadcast_scoped \
			 found nothing to send to");
	}

	#[tokio::test]
	async fn replicating_successfully_promotes_the_peer_to_online() {
		// InProgress receives writes but does not gate the commit; Online does.
		// Nothing advanced a peer between them outside the admin API.
		let (_a, b, _) = pair().await;
		b.put(tonic::Request::new(PutRequest {
			key: b"/peering/p".to_vec(),
			value: b"v".to_vec(),
			..Default::default()
		})).await.unwrap();

		let mut state = None;
		for _ in 0..50 {
			let s = b.peers.read().await.peer_info().await
				.into_iter().map(|(_, _, s)| s).next();
			if s == Some(PeerState::Online) { state = s; break; }
			tokio::time::sleep(Duration::from_millis(20)).await;
		}
		assert_eq!(state, Some(PeerState::Online),
			"a peer that replicated a message has proven what Online means");
	}

	/// [lockup] The lab's synchronized restart, driven the way ytserv drives etcd:
	/// every node starts with NO peers (ytserv's `init` passes none), is served, and
	/// only then learns its peers - from `sync_etcd_peers`, a `Config` event naming
	/// all three plus a `/cluster/peers/*` put per node - on every node at once.
	/// Afterwards every node must still answer, and a key written on any node must
	/// reach the other two. On the lab two of three nodes stopped answering
	/// `member_list` and a put on one of them hung and never replicated.
	#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
	async fn three_nodes_peering_at_once_stay_live() {
		use crate::{EtcdEvents, EtcdMgmtEvent, KvEvent};
		let ports: Vec<u16> = (0..3).map(|_| free_port()).collect();
		let urls: Vec<String> = ports.iter().map(|p| format!("http://127.0.0.1:{}", p)).collect();
		let ids = ["44444444-4444-5444-8444-444444444441", "44444444-4444-5444-8444-444444444442",
		           "44444444-4444-5444-8444-444444444443"];
		let mut nodes = Vec::new();
		for i in 0..3 {
			let n = node(ids[i], &format!("127.0.0.1:{}", ports[i]), "").await;
			let served = n.clone();
			tokio::spawn(async move { let _ = served.serve().await; });
			nodes.push(n);
		}
		for p in &ports {
			for _ in 0..50 {
				if std::net::TcpStream::connect(("127.0.0.1", *p)).is_ok() { break; }
				tokio::time::sleep(Duration::from_millis(20)).await;
			}
		}
		// what ytserv's sync_etcd_peers sends, to every node at once - twice, as a
		// roster that fills in one peer at a time sends it again
		let all = urls.join(",");
		for round in 0..2 {
			let mut sends = Vec::new();
			for (i, n) in nodes.iter().enumerate() {
				let ev = n.event.clone();
				let (me, all, urls) = (urls[i].clone(), all.clone(), urls.clone());
				sends.push(tokio::spawn(async move {
					let mut c = crate::cli::EtcdConfig::default();
					c.listen_client_urls = me;
					c.initial_advertise_peer_urls = all.clone();
					c.listen_peer_urls = all;
					ev.send(EtcdEvents::Mgmt(EtcdMgmtEvent::Config(c))).await.unwrap();
					for (j, u) in urls.iter().enumerate() {
						ev.send(EtcdEvents::Data(KvEvent::Put(PutRequest {
							key: format!("/cluster/peers/{}", j).into_bytes(),
							value: format!("{} r{}", u, round).into_bytes(),
							..Default::default()
						}))).await.unwrap();
					}
				}));
			}
			for s in sends { s.await.unwrap(); }
		}
		tokio::time::sleep(Duration::from_millis(500)).await;

		// every node still answers: the peer set can be read
		for (i, n) in nodes.iter().enumerate() {
			let held = tokio::time::timeout(Duration::from_secs(3), async {
				n.peers.read().await.peer_info().await.len()
			}).await;
			assert!(held.is_ok(), "node {} no longer answers: its peer set stays locked", i);
		}
		// and a key written on any node reaches the other two
		for (i, n) in nodes.iter().enumerate() {
			let key = format!("/live/{}", i).into_bytes();
			let put = tokio::time::timeout(Duration::from_secs(5), n.put(tonic::Request::new(PutRequest {
				key: key.clone(), value: b"v".to_vec(), ..Default::default()
			}))).await;
			assert!(put.is_ok(), "a put on node {} hangs", i);
			for (j, m) in nodes.iter().enumerate() {
				let mut seen = false;
				for _ in 0..100 {
					if m.vault.read().await.contains_key(&key) { seen = true; break; }
					tokio::time::sleep(Duration::from_millis(20)).await;
				}
				assert!(seen, "the key written on node {} never reached node {}", i, j);
			}
		}
	}

	/// [lockup] The same start with what rppd adds on the lab: a queue consumer on
	/// every node (`/q/analytics/consumer/<client>`, registered through
	/// `new_watcher` as embedded rppd does) and queue messages produced on every
	/// node while the peers are still being learned.
	#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
	async fn three_nodes_with_queue_consumers_stay_live() {
		use crate::{EtcdEvents, EtcdMgmtEvent, KvEvent};
		use crate::etcdpb::etcdserverpb::{WatchRequest, WatchCreateRequest, watch_request};
		let ports: Vec<u16> = (0..3).map(|_| free_port()).collect();
		let urls: Vec<String> = ports.iter().map(|p| format!("http://127.0.0.1:{}", p)).collect();
		let ids = ["55555555-5555-5555-8555-555555555551", "55555555-5555-5555-8555-555555555552",
		           "55555555-5555-5555-8555-555555555553"];
		let mut nodes = Vec::new();
		for i in 0..3 {
			let n = node(ids[i], &format!("127.0.0.1:{}", ports[i]), "").await;
			let served = n.clone();
			tokio::spawn(async move { let _ = served.serve().await; });
			nodes.push(n);
		}
		for p in &ports {
			for _ in 0..50 {
				if std::net::TcpStream::connect(("127.0.0.1", *p)).is_ok() { break; }
				tokio::time::sleep(Duration::from_millis(20)).await;
			}
		}
		// a consumer per node, the way embedded rppd registers one
		let mut keep = Vec::new();
		for (i, n) in nodes.iter().enumerate() {
			let client = uuid::Uuid::parse_str(&format!("66666666-6666-5666-8666-66666666666{}", i)).unwrap();
			let (req_tx, req_rx) = tokio::sync::mpsc::channel(100);
			let (resp_tx, mut resp_rx) = tokio::sync::mpsc::channel(100);
			n.new_watcher(req_rx, resp_tx, &client).await;
			req_tx.send(WatchRequest { request_union: Some(watch_request::RequestUnion::CreateRequest(
				WatchCreateRequest { key: format!("/q/analytics/consumer/{}", client).into_bytes(), ..Default::default() }
			)) }).await.unwrap();
			tokio::spawn(async move { while resp_rx.recv().await.is_some() {} });
			keep.push(req_tx);
		}
		// peers learned and messages produced, on every node at once
		let all = urls.join(",");
		let mut sends = Vec::new();
		for (i, n) in nodes.iter().enumerate() {
			let ev = n.event.clone();
			let (me, all, urls) = (urls[i].clone(), all.clone(), urls.clone());
			sends.push(tokio::spawn(async move {
				for round in 0..2 {
					let mut c = crate::cli::EtcdConfig::default();
					c.listen_client_urls = me.clone();
					c.initial_advertise_peer_urls = all.clone();
					c.listen_peer_urls = all.clone();
					ev.send(EtcdEvents::Mgmt(EtcdMgmtEvent::Config(c))).await.unwrap();
					for (j, u) in urls.iter().enumerate() {
						ev.send(EtcdEvents::Data(KvEvent::Put(PutRequest {
							key: format!("/cluster/peers/{}", j).into_bytes(),
							value: format!("{} r{}", u, round).into_bytes(), ..Default::default()
						}))).await.unwrap();
						ev.send(EtcdEvents::Data(KvEvent::Put(PutRequest {
							key: format!("/q/analytics/p/m{}-{}-{}", i, j, round).into_bytes(),
							value: b"msg".to_vec(), ..Default::default()
						}))).await.unwrap();
					}
				}
			}));
		}
		for s in sends {
			tokio::time::timeout(Duration::from_secs(10), s).await
				.expect("sending the peering events blocked: an event loop is stuck").unwrap();
		}
		tokio::time::sleep(Duration::from_millis(500)).await;
		for (i, n) in nodes.iter().enumerate() {
			let held = tokio::time::timeout(Duration::from_secs(3), async {
				n.peers.read().await.peer_info().await.len()
			}).await;
			assert!(held.is_ok(), "node {} no longer answers: its peer set stays locked", i);
		}
		for (i, n) in nodes.iter().enumerate() {
			let key = format!("/live2/{}", i).into_bytes();
			let put = tokio::time::timeout(Duration::from_secs(5), n.put(tonic::Request::new(PutRequest {
				key: key.clone(), value: b"v".to_vec(), ..Default::default()
			}))).await;
			assert!(put.is_ok(), "a put on node {} hangs", i);
			for (j, m) in nodes.iter().enumerate() {
				let mut seen = false;
				for _ in 0..100 {
					if m.vault.read().await.contains_key(&key) { seen = true; break; }
					tokio::time::sleep(Duration::from_millis(20)).await;
				}
				assert!(seen, "the key written on node {} never reached node {}", i, j);
			}
		}
		drop(keep);
	}

	/// [lockup] A peer that does not answer must not stop this node.
	///
	/// The shape of the lab lock-up, made deterministic: B's put replicates to A,
	/// and A never answers (the test holds A's store, so A's put handler waits).
	/// While that broadcast is in flight a writer arrives on B's peer set - what
	/// `reconfigure` and the peer retry are - and then a reader: a member list,
	/// the next put. With the broadcast inside `peers.read()`, the fair lock put
	/// the writer behind it and the reader behind the writer, and none of them
	/// ever finished - while no peer is `Online` (every peer, after a start) a
	/// broadcast had no deadline at all.
	#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
	async fn a_peer_that_does_not_answer_does_not_stop_this_node() {
		let (a, b, _) = pair().await;
		let stuck = a.vault.write().await;
		let b2 = b.clone();
		let put = tokio::spawn(async move {
			b2.put(tonic::Request::new(PutRequest {
				key: b"/lockup/k".to_vec(), value: b"v".to_vec(), ..Default::default()
			})).await
		});
		tokio::time::sleep(Duration::from_millis(300)).await; // the broadcast is waiting on A
		let b3 = b.clone();
		let writer = tokio::spawn(async move { let _w = b3.peers.write().await; });
		tokio::time::sleep(Duration::from_millis(100)).await; // the writer is queued
		let read = tokio::time::timeout(Duration::from_secs(2), async {
			b.peers.read().await.connected()
		}).await;
		assert!(read.is_ok(), "B's peer set stayed locked while a broadcast waited on a silent peer");
		assert!(tokio::time::timeout(Duration::from_secs(2), writer).await.is_ok(),
			"the writer never got B's peer set");
		let done = tokio::time::timeout(crate::peer::PEER_RPC_TIMEOUT + Duration::from_secs(3), put).await;
		assert!(done.is_ok(), "the put waited on the silent peer past PEER_RPC_TIMEOUT");
		drop(stuck);
	}
}
