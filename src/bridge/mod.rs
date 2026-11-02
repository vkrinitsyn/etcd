//! [bridge] Feature `bridge` (bridge.md): this node's queue or key prefix sent to
//! a broker, or JMS clients served as an AMQP peer. [`node`] reads the
//! `bridge_*` settings for the binary; [`broker`] holds the brokers, [`listener`]
//! the AMQP peer, [`mapping`] the JMS message and [`cloudevents`] its attributes.
//! The bridge is a client of the node like any other, over gRPC - or, in the
//! process that runs the node (an embedder's, as ytserv), of that node itself
//! ([`Config::node`]): no connection, so no TLS to meet and no listener to dial.
//!
//! The bridge: an etcd queue consumer that sends each message to a broker -
//! AMQP 1.0 (JMS), MQTT 5 or Kafka ([`crate::bridge::broker`]) - and acknowledges it
//! in etcd only after the broker took it.
//!
//! Two tasks joined by a bounded channel, so the watch is always read:
//!
//! - **watch** consumes `/q/{queue}/c/{consumer}` and passes each delivery on.
//!   While the channel is full it waits; etcd then drops it as a slow consumer
//!   (ETCD-E052, a `canceled` frame) and it watches again once there is room.
//!   What it holds stays held meanwhile.
//! - **forward** sends one message at a time, in the order etcd delivered them,
//!   and deletes its delivery key when the broker took it. A
//!   broker that is down, refuses for now, or does not answer is retried, 1 s
//!   doubling to 60 s, and nothing is acknowledged: etcd hands a message out
//!   once per stream, so the bridge keeps it until it lands.
//!
//! A message no broker will ever take - refused as malformed or too large, or
//! a value that cannot take the body type asked for - goes to the dead-letter
//! queue when one is set, and is acknowledged after that; without one it is
//! retried like any other failure. Any other refusal (a full address, an
//! unknown reason) is retried.
//!
//! A new etcd stream (the old one failed) gets everything unacknowledged again
//! from etcd, so what the forward task still holds from the old stream is
//! dropped, not sent. Delivery is **at least once**: a message the broker took
//! whose acknowledgement did not reach etcd is sent again.
//!
//! Every network wait has a deadline: the etcd connection is probed by HTTP/2
//! keepalive, and the broker's by its own (AMQP idle timeout, MQTT keep alive),
//! so a peer that vanished without closing its socket is noticed and connected
//! again.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use etcd_client::{Client, ConnectOptions, EventType, WatchOptions, WatchStream};
use slog::{Logger, debug, error, info, warn};
use tokio::sync::{Mutex, mpsc};
use uuid::Uuid;

pub mod cloudevents;
pub mod listener;
pub mod mapping;
pub mod node;
pub mod broker;

use crate::bridge::mapping::{Delivery, KvEvent, Shape};
use crate::bridge::broker::{Kind, Sent, Target};
use crate::etcdpb::etcdserverpb::{watch_request, WatchCreateRequest, WatchRequest};

const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// The first pause before watching again after a cancel; it doubles while
/// cancels repeat with no message between them.
const REWATCH_MIN: Duration = Duration::from_millis(200);
/// One etcd call (connect, delete, put).
const ETCD_TIMEOUT: Duration = Duration::from_secs(10);
/// HTTP/2 pings on the etcd connection, and how long one may go unanswered.
const ETCD_KEEPALIVE: Duration = Duration::from_secs(10);
/// Tries of an acknowledge before the message is left to come back.
const ACK_TRIES: u32 = 3;

#[derive(Clone)]
pub struct Config {
    /// etcd endpoints, `http://host:port`
    pub etcd: Vec<String>,
    /// etcd user and password, when the server checks them
    pub etcd_user: Option<(String, String)>,
    /// the etcd queue to consume
    pub queue: String,
    /// instead of a queue, a key prefix to watch: each put and delete under it
    /// is sent, and nothing is acknowledged (a watch has nothing to
    /// acknowledge). Best effort: a change while the watch is down is missed
    pub prefix: Option<String>,
    /// this consumer's id in the queue's registry
    pub consumer: Uuid,
    /// the broker: `amqp://`, `amqps://`, `mqtt://`, `mqtts://` or `kafka://`,
    /// then `[user:password@]host:port`
    pub target_url: String,
    /// where on the broker: the AMQP address (the JMS queue), the MQTT topic
    /// prefix, or the Kafka topic
    pub address: String,
    /// how a delivery becomes a message
    pub shape: Shape,
    /// an etcd queue for messages no broker will take
    pub dead_letter: Option<String>,
    /// deliveries held between the watch and the broker
    pub buffer: usize,
    /// how long to wait for the broker: to connect, and to settle one message
    pub send_timeout: Duration,
    /// [bridge-local] the node, when it runs in this process (an embedder's, as
    /// ytserv's): watched and written in-process - no connection, so no TLS to meet
    /// and no listener of its own to dial (`etcd` and `etcd_user` are not used then).
    /// `None`: the `etcd` endpoints, over gRPC
    pub node: Option<crate::cluster::EtcdNode>,
    /// [bridge-secret] the node's prefix policy: a `prefix` holding a key it serves to
    /// no remote caller (`local_only_prefix`, its exceptions aside) is refused - the
    /// bridge sends what it reads off the node out of it, and reads as a local client.
    /// In-process, the node's own is read too, at the start and at every change sent,
    /// and both must allow it. `None` over gRPC: not known, nothing refused - an
    /// embedder bridging its own node over loopback sets it
    pub policy: Option<crate::prefix::PrefixPolicy>,
}

