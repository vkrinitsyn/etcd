//! [bridge] Feature `bridge` (bridge.md): this node's queue or key prefix sent to
//! a broker, or JMS clients served as an AMQP peer. [`node`] reads the
//! `bridge_*` settings for the binary; [`broker`] holds the brokers, [`listener`]
//! the AMQP peer, [`mapping`] the JMS message and [`cloudevents`] its attributes.
//! The bridge is a client of the node like any other, over gRPC.
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
        if self.etcd.is_empty() {
            return Err("no etcd endpoint".into());
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
}

/// A delivery with the stream it came on: `client` is that stream's
/// connection, used for the acknowledge.
struct Held {
    stream: u64,
    client: Client,
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
    broker::tls_provider();
    let (tx, rx) = mpsc::channel(cfg.buffer);
    let stream = Arc::new(AtomicU64::new(0));
    let target = Arc::new(Mutex::new(None));
    let mut watch = tokio::spawn(watch_loop(cfg.clone(), log.clone(), tx, stream.clone()));
    let mut forward = match link {
        Some(sender) => tokio::spawn(link_loop(cfg.clone(), log.clone(), rx, stream, sender)),
        None => tokio::spawn(forward_loop(cfg.clone(), log.clone(), rx, stream, target.clone())),
    };
    let result = tokio::select! {
        _ = shutdown => {
            info!(log, "shutting down");
            Ok(())
        }
        r = &mut watch => Err(format!("the watch task ended: {:?}", r)),
        r = &mut forward => Err(format!("the forward task ended: {:?}", r)),
    };
    watch.abort();
    forward.abort();
    let _ = watch.await;
    let _ = forward.await;
    // the forward task is gone, and its lock with it
    if let Some(t) = target.lock().await.take() {
        t.close(cfg.send_timeout).await;
    }
    result
}

pub fn consumer_key(cfg: &Config) -> String {
    format!("/q/{}/c/{}", cfg.queue, cfg.consumer)
}

async fn watch_loop(cfg: Config, log: Logger, tx: mpsc::Sender<Held>, current: Arc<AtomicU64>) {
    let key = cfg.prefix.clone().unwrap_or_else(|| consumer_key(&cfg));
    let opts = cfg.prefix.as_ref().map(|_| WatchOptions::new().with_prefix());
    let mut backoff = Backoff::new(BACKOFF_MIN, BACKOFF_MAX);
    loop {
        let (client, mut stream) = match connect_watch(&cfg, &key, opts.clone()).await {
            Ok(c) => c,
            Err(e) => {
                warn!(log, "etcd {:?}: {}", cfg.etcd, e);
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
            match stream.message().await {
                Ok(Some(resp)) => {
                    // the stream works: a failure after this starts the backoff over
                    backoff.reset();
                    if resp.canceled() {
                        warn!(log, "watch canceled: {}, watching again", resp.cancel_reason());
                        rewatch.wait().await;
                        if let Err(e) = stream.watch(key.as_str(), opts.clone()).await {
                            warn!(log, "watch again: {}", e);
                            break;
                        }
                        continue;
                    }
                    for ev in resp.events() {
                        let Some(kv) = ev.kv() else { continue };
                        let delivery = match (&cfg.prefix, ev.event_type()) {
                            (None, EventType::Put) => Delivery::new(&cfg.queue, kv.key(), kv.value()),
                            (None, EventType::Delete) => continue,
                            (Some(p), EventType::Put) => Delivery::kv(p, kv.key(), kv.value(), KvEvent::Put),
                            (Some(p), EventType::Delete) => Delivery::kv(p, kv.key(), kv.value(), KvEvent::Delete),
                        };
                        rewatch.reset();
                        debug!(log, "received {}", delivery.delivery_key);
                        let held = Held { stream: id, client: client.clone(), delivery };
                        if tx.send(held).await.is_err() {
                            return; // the forward task is gone
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

async fn connect_watch(cfg: &Config, key: &str, watch: Option<WatchOptions>) -> Result<(Client, WatchStream), String> {
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
        Ok(r) => r.map_err(|e| e.to_string()),
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
    let put = held.client.put(key, held.delivery.value.clone(), None);
    match tokio::time::timeout(ETCD_TIMEOUT, put).await {
        Ok(Ok(_)) => {
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
        match tokio::time::timeout(ETCD_TIMEOUT, held.client.delete(held.delivery.raw_key.clone(), None)).await {
            Ok(Ok(_)) => {
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
        }
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
