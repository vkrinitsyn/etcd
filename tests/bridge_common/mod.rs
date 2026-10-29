//! What the end-to-end suites share: an etcds node in-process, a minimal AMQP
//! 1.0 broker that settles as a test says, a TCP proxy that can drop and refuse
//! connections - a peer that goes away - and the bridge's log lines.
#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use etcd_client::{Client, EventType};
use etcds::bridge::{self, Config};
use etcds::bridge::mapping::Shape;
use etcds::cluster::EtcdNode;
use fe2o3_amqp::acceptor::{ConnectionAcceptor, LinkAcceptor, LinkEndpoint, SessionAcceptor};
use fe2o3_amqp::types::definitions::{AmqpError, Error as AmqpErr};
use fe2o3_amqp::types::messaging::{Body, Message, Modified};
use fe2o3_amqp::types::primitives::Value;
use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use uuid::Uuid;

pub type Got = Message<Body<Value>>;

pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// An etcds node on `addr`, ready.
pub async fn etcd_on(addr: &str) -> EtcdNode {
    let mut cfg = etcds::cli::EtcdConfig::with_defaults();
    cfg.listen_client_urls = addr.to_string();
    let node = EtcdNode::init(cfg, slog::Logger::root(slog::Discard, slog::o!())).await.unwrap();
    node.serve().await.unwrap();
    let endpoint = format!("http://{}", addr);
    let mut c = Client::connect([endpoint.as_str()], None).await.unwrap();
    for _ in 0..50 {
        if c.status().await.is_ok() {
            return node;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("etcds on {} did not start", addr);
}

pub async fn etcd() -> (EtcdNode, String) {
    let addr = format!("127.0.0.1:{}", free_port());
    (etcd_on(&addr).await, format!("http://{}", addr))
}

/// How the broker settles the n-th delivery it gets (from 0).
#[derive(Clone, Debug)]
pub enum Settle {
    Accept,
    Reject(AmqpError),
    Release,
    Modify,
    /// never: the sender waits for an outcome that does not come
    Ignore,
    /// accept after a pause
    Delay(Duration),
    /// reject after a pause
    RejectAfter(Duration, AmqpError),
    /// drop the connection, unsettled
    Drop,
}

/// A broker on `listener`: every message it gets goes to the channel, and is
/// settled as `policy` says for its text and its number.
pub fn broker_on(listener: TcpListener, policy: impl Fn(usize, &str) -> Settle + Send + Sync + 'static) -> mpsc::UnboundedReceiver<Got> {
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
                    let t = text_of(d.body());
                    let _ = tx.send(d.message().clone());
                    let _ = match policy(n.fetch_add(1, Ordering::SeqCst), &t) {
                        Settle::Accept => recv.accept(&d).await,
                        Settle::Reject(c) => recv.reject(&d, AmqpErr::new(c, Some("test".to_string()), None)).await,
                        Settle::Release => recv.release(&d).await,
                        Settle::Modify => recv.modify(&d, Modified { delivery_failed: Some(true), undeliverable_here: None, message_annotations: None }).await,
                        Settle::Ignore => Ok(()),
                        Settle::Delay(p) => {
                            tokio::time::sleep(p).await;
                            recv.accept(&d).await
                        }
                        Settle::RejectAfter(p, c) => {
                            tokio::time::sleep(p).await;
                            recv.reject(&d, AmqpErr::new(c, Some("test".to_string()), None)).await
                        }
                        Settle::Drop => return, // the connection with it
                    };
                }
            });
        }
    });
    rx
}

pub async fn broker(policy: impl Fn(usize, &str) -> Settle + Send + Sync + 'static) -> (String, mpsc::UnboundedReceiver<Got>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("amqp://{}", listener.local_addr().unwrap());
    (url, broker_on(listener, policy))
}

pub fn text_of(b: &Body<Value>) -> String {
    match b {
        Body::Value(v) => match &v.0 {
            Value::String(s) => s.clone(),
            _ => String::new(),
        },
        _ => String::new(),
    }
}