/// A base to set the fields an embedder needs over - `Config { queue, target_url,
/// ..Default::default() }` - so a field added later keeps such code building: no
/// endpoints, queue or target (`validate` refuses it as it is), a random consumer,
/// and the binary's defaults - a buffer of 64, a send timeout of 30 s
impl Default for Config {
    fn default() -> Self {
        Config {
            etcd: vec![],
            etcd_user: None,
            queue: String::new(),
            prefix: None,
            consumer: Uuid::new_v4(),
            target_url: String::new(),
            address: String::new(),
            shape: Shape::default(),
            dead_letter: None,
            buffer: 64,
            send_timeout: Duration::from_secs(30),
            node: None,
            policy: None,
        }
    }
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("etcd", &self.etcd)
            .field("etcd_user", &self.etcd_user.as_ref().map(|(u, _)| u))
            .field("queue", &self.queue)
            .field("prefix", &self.prefix)
            .field("consumer", &self.consumer)
            .field("target_url", &redact(&self.target_url))
            .field("address", &self.address)
            .field("shape", &self.shape)
            .field("dead_letter", &self.dead_letter)
            .field("buffer", &self.buffer)
            .field("send_timeout", &self.send_timeout)
            .field("node", &self.node.as_ref().map(|_| "in-process"))
            .field("policy", &self.policy)
            .finish()
    }
}

impl Config {
    /// What `run` refuses to start with.
    pub fn validate(&self) -> Result<(), String> {
        fn queue_name(what: &str, q: &str) -> Result<(), String> {
            if q.is_empty() || q.contains('/') {
                return Err(format!("{} {:?}: one path segment, not empty", what, q));
            }
            Ok(())
        }
        if self.etcd.is_empty() && self.node.is_none() {
            return Err("no etcd endpoint, and no node in-process".into());
        }
        match &self.prefix {
            None => queue_name("queue", &self.queue)?,
            Some(p) => {
                // nor one over the queues: under it are the server's registry
                // records, which are no one's changes
                let over = |ns: &str| p.starts_with(ns) || ns.starts_with(p.as_str());
                if !p.starts_with('/') || over("/q/") || over("/queue/") {
                    return Err(format!("prefix {:?}: a key prefix from /, not over the queues", p));
                }
            }
        }
        if let Some(d) = &self.dead_letter {
            queue_name("dead-letter queue", d)?;
            if d == &self.queue {
                return Err("the dead-letter queue is the queue itself".into());
            }
        }
        if self.address.is_empty() {
            return Err("no address".into());
        }
        if broker::kind(&self.target_url)? == Kind::Kafka {
            broker::kafka::KafkaUrl::parse(&self.target_url)?;
        } else if let (_, Some(ca)) = broker::tls_ca(&self.target_url)? {
            // [bridge-ca] read at start: a CA that does not, refused before anything is sent
            broker::ca_roots(&ca)?;
        }
        if broker::kind(&self.target_url)? == Kind::Mqtt && self.address.contains(['+', '#']) {
            return Err(format!("MQTT topic prefix {:?}: no wildcards", self.address));
        }
        if self.buffer == 0 {
            return Err("buffer 0: at least 1".into());
        }
        if self.send_timeout < Duration::from_secs(1) {
            return Err(format!("send timeout {:?}: at least 1 s", self.send_timeout));
        }
        Ok(())
    }

    /// [bridge-secret] Refused when `prefix` holds a key `policy` serves to no remote
    /// caller: the bridge reads as a local client and sends to a broker, so it would
    /// carry those keys out of the node - an embedder's secret store under `/secret/`
    /// (its envelopes' metadata, its key wraps) to whoever reads the broker. Asked as
    /// a remote caller would be: what the node gives one, the bridge may send
    ///
    /// A queue (the listener's links too) is asked as a remote consumer's watch of it
    /// is, in both spellings: a queue a remote consumer may not take, the bridge may not
    /// send either.
    pub fn served_out(&self, policy: &crate::prefix::PrefixPolicy) -> Result<(), String> {
        let remote = Some(std::net::SocketAddr::from(([192, 0, 2, 1], 0))); // TEST-NET-1: any non-loopback
        let Some(p) = &self.prefix else {
            let keys = [consumer_key(self), format!("/queue/{}/consumer/{}", self.queue, self.consumer)];
            return match keys.iter().all(|k| policy.range_may_serve(k.as_bytes(), &[], remote)) {
                true => Ok(()),
                false => Err(format!("queue {:?}: the node serves it to no remote consumer (local_only_prefix) - \
                    the bridge would send its messages out of it", self.queue)),
            };
        };
        let end = crate::prefix::prefix_end(p.as_bytes()).unwrap_or_else(|| vec![0]);
        if policy.range_may_serve(p.as_bytes(), &end, remote) {
            Ok(())
        } else {
            Err(format!("prefix {:?}: it holds keys the node serves to no remote caller (local_only_prefix) - \
                the bridge would send them out of it", p))
        }
    }

