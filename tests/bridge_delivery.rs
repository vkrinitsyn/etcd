//! End to end, in-process: an etcds node, a minimal AMQP 1.0 broker (fe2o3's
//! acceptor) that settles as each test tells it, and the bridge between them.
//! A message is acknowledged when it is gone from the queue: a consumer
//! watching after the bridge stopped gets nothing.
#![cfg(feature = "bridge")]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use etcd_client::{Client, EventType};
use etcds::bridge::{self, Config};
use etcds::bridge::mapping::Shape;
use etcds::cluster::EtcdNode;
use fe2o3_amqp::acceptor::{ConnectionAcceptor, LinkAcceptor, LinkEndpoint, SessionAcceptor};
use fe2o3_amqp::types::definitions::{AmqpError, Error as AmqpErr};
use fe2o3_amqp::types::messaging::{Body, Message};
use fe2o3_amqp::types::primitives::Value;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

type Got = Message<Body<Value>>;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

async fn etcd() -> (EtcdNode, String) {
    let addr = format!("127.0.0.1:{}", free_port());
    let mut cfg = etcds::cli::EtcdConfig::with_defaults();
    cfg.listen_client_urls = addr.clone();
    let node = EtcdNode::init(cfg, slog::Logger::root(slog::Discard, slog::o!()),
        #[cfg(feature = "tracer")] None).await.unwrap();
    node.serve().await.unwrap();
    let endpoint = format!("http://{}", addr);
    let mut c = Client::connect([endpoint.as_str()], None).await.unwrap();
    for _ in 0..50 {
        if c.status().await.is_ok() {
            return (node, endpoint);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("etcds on {} did not start", addr);
}

/// How the broker settles the n-th delivery it gets (from 0).
#[derive(Clone, Debug)]
enum Settle {
    Accept,
    Reject(AmqpError),
    Release,
    /// never: the sender waits for an outcome that does not come
    Ignore,
}

/// A broker on `listener`: every message it gets goes to the channel, and is
/// settled as `policy` says for its text and its number.
fn broker_on(
    listener: TcpListener,
    policy: impl Fn(usize, &str) -> Settle + Send + Sync + 'static,
) -> mpsc::UnboundedReceiver<Got> {
    let (tx, rx) = mpsc::unbounded_channel();
    let policy = Arc::new(policy);
    let n = Arc::new(AtomicUsize::new(0));
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let (tx, policy, n) = (tx.clone(), policy.clone(), n.clone());
            tokio::spawn(async move {
                let Ok(mut conn) = ConnectionAcceptor::new("test-broker").accept(stream).await else { return };
                let Ok(mut session) = SessionAcceptor::new().accept(&mut conn).await else { return };
                let Ok(LinkEndpoint::Receiver(mut recv)) = LinkAcceptor::new().accept(&mut session).await else { return };
                while let Ok(d) = recv.recv::<Body<Value>>().await {
                    let t = match d.body() {
                        Body::Value(v) => match &v.0 {
                            Value::String(s) => s.clone(),
                            _ => String::new(),
                        },
                        _ => String::new(),
                    };
                    let _ = tx.send(d.message().clone());
                    let _ = match policy(n.fetch_add(1, Ordering::SeqCst), &t) {
                        Settle::Accept => recv.accept(&d).await,
                        Settle::Reject(c) => recv.reject(&d, AmqpErr::new(c, Some("test".to_string()), None)).await,
                        Settle::Release => recv.release(&d).await,
                        Settle::Ignore => Ok(()),
                    };
                }
            });
        }
    });
    rx
}

async fn broker(policy: impl Fn(usize, &str) -> Settle + Send + Sync + 'static) -> (String, mpsc::UnboundedReceiver<Got>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("amqp://{}", listener.local_addr().unwrap());
    (url, broker_on(listener, policy))
}

/// Rejects a message whose text contains "bad" as malformed, accepts the rest.
fn bad_is_malformed(_: usize, t: &str) -> Settle {
    if t.contains("bad") { Settle::Reject(AmqpError::DecodeError) } else { Settle::Accept }
}

fn config(etcd: &str, amqp: &str, queue: &str, dead_letter: Option<&str>) -> Config {
    Config {
        etcd: vec![etcd.to_string()],
        etcd_user: None,
        queue: queue.to_string(),
        prefix: None,
        consumer: Uuid::new_v4(),
        target_url: amqp.to_string(),
        address: format!("jms.{}", queue),
        shape: Shape::default(),
        dead_letter: dead_letter.map(str::to_string),
        buffer: 16,
        send_timeout: Duration::from_secs(5),
    }
}