pub fn text(m: &Got) -> String {
    match &m.body {
        Body::Value(v) => match &v.0 {
            Value::String(s) => s.clone(),
            other => panic!("not a string: {:?}", other),
        },
        other => panic!("not an amqp-value: {:?}", other),
    }
}

pub async fn next(rx: &mut mpsc::UnboundedReceiver<Got>) -> Got {
    tokio::time::timeout(Duration::from_secs(20), rx.recv()).await.expect("broker got nothing").unwrap()
}

pub fn config(etcd: &str, target: &str, queue: &str, dead_letter: Option<&str>) -> Config {
    Config {
        etcd: vec![etcd.to_string()],
        etcd_user: None,
        queue: queue.to_string(),
        prefix: None,
        consumer: Uuid::new_v4(),
        target_url: target.to_string(),
        address: format!("jms.{}", queue),
        shape: Shape::default(),
        dead_letter: dead_letter.map(str::to_string),
        buffer: 16,
        send_timeout: Duration::from_secs(5),
    }
}

/// The bridge's log lines, for a test that checks a path was taken.
#[derive(Clone, Default)]
pub struct Lines(pub Arc<Mutex<Vec<String>>>);

impl slog::Drain for Lines {
    type Ok = ();
    type Err = slog::Never;
    fn log(&self, r: &slog::Record, _: &slog::OwnedKVList) -> Result<(), slog::Never> {
        self.0.lock().unwrap().push(r.msg().to_string());
        Ok(())
    }
}

impl Lines {
    pub fn logger(&self) -> slog::Logger {
        slog::Logger::root(self.clone(), slog::o!())
    }

    pub fn any(&self, part: &str) -> bool {
        self.0.lock().unwrap().iter().any(|l| l.contains(part))
    }

    pub async fn wait_for(&self, part: &str, within: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + within;
        while tokio::time::Instant::now() < deadline {
            if self.any(part) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }
}

pub struct Running(oneshot::Sender<()>, JoinHandle<Result<(), String>>);

pub fn start(cfg: Config, log: slog::Logger) -> Running {
    let (stop, stopped) = oneshot::channel::<()>();
    let h = tokio::spawn(bridge::run(cfg, log, async {
        let _ = stopped.await;
    }));
    Running(stop, h)
}

pub async fn stop(r: Running) {
    let _ = r.0.send(());
    assert_eq!(r.1.await.unwrap(), Ok(()), "a shutdown is not an error");
}

/// What a fresh consumer of `queue` receives within `wait`: the values.
pub async fn drain(etcd: &str, queue: &str, wait: Duration) -> Vec<Vec<u8>> {
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

/// A TCP proxy to `upstream`: `pause` refuses new connections, `cut` also
/// drops the open ones, `resume` lets them through again.
pub struct Proxy {
    pub addr: String,
    open: Arc<AtomicBool>,
    conns: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl Proxy {
    pub async fn to(upstream: String) -> Proxy {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let open = Arc::new(AtomicBool::new(true));
        let conns: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::default();
        let (o, c) = (open.clone(), conns.clone());
        tokio::spawn(async move {
            while let Ok((mut inbound, _)) = listener.accept().await {
                if !o.load(Ordering::SeqCst) {
                    continue; // refused: dropped at once
                }
                let up = upstream.clone();
                let h = tokio::spawn(async move {
                    if let Ok(mut outbound) = TcpStream::connect(&up).await {
                        let _ = copy_bidirectional(&mut inbound, &mut outbound).await;
                    }
                });
                c.lock().unwrap().push(h);
            }
        });
        Proxy { addr, open, conns }
    }

    pub fn pause(&self) {
        self.open.store(false, Ordering::SeqCst);
    }

    pub fn cut(&self) {
        self.pause();
        for h in self.conns.lock().unwrap().drain(..) {
            h.abort();
        }
    }

    pub fn resume(&self) {
        self.open.store(true, Ordering::SeqCst);
    }
}