    /// [bridge-secret] `served_out` against the node's own policy in-process, as it is
    /// now, and against the one given
    pub(crate) async fn kept_in(&self) -> Result<(), String> {
        if let Some(n) = &self.node {
            self.served_out(&*n.policy.read().await)?;
        }
        if let Some(p) = &self.policy {
            self.served_out(p)?;
        }
        Ok(())
    }
}

/// [bridge-local] The node a stream's deliveries came from: the acknowledge and the
/// dead-letter put go back the same way. In-process they are the embedder's own writes
/// (`EtcdNode::kv_delete`, `kv_put`): the same put and delete a gRPC call makes, less
/// its `readonly_prefix` refusal - a prefix only the embedder writes - and its metrics
#[derive(Clone)]
enum Link {
    Grpc(Client),
    Local(crate::cluster::EtcdNode),
}

impl Link {
    async fn delete(&mut self, key: Vec<u8>) -> Result<(), String> {
        match self {
            Link::Grpc(c) => c.delete(key, None).await.map(|_| ()).map_err(|e| e.to_string()),
            Link::Local(n) => n.kv_delete(key).await.map_err(|e| e.to_string()),
        }
    }

    async fn put(&mut self, key: String, value: Vec<u8>) -> Result<(), String> {
        match self {
            Link::Grpc(c) => c.put(key, value, None).await.map(|_| ()).map_err(|e| e.to_string()),
            Link::Local(n) => n.kv_put(key.into_bytes(), value).await.map_err(|e| e.to_string()),
        }
    }
}

/// [bridge-local] One watch response, from either link: canceled (and why), or its
/// events as (a put, the key, the value)
struct Frame {
    canceled: bool,
    reason: String,
    events: Vec<(bool, Vec<u8>, Vec<u8>)>,
}

/// [bridge-local] A watch stream: gRPC's, or one of the node in this process
/// (`EtcdNode::new_watcher`). Dropped, an in-process one closes as a gRPC one does:
/// the node sees its consumer gone and hands out again what it held
enum Watch {
    Grpc(WatchStream),
    Local {
        req: mpsc::Sender<WatchRequest>,
        rx: mpsc::Receiver<Result<crate::etcdpb::etcdserverpb::WatchResponse, tonic::Status>>,
        watch_id: i64,
        /// what came before the watch's `created` frame: a prefix watch is told of a
        /// change from the moment it is registered, which is before that frame is sent
        early: std::collections::VecDeque<Frame>,
    },
}

/// One in-process response as a `Frame`
fn local_frame(r: crate::etcdpb::etcdserverpb::WatchResponse) -> Frame {
    Frame {
        canceled: r.canceled,
        reason: r.cancel_reason,
        events: r.events.into_iter()
            .filter_map(|ev| {
                let put = ev.r#type == crate::etcdpb::mvccpb::event::EventType::Put as i32;
                ev.kv.map(|kv| (put, kv.key, kv.value))
            })
            .collect(),
    }
}

/// A watch of `key`, or of every key under it
fn create(key: &str, prefix: bool, watch_id: i64) -> WatchRequest {
    let range_end = if prefix { crate::prefix::prefix_end(key.as_bytes()).unwrap_or_else(|| vec![0]) } else { vec![] };
    WatchRequest { request_union: Some(watch_request::RequestUnion::CreateRequest(
        WatchCreateRequest { key: key.as_bytes().to_vec(), range_end, watch_id, ..Default::default() })) }
}

impl Watch {
    async fn next(&mut self) -> Result<Option<Frame>, String> {
        match self {
            Watch::Grpc(s) => match s.message().await {
                Ok(Some(r)) => Ok(Some(Frame {
                    canceled: r.canceled(),
                    reason: r.cancel_reason().to_string(),
                    events: r.events().iter()
                        .filter_map(|ev| ev.kv().map(|kv| (ev.event_type() == EventType::Put, kv.key().to_vec(), kv.value().to_vec())))
                        .collect(),
                })),
                Ok(None) => Ok(None),
                Err(e) => Err(e.to_string()),
            },
            Watch::Local { rx, early, .. } => {
                if let Some(f) = early.pop_front() {
                    return Ok(Some(f));
                }
                match rx.recv().await {
                    Some(Ok(r)) => Ok(Some(local_frame(r))),
                    Some(Err(s)) => Err(s.to_string()),
                    None => Ok(None),
                }
            }
        }
    }

    /// Watched again on the same stream, after a cancel
    async fn again(&mut self, key: &str, prefix: bool) -> Result<(), String> {
        match self {
            Watch::Grpc(s) => s.watch(key, prefix.then(|| WatchOptions::new().with_prefix())).await.map_err(|e| e.to_string()),
            Watch::Local { req, watch_id, .. } => {
                *watch_id += 1;
                req.send(create(key, prefix, *watch_id)).await.map_err(|_| "the node's watch is gone".to_string())
            }
        }
    }
}