struct Running(oneshot::Sender<()>, tokio::task::JoinHandle<Result<(), String>>);

/// The bridge's log lines, for a test that checks a path was taken.
#[derive(Clone, Default)]
struct Lines(Arc<std::sync::Mutex<Vec<String>>>);

impl slog::Drain for Lines {
    type Ok = ();
    type Err = slog::Never;
    fn log(&self, r: &slog::Record, _: &slog::OwnedKVList) -> Result<(), slog::Never> {
        self.0.lock().unwrap().push(r.msg().to_string());
        Ok(())
    }
}

impl Lines {
    fn any(&self, part: &str) -> bool {
        self.0.lock().unwrap().iter().any(|l| l.contains(part))
    }
}

fn start(cfg: Config) -> Running {
    start_logged(cfg, slog::Logger::root(slog::Discard, slog::o!()))
}

fn start_logged(cfg: Config, log: slog::Logger) -> Running {
    let (stop, stopped) = oneshot::channel::<()>();
    let h = tokio::spawn(bridge::run(cfg, log, async {
        let _ = stopped.await;
    }));
    Running(stop, h)
}

async fn stop(r: Running) {
    let _ = r.0.send(());
    assert_eq!(r.1.await.unwrap(), Ok(()), "a shutdown is not an error");
}

async fn next(rx: &mut mpsc::UnboundedReceiver<Got>) -> Got {
    tokio::time::timeout(Duration::from_secs(15), rx.recv()).await.expect("broker got nothing").unwrap()
}

fn text(m: &Got) -> String {
    match &m.body {
        Body::Value(v) => match &v.0 {
            Value::String(s) => s.clone(),
            other => panic!("not a string: {:?}", other),
        },
        other => panic!("not an amqp-value: {:?}", other),
    }
}

fn message_id(m: &Got) -> String {
    format!("{:?}", m.properties.as_ref().unwrap().message_id)
}

/// What a fresh consumer of `queue` receives within `wait`: the values.
async fn drain(etcd: &str, queue: &str, wait: Duration) -> Vec<Vec<u8>> {
    let mut c = Client::connect([etcd], None).await.unwrap();
    let mut w = c.watch(format!("/q/{}/c/{}", queue, Uuid::new_v4()), None).await.unwrap();
    let mut got = vec![];
    let deadline = tokio::time::Instant::now() + wait;
    while let Ok(Ok(Some(resp))) = tokio::time::timeout_at(deadline, w.message()).await {
        for ev in resp.events() {
            if ev.event_type() == EventType::Put
                && let Some(kv) = ev.kv()
            {
                got.push(kv.value().to_vec());
                c.delete(kv.key(), None).await.unwrap();
            }
        }
    }
    got
}