/// A delivery with the stream it came on: `client` is that stream's
/// connection, used for the acknowledge.
struct Held {
    stream: u64,
    client: Link,
    delivery: Delivery,
}

/// Runs until `shutdown` completes, then closes the broker connection. Whatever
/// was not acknowledged by then goes back to etcd's line when the watch stream
/// closes. An error is a configuration refused, or a task that ended on its own.
pub async fn run(cfg: Config, log: Logger, shutdown: impl Future<Output = ()>) -> Result<(), String> {
    run_on(cfg, log, shutdown, None).await
}

/// `run`, sending to `link` when there is one - a client's link on the
/// listener - which is not connected again: when it ends, the run ends.
pub(crate) async fn run_on(
    cfg: Config,
    log: Logger,
    shutdown: impl Future<Output = ()>,
    link: Option<fe2o3_amqp::Sender>,
) -> Result<(), String> {
    cfg.validate()?;
    cfg.kept_in().await?;
    broker::tls_provider();
    let (tx, rx) = mpsc::channel(cfg.buffer);
    let stream = Arc::new(AtomicU64::new(0));
    let target = Arc::new(Mutex::new(None));
    // [bridge-secret] why the watch stopped, set before its sender goes: the forward
    // task then ends as well (its channel closed), and `select!` may see that first
    let why = Arc::new(std::sync::Mutex::new(None::<String>));
    // [bridge] the run's tasks end with it: a run dropped mid-way (a listener's link whose
    // client did not finish within the stop's wait) aborts them, not leaves them behind
    let mut watch = Aborting(tokio::spawn(watch_loop(cfg.clone(), log.clone(), tx, stream.clone(), why.clone())));
    let mut forward = Aborting(match link {
        Some(sender) => tokio::spawn(link_loop(cfg.clone(), log.clone(), rx, stream, sender)),
        None => tokio::spawn(forward_loop(cfg.clone(), log.clone(), rx, stream, target.clone())),
    });
    // which one ended: its handle is spent - awaited again, it panicked "polled after
    // completion" instead of the run returning its error
    let (result, watch_ended, forward_ended) = tokio::select! {
        _ = shutdown => {
            info!(log, "shutting down");
            (Ok(()), false, false)
        }
        r = &mut watch.0 => (match r {
            Ok(Err(e)) => Err(e),
            r => Err(format!("the watch task ended: {:?}", r)),
        }, true, false),
        r = &mut forward.0 => (Err(format!("the forward task ended: {:?}", r)), false, true),
    };
    watch.0.abort();
    forward.0.abort();
    if !watch_ended {
        let _ = (&mut watch.0).await;
    }
    if !forward_ended {
        let _ = (&mut forward.0).await;
    }
    let result = match (result, why.lock().map(|mut w| w.take()).unwrap_or_default()) {
        (Err(_), Some(why)) => Err(why),
        (r, _) => r,
    };
    // the forward task is gone, and its lock with it
    if let Some(t) = target.lock().await.take() {
        t.close(cfg.send_timeout).await;
    }
    result
}

pub fn consumer_key(cfg: &Config) -> String {
    format!("/q/{}/c/{}", cfg.queue, cfg.consumer)
}

/// Ends only with the reason the bridge must stop (`Err`), or `Ok` once the forward task is gone
async fn watch_loop(cfg: Config, log: Logger, tx: mpsc::Sender<Held>, current: Arc<AtomicU64>,
    why: Arc<std::sync::Mutex<Option<String>>>) -> Result<(), String> {
    let key = cfg.prefix.clone().unwrap_or_else(|| consumer_key(&cfg));
    let prefixed = cfg.prefix.is_some();
    let mut backoff = Backoff::new(BACKOFF_MIN, BACKOFF_MAX);
    loop {
        let (client, mut stream) = match connect_watch(&cfg, &key, prefixed).await {
            Ok(c) => c,
            Err(e) => {
                match &cfg.node {
                    Some(_) => warn!(log, "the node in-process: {}", e),
                    None => warn!(log, "etcd {:?}: {}", cfg.etcd, e),
                }
                backoff.wait().await;
                continue;
            }
        };
        // a new stream: what the forward task holds from the last one is
        // etcd's to hand out again
        let id = current.fetch_add(1, Ordering::SeqCst) + 1;
        match &cfg.prefix {
            None => info!(log, "consuming {}", key; "stream" => id),
            Some(_) => info!(log, "watching {}*", key; "stream" => id),
        }
        let mut rewatch = Backoff::new(REWATCH_MIN, BACKOFF_MAX);

        loop {
            match stream.next().await {
                Ok(Some(resp)) => {
                    // the stream works: a failure after this starts the backoff over
                    backoff.reset();
                    // [bridge-secret] in-process, the node's policy as it is now: a
                    // reconfigure that keeps the prefix's keys local stops the bridge
                    if let (Some(n), false) = (&cfg.node, resp.events.is_empty()) {
                        if let Err(e) = cfg.served_out(&*n.policy.read().await) {
                            error!(log, "stopped: {}", e);
                            if let Ok(mut w) = why.lock() {
                                *w = Some(e.clone());
                            }
                            return Err(e);
                        }
                    }
                    if resp.canceled {
                        warn!(log, "watch canceled: {}, watching again", resp.reason);
                        rewatch.wait().await;
                        if let Err(e) = stream.again(key.as_str(), prefixed).await {
                            warn!(log, "watch again: {}", e);
                            break;
                        }
                        continue;
                    }
                    for (put, k, v) in resp.events {
                        let delivery = match (&cfg.prefix, put) {
                            (None, true) => Delivery::new(&cfg.queue, &k, &v),
                            (None, false) => continue,
                            (Some(p), true) => Delivery::kv(p, &k, &v, KvEvent::Put),
                            (Some(p), false) => Delivery::kv(p, &k, &v, KvEvent::Delete),
                        };
                        rewatch.reset();
                        debug!(log, "received {}", delivery.delivery_key);
                        let held = Held { stream: id, client: client.clone(), delivery };
                        if tx.send(held).await.is_err() {
                            return Ok(()); // the forward task is gone
                        }
                    }
                }
                Ok(None) => {
                    warn!(log, "etcd closed the watch stream");
                    break;
                }
                Err(e) => {
                    warn!(log, "watch: {}", e);
                    break;
                }
            }
        }
        // the stream is gone: what it held is etcd's to hand out again, and
        // sending it now would only send it twice - its acknowledge cannot
        // reach the stream
        current.fetch_add(1, Ordering::SeqCst);
        backoff.wait().await;
    }
}

async fn connect_watch(cfg: &Config, key: &str, prefix: bool) -> Result<(Link, Watch), String> {
    // [bridge-local] the node in this process: its watcher, as a gRPC Watch's
    if let Some(node) = &cfg.node {
        let (req, req_rx) = mpsc::channel(4);
        let (tx, mut rx) = mpsc::channel(cfg.buffer.max(16));
        node.new_watcher(req_rx, tx, &Uuid::new_v4()).await;
        req.send(create(key, prefix, 1)).await.map_err(|_| "the node took no watch".to_string())?;
        // its `created` frame - a change under a prefix may come before it, kept
        let deadline = tokio::time::Instant::now() + ETCD_TIMEOUT;
        let mut early = std::collections::VecDeque::new();
        loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(Ok(r))) if r.canceled => return Err(format!("the node refused the watch: {}", r.cancel_reason)),
                Ok(Some(Ok(r))) if r.created => {
                    return Ok((Link::Local(node.clone()), Watch::Local { req, rx, watch_id: 1, early }));
                }
                Ok(Some(Ok(r))) => early.push_back(local_frame(r)),
                Ok(Some(Err(s))) => return Err(s.to_string()),
                Ok(None) => return Err("the node closed the watch".into()),
                Err(_) => return Err(format!("no answer in {:?}", ETCD_TIMEOUT)),
            }
        }
    }
    let watch = prefix.then(|| WatchOptions::new().with_prefix());
    let mut opts = ConnectOptions::new()
        .with_connect_timeout(ETCD_TIMEOUT)
        .with_keep_alive(ETCD_KEEPALIVE, ETCD_KEEPALIVE)
        .with_keep_alive_while_idle(true)
        .with_tcp_keepalive(ETCD_KEEPALIVE);
    if let Some((u, p)) = &cfg.etcd_user {
        opts = opts.with_user(u.clone(), p.clone());
    }
    let connect = async {
        let mut client = Client::connect(&cfg.etcd, Some(opts)).await?;
        let stream = client.watch(key, watch).await?;
        Ok::<_, etcd_client::Error>((client, stream))
    };
    match tokio::time::timeout(ETCD_TIMEOUT, connect).await {
        Ok(r) => r.map(|(c, s)| (Link::Grpc(c), Watch::Grpc(s))).map_err(|e| e.to_string()),
        Err(_) => Err(format!("no answer in {:?}", ETCD_TIMEOUT)),
    }
}