async fn producer(etcd: &str) -> Client {
    let c = Client::connect([etcd], None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await; // the bridge's consumer registers
    c
}

#[tokio::test(flavor = "multi_thread")]
async fn accepted_messages_are_acknowledged_in_order() {
    let (_node, etcd) = etcd().await;
    let (amqp, mut rx) = broker(|_, _| Settle::Accept).await;
    let b = start(config(&etcd, &amqp, "jobs", None));

    let mut c = producer(&etcd).await;
    for i in 0..5 {
        c.put(format!("/q/jobs/p/k{}", i), format!("v{}", i), None).await.unwrap();
    }
    c.put("/q/jobs/p/bin", vec![0xffu8, 0x00, 0xfe], None).await.unwrap();

    for i in 0..5 {
        let m = next(&mut rx).await;
        assert_eq!(text(&m), format!("v{}", i));
        assert!(m.header.as_ref().unwrap().durable);
    }
    let m = next(&mut rx).await;
    match &m.body {
        Body::Data(d) => assert_eq!(d.iter().next().unwrap().0.as_ref(), &[0xff, 0x00, 0xfe]),
        other => panic!("not data: {:?}", other),
    }

    tokio::time::sleep(Duration::from_millis(300)).await; // the last delete lands
    stop(b).await;
    assert!(drain(&etcd, "jobs", Duration::from_secs(2)).await.is_empty(), "left unacknowledged");
}

#[tokio::test(flavor = "multi_thread")]
async fn rejected_without_dead_letter_is_kept() {
    let (_node, etcd) = etcd().await;
    let (amqp, mut rx) = broker(bad_is_malformed).await;
    let b = start(config(&etcd, &amqp, "keep", None));

    let mut c = producer(&etcd).await;
    c.put("/q/keep/p/x", "bad one", None).await.unwrap();

    assert_eq!(text(&next(&mut rx).await), "bad one");
    assert_eq!(text(&next(&mut rx).await), "bad one", "retried");
    stop(b).await;
    // back on the line once the bridge's stream closed
    assert_eq!(drain(&etcd, "keep", Duration::from_secs(3)).await, vec![b"bad one".to_vec()]);
}

#[tokio::test(flavor = "multi_thread")]
async fn rejected_goes_to_dead_letter_then_acknowledged() {
    let (_node, etcd) = etcd().await;
    let (amqp, mut rx) = broker(bad_is_malformed).await;
    let b = start(config(&etcd, &amqp, "orders", Some("orders-dlq")));

    let mut c = producer(&etcd).await;
    c.put("/q/orders/p/a", "bad order", None).await.unwrap();
    c.put("/q/orders/p/b", "good order", None).await.unwrap();

    assert_eq!(text(&next(&mut rx).await), "bad order");
    assert_eq!(text(&next(&mut rx).await), "good order");
    tokio::time::sleep(Duration::from_millis(300)).await;
    stop(b).await;

    assert!(drain(&etcd, "orders", Duration::from_secs(2)).await.is_empty(), "left unacknowledged");
    assert_eq!(drain(&etcd, "orders-dlq", Duration::from_secs(2)).await, vec![b"bad order".to_vec()]);
}

/// A rejection for now (a full address) is not dead-lettered: it is retried
/// until the broker takes it.
#[tokio::test(flavor = "multi_thread")]
async fn rejected_for_now_and_released_are_retried() {
    let (_node, etcd) = etcd().await;
    let (amqp, mut rx) = broker(|n, _| match n {
        0 => Settle::Reject(AmqpError::ResourceLimitExceeded),
        1 => Settle::Release,
        _ => Settle::Accept,
    })
    .await;
    let b = start(config(&etcd, &amqp, "full", Some("full-dlq")));

    let mut c = producer(&etcd).await;
    c.put("/q/full/p/x", "later", None).await.unwrap();
    let first = next(&mut rx).await;
    let second = next(&mut rx).await;
    let third = next(&mut rx).await;
    assert_eq!((text(&first), text(&third)), ("later".into(), "later".into()));
    assert_eq!(message_id(&first), message_id(&second), "one delivery, one message id");
    assert_eq!(message_id(&first), message_id(&third));

    tokio::time::sleep(Duration::from_millis(300)).await;
    stop(b).await;
    assert!(drain(&etcd, "full", Duration::from_secs(2)).await.is_empty(), "left unacknowledged");
    assert!(drain(&etcd, "full-dlq", Duration::from_secs(1)).await.is_empty(), "not dead-lettered");
}

/// A broker that never settles: the send times out, the bridge connects again
/// and sends the same message, with the same id.
#[tokio::test(flavor = "multi_thread")]
async fn no_outcome_is_sent_again_with_the_same_id() {
    let (_node, etcd) = etcd().await;
    let (amqp, mut rx) = broker(|n, _| if n == 0 { Settle::Ignore } else { Settle::Accept }).await;
    let mut cfg = config(&etcd, &amqp, "slow", None);
    cfg.send_timeout = Duration::from_secs(1);
    let b = start(cfg);

    let mut c = producer(&etcd).await;
    c.put("/q/slow/p/x", "once more", None).await.unwrap();
    let first = next(&mut rx).await;
    let again = next(&mut rx).await;
    assert_eq!(text(&again), "once more");
    assert_eq!(message_id(&first), message_id(&again), "a broker can drop the resend as a duplicate");

    tokio::time::sleep(Duration::from_millis(300)).await;
    stop(b).await;
    assert!(drain(&etcd, "slow", Duration::from_secs(2)).await.is_empty(), "left unacknowledged");
}

/// The broker down past etcd's 5 s slow-consumer limit with a buffer of one
/// and more messages than the transport holds:
/// etcd drops the bridge's watch (ETCD-E052), the bridge watches again, and
/// every message arrives once, in order, when the broker is back.
#[tokio::test(flavor = "multi_thread")]
async fn broker_down_holds_then_delivers_in_order() {
    let (_node, etcd) = etcd().await;
    let addr = format!("127.0.0.1:{}", free_port()); // nothing listens there yet
    let mut cfg = config(&etcd, &format!("amqp://{}", addr), "late", None);
    cfg.buffer = 1;
    cfg.send_timeout = Duration::from_secs(2);
    let lines = Lines::default();
    let b = start_logged(cfg, slog::Logger::root(lines.clone(), slog::o!()));

    // more than the server's watch channel and the client's HTTP/2 stream
    // window (hyper: 2 MiB) hold, so etcd's send to the bridge stalls
    const N: usize = 200;
    let pad = "x".repeat(16 * 1024);
    let mut c = producer(&etcd).await;
    for i in 0..N {
        c.put(format!("/q/late/p/k{}", i), format!("w{:03}{}", i, pad), None).await.unwrap();
    }
    tokio::time::sleep(Duration::from_secs(8)).await;

    let mut rx = broker_on(TcpListener::bind(&addr).await.unwrap(), |_, _| Settle::Accept);
    for i in 0..N {
        assert_eq!(text(&next(&mut rx).await)[..4], format!("w{:03}", i));
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(rx.try_recv().is_err(), "each once");
    // the cancel frame queues behind what the transport held: read once the
    // broker is back
    assert!(lines.any("ETCD-E052"), "etcd dropped the bridge as a slow consumer");
    assert!(lines.any("watching again"), "and the bridge watched again");
    stop(b).await;
    assert!(drain(&etcd, "late", Duration::from_secs(2)).await.is_empty(), "left unacknowledged");
}

#[tokio::test]
async fn a_bad_config_is_refused() {
    let mut cfg = config("http://127.0.0.1:1", "amqp://127.0.0.1:1", "a/b", None);
    let log = slog::Logger::root(slog::Discard, slog::o!());
    assert!(bridge::run(cfg.clone(), log.clone(), std::future::pending()).await.is_err());
    cfg.queue = "ok".into();
    cfg.send_timeout = Duration::ZERO;
    assert!(bridge::run(cfg, log, std::future::pending()).await.is_err());
}

/// A watched key prefix: puts and deletes go out in order, each marked, and
/// the keys stay - a send is not a delete, as a queue's acknowledge is.
#[tokio::test(flavor = "multi_thread")]
async fn a_key_prefix_sends_puts_and_deletes_and_leaves_the_keys() {
    let (_node, etcd) = etcd().await;
    let (amqp, mut rx) = broker(|_, _| Settle::Accept).await;
    let mut cfg = config(&etcd, &amqp, "unused", None);
    cfg.prefix = Some("/config/".into());
    let b = start(cfg);

    let mut c = producer(&etcd).await;
    c.put("/config/db", "pg://a", None).await.unwrap();
    c.put("/config/cache", "redis://b", None).await.unwrap();
    c.put("/other/x", "not watched", None).await.unwrap();
    c.delete("/config/db", None).await.unwrap();

    let event = |m: &Got| {
        let app = &m.application_properties.as_ref().unwrap().0;
        let s = |k: &str| match app.get(k) {
            Some(fe2o3_amqp::types::primitives::SimpleValue::String(s)) => s.clone(),
            other => panic!("{}: {:?}", k, other),
        };
        (s("etcd_event"), s("etcd_key"), s("etcd_prefix"))
    };
    let m = next(&mut rx).await;
    assert_eq!((text(&m), event(&m)), ("pg://a".into(), ("put".into(), "db".into(), "/config/".into())));
    let m = next(&mut rx).await;
    assert_eq!((text(&m), event(&m).0, event(&m).1), ("redis://b".into(), "put".into(), "cache".into()));
    let m = next(&mut rx).await;
    assert_eq!((event(&m).0, event(&m).1), ("delete".into(), "db".into()));
    match &m.body {
        Body::Data(d) => assert!(d.iter().all(|x| x.0.is_empty()), "a delete has no value"),
        other => panic!("not an empty data body: {:?}", other),
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(rx.try_recv().is_err(), "nothing from outside the prefix");

    let got = c.get("/config/cache", None).await.unwrap();
    assert_eq!(got.kvs().first().map(|kv| kv.value().to_vec()), Some(b"redis://b".to_vec()), "the key stays");
    stop(b).await;
}