async fn forward_loop(
    cfg: Config,
    log: Logger,
    mut rx: mpsc::Receiver<Held>,
    current: Arc<AtomicU64>,
    target: Arc<Mutex<Option<Target>>>,
) {
    let mut target = target.lock().await;
    let mut backoff = Backoff::new(BACKOFF_MIN, BACKOFF_MAX);
    while let Some(mut held) = rx.recv().await {
        loop {
            // a key's change is the watch's alone: no stream hands it out again
            if held.delivery.event.is_none() && held.stream != current.load(Ordering::SeqCst) {
                debug!(log, "{} is from a closed stream, etcd hands it out again", held.delivery.delivery_key);
                break;
            }
            let dk = held.delivery.delivery_key.clone();
            let t = match target.as_mut() {
                Some(t) => t,
                None => match Target::connect(&cfg).await {
                    Ok(t) => {
                        info!(log, "sending to {} at {}", cfg.address, redact(&cfg.target_url));
                        target.insert(t)
                    }
                    Err(e) => {
                        warn!(log, "{}: {}", redact(&cfg.target_url), e);
                        backoff.wait().await;
                        continue;
                    }
                },
            };
            match tokio::time::timeout(cfg.send_timeout, t.send(&held.delivery, &cfg, now_ms())).await {
                Ok(Ok(Sent::Accepted)) => {
                    acknowledge(&log, &mut held).await;
                    backoff.reset();
                    break;
                }
                Ok(Ok(Sent::Permanent(why))) => {
                    if give_up(&cfg, &log, &mut held, &why).await {
                        backoff.reset();
                        break;
                    }
                }
                Ok(Ok(Sent::Retry(why))) => warn!(log, "{}: {}, retrying", dk, why),
                Ok(Err(e)) => {
                    warn!(log, "send {}: {}", dk, e);
                    if let Some(t) = target.take() {
                        t.close(cfg.send_timeout).await;
                    }
                }
                Err(_) => {
                    // the broker may have it: sent again on a new connection,
                    // at least once, with the same message id
                    warn!(log, "send {}: no answer in {:?}", dk, cfg.send_timeout);
                    if let Some(t) = target.take() {
                        t.close(cfg.send_timeout).await;
                    }
                }
            }
            backoff.wait().await;
        }
    }
}

/// A JMS client's link on the listener: up to `buffer` messages out at once,
/// each acknowledged in etcd when the client settles it - so a client that
/// acknowledges several at a time, or takes its time, is not held to one, nor
/// to the send timeout. A message the client releases is sent again; one it
/// rejects for good goes to the dead-letter queue, or stays held until the link
/// ends. The link's end - the client leaves, or it fails - ends the run, and
/// what is out unsettled goes back to etcd's line with the stream.
async fn link_loop(cfg: Config, log: Logger, mut rx: mpsc::Receiver<Held>, current: Arc<AtomicU64>, mut sender: fe2o3_amqp::Sender) {
    use futures_util::stream::{FuturesUnordered, StreamExt};
    use std::collections::VecDeque;

    async fn next_held(again: &mut VecDeque<Held>, rx: &mut mpsc::Receiver<Held>) -> Option<Held> {
        match again.pop_front() {
            Some(h) => Some(h),
            None => rx.recv().await,
        }
    }

    let mut inflight = FuturesUnordered::new();
    let mut again: VecDeque<Held> = VecDeque::new();
    loop {
        tokio::select! {
            e = sender.on_detach() => {
                info!(log, "the client left: {}", e);
                return;
            }
            Some((mut held, r)) = inflight.next(), if !inflight.is_empty() => {
                match r {
                    Ok(outcome) => match broker::amqp::sent(outcome) {
                        Sent::Accepted => acknowledge(&log, &mut held).await,
                        Sent::Permanent(why) => {
                            // without a dead-letter queue, held until the link ends
                            let _ = give_up(&cfg, &log, &mut held, &why).await;
                        }
                        Sent::Retry(why) => {
                            warn!(log, "{}: {}, sending again", held.delivery.delivery_key, why);
                            again.push_back(held);
                        }
                    },
                    Err(e) => {
                        info!(log, "the link failed: {}", e);
                        return;
                    }
                }
            }
            h = next_held(&mut again, &mut rx), if inflight.len() < cfg.buffer => {
                let Some(mut held) = h else { return };
                if held.delivery.event.is_none() && held.stream != current.load(Ordering::SeqCst) {
                    continue; // etcd hands it out again
                }
                let send = broker::amqp::send_unsettled(&mut sender, &held.delivery, &cfg.shape, now_ms());
                match tokio::time::timeout(cfg.send_timeout, send).await {
                    Ok(Ok(Ok(pending))) => inflight.push(async move {
                        let r = pending.await;
                        (held, r)
                    }),
                    Ok(Ok(Err(why))) => {
                        let _ = give_up(&cfg, &log, &mut held, &why).await;
                    }
                    Ok(Err(e)) => {
                        info!(log, "the link failed: {}", e);
                        return;
                    }
                    // no credit from the client yet: first in line again
                    Err(_) => again.push_front(held),
                }
            }
        }
    }
}

/// For a message no broker will take: onto the dead-letter queue, then
/// acknowledged. False while it is still held - no dead-letter queue, or the
/// put failed.
async fn give_up(cfg: &Config, log: &Logger, held: &mut Held, why: &str) -> bool {
    let dk = held.delivery.delivery_key.clone();
    let Some(dlq) = &cfg.dead_letter else {
        error!(log, "{}: {}, not acknowledged: retrying (no dead-letter queue)", dk, why);
        return false;
    };
    // one segment: a watched key's `/` would cut it short
    let key = format!("/q/{}/p/{}", dlq, held.delivery.key.replace('%', "%25").replace('/', "%2F"));
    let put = held.client.put(key, held.delivery.value.clone());
    match tokio::time::timeout(ETCD_TIMEOUT, put).await {
        Ok(Ok(())) => {
            warn!(log, "{}: {}, put on dead-letter queue {}", dk, why, dlq);
            acknowledge(log, held).await;
            true
        }
        Ok(Err(e)) => {
            error!(log, "{}: {}, dead-letter queue {}: {}", dk, why, dlq, e);
            false
        }
        Err(_) => {
            error!(log, "{}: {}, dead-letter queue {}: no answer in {:?}", dk, why, dlq, ETCD_TIMEOUT);
            false
        }
    }
}

/// The delete of the delivery key, on the stream's own connection. A delete
/// that fails every try leaves the message held by the stream: it comes back,
/// and is sent again, when the stream closes.
async fn acknowledge(log: &Logger, held: &mut Held) {
    let dk = held.delivery.delivery_key.clone();
    if held.delivery.event.is_some() {
        // a watched key: deleting it would remove the key itself
        debug!(log, "sent {}", dk);
        return;
    }
    for attempt in 1..=ACK_TRIES {
        match tokio::time::timeout(ETCD_TIMEOUT, held.client.delete(held.delivery.raw_key.clone())).await {
            Ok(Ok(())) => {
                debug!(log, "acknowledged {}", dk);
                return;
            }
            Ok(Err(e)) => warn!(log, "acknowledge {} ({}/{}): {}", dk, attempt, ACK_TRIES, e),
            Err(_) => warn!(log, "acknowledge {} ({}/{}): no answer in {:?}", dk, attempt, ACK_TRIES, ETCD_TIMEOUT),
        }
        if attempt < ACK_TRIES {
            tokio::time::sleep(BACKOFF_MIN).await;
        }
    }
    error!(log, "{} not acknowledged: it is sent again when the stream closes", dk);
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or_default()
}

/// [bridge] A task that ends with its owner: dropped, it is aborted, not left running
struct Aborting<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for Aborting<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The URL without its password, for the log.
pub fn redact(url: &str) -> String {
    match (url.find("://"), url.rfind('@')) {
        (Some(s), Some(at)) if at > s => match url[s + 3..at].find(':') {
            Some(c) => format!("{}:***{}", &url[..s + 3 + c], &url[at..]),
            None => url.to_string(),
        },
        _ => url.to_string(),
    }
}

struct Backoff {
    min: Duration,
    max: Duration,
    next: Option<Duration>,
}

impl Backoff {
    fn new(min: Duration, max: Duration) -> Self {
        Backoff { min, max, next: None }
    }

    async fn wait(&mut self) {
        let d = self.next.map_or(self.min, |d| (d * 2).min(self.max));
        self.next = Some(d);
        tokio::time::sleep(d).await;
    }

    fn reset(&mut self) {
        self.next = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config {
            etcd: vec!["http://localhost:2379".into()],
            etcd_user: None,
            queue: "jobs".into(),
            prefix: None,
            consumer: Uuid::new_v4(),
            target_url: "amqp://app:s3cret@mq:5672".into(),
            address: "jobs".into(),
            shape: Shape::default(),
            dead_letter: None,
            buffer: 64,
            send_timeout: Duration::from_secs(30),
            node: None,
            policy: None,
        }
    }

    /// [bridge-local] What came before the watch's `created` frame is read first, in order
    #[tokio::test]
    async fn an_in_process_watch_reads_what_came_before_created_first() {
        let (req, _req_rx) = mpsc::channel(1);
        let (tx, rx) = mpsc::channel(4);
        let frame = |k: &str| Frame { canceled: false, reason: String::new(), events: vec![(true, k.as_bytes().to_vec(), vec![])] };
        let mut w = Watch::Local { req, rx, watch_id: 1, early: [frame("/p/a"), frame("/p/b")].into_iter().collect() };
        tx.send(Ok(crate::etcdpb::etcdserverpb::WatchResponse { events: vec![crate::etcdpb::mvccpb::Event {
            r#type: 1, kv: Some(crate::etcdpb::mvccpb::KeyValue { key: b"/p/c".to_vec(), ..Default::default() }), prev_kv: None }],
            ..Default::default() })).await.unwrap();
        let keys: Vec<(bool, Vec<u8>)> = vec![w.next().await, w.next().await, w.next().await].into_iter()
            .map(|f| { let f = f.unwrap().unwrap(); (f.events[0].0, f.events[0].1.clone()) }).collect();
        assert_eq!(keys, vec![(true, b"/p/a".to_vec()), (true, b"/p/b".to_vec()), (false, b"/p/c".to_vec())], "a delete is type 1");
    }

    /// [bridge-secret] In-process the node's policy is read at every change sent: a
    /// reconfigure that keeps the prefix's keys local stops the bridge
    #[tokio::test(flavor = "multi_thread")]
    async fn an_in_process_bridge_stops_once_its_prefix_is_kept_local() {
        let n = crate::queue::node_tests::node("33333333-3333-5333-8333-333333333333").await;
        let mut c = cfg();
        c.etcd = vec![];
        c.node = Some(n.clone());
        c.prefix = Some("/cfg/".into());
        c.target_url = "amqp://127.0.0.1:1".into(); // no broker: what is sent waits
        c.address = "cfg".into();
        let log = Logger::root(slog::Discard, slog::o!());
        let run = tokio::spawn(run(c, log, std::future::pending()));
        tokio::time::sleep(Duration::from_millis(300)).await;
        *n.policy.write().await = crate::prefix::PrefixPolicy::new("", "", "/cfg/", "");
        n.kv_put(b"/cfg/x".to_vec(), b"1".to_vec()).await.unwrap();
        let ended = tokio::time::timeout(Duration::from_secs(10), run).await.expect("stopped").unwrap();
        assert!(ended.is_err(), "ended, not shut down: {:?}", ended);
        // the caller is told why, not "the watch task ended: Ok(())"
        let e = ended.unwrap_err();
        assert!(e.contains("serves to no remote caller"), "the reason returned: {}", e);
    }

    /// [bridge-secret] The stop's reason is what `run` returns, every time: the forward
    /// task ends with the watch (its channel closed), and `select!` took either one
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_policy_stop_reason_is_returned_every_time() {
        for round in 0..30 {
            let n = crate::queue::node_tests::node("44444444-4444-5444-8444-444444444444").await;
            let mut c = cfg();
            c.etcd = vec![];
            c.node = Some(n.clone());
            c.prefix = Some("/cfg/".into());
            c.target_url = "amqp://127.0.0.1:1".into();
            c.address = "cfg".into();
            let run = tokio::spawn(run(c, Logger::root(slog::Discard, slog::o!()), std::future::pending()));
            tokio::time::sleep(Duration::from_millis(100)).await;
            *n.policy.write().await = crate::prefix::PrefixPolicy::new("", "", "/cfg/", "");
            n.kv_put(b"/cfg/x".to_vec(), b"1".to_vec()).await.unwrap();
            let e = tokio::time::timeout(Duration::from_secs(10), run).await.expect("stopped").unwrap().unwrap_err();
            assert!(e.contains("serves to no remote caller"), "round {}: {}", round, e);
        }
    }

    /// [bridge-secret] `run` refuses a queue its policy gives no remote consumer - in
    /// either spelling - before it watches anything; another queue it runs
    #[tokio::test]
    async fn run_refuses_a_queue_kept_local() {
        for local in ["/q/payroll/", "/queue/payroll/"] {
            let mut c = cfg();
            c.queue = "payroll".into();
            c.policy = Some(crate::prefix::PrefixPolicy::new("", "", local, ""));
            let log = Logger::root(slog::Discard, slog::o!());
            let r = tokio::time::timeout(Duration::from_secs(5), run(c, log, std::future::pending())).await
                .expect("refused at once");
            assert!(r.unwrap_err().contains("no remote consumer"), "{}", local);
        }
        let mut c = cfg();
        c.policy = Some(crate::prefix::PrefixPolicy::new("", "", "/q/payroll/", ""));
        assert!(c.served_out(c.policy.as_ref().unwrap()).is_ok(), "the queue jobs");
    }

    /// [bridge] a run's task ends with the run: dropped mid-way, it is aborted
    #[tokio::test]
    async fn a_task_ends_with_its_run() {
        let done = Arc::new(AtomicU64::new(0));
        let d = done.clone();
        let t = Aborting(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            d.store(1, Ordering::SeqCst);
        }));
        drop(t);
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(done.load(Ordering::SeqCst), 0, "the task went on after its run was dropped");
    }

    #[test]
    fn redacts_the_password() {
        assert_eq!(redact("amqp://app:s3cret@mq:5672"), "amqp://app:***@mq:5672");
        assert_eq!(redact("amqp://app@mq:5672"), "amqp://app@mq:5672");
        assert_eq!(redact("amqp://mq:5672"), "amqp://mq:5672");
        let mut c = cfg();
        c.etcd_user = Some(("u".into(), "etcd-secret".into()));
        let shown = format!("{:?}", c);
        assert!(!shown.contains("s3cret") && !shown.contains("etcd-secret"), "{}", shown);
    }

    #[test]
    fn validates() {
        assert!(cfg().validate().is_ok());
        let bad: Vec<fn(&mut Config)> = vec![
            |c| c.etcd.clear(),
            |c| c.prefix = Some("config/".into()),
            |c| c.prefix = Some("/q/jobs/".into()),
            |c| c.prefix = Some("/".into()),
            |c| c.prefix = Some("/qu".into()),
            |c| c.queue = "".into(),
            |c| c.queue = "a/b".into(),
            |c| c.dead_letter = Some("x/y".into()),
            |c| c.dead_letter = Some("jobs".into()),
            |c| c.address = "".into(),
            |c| c.target_url = "http://mq".into(),
            |c| {
                c.target_url = "mqtt://mq".into();
                c.address = "etcd/+".into();
            },
            |c| c.buffer = 0,
            |c| c.send_timeout = Duration::ZERO,
        ];
        for (i, f) in bad.into_iter().enumerate() {
            let mut c = cfg();
            f(&mut c);
            assert!(c.validate().is_err(), "case {} accepted: {:?}", i, c);
        }
    }
}
