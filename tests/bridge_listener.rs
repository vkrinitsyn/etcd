//! The listener, in-process: an etcds node, the bridge listening for AMQP
//! clients, and fe2o3 as the client - a JMS consumer would attach the same
//! receiver link. A message is acknowledged in etcd when the client accepts it.
#![cfg(feature = "bridge")]

use std::time::Duration;

use etcd_client::{Client, EventType};
use etcds::bridge::Config;
use etcds::bridge::listener::{self, Listen};
use etcds::bridge::mapping::Shape;
use etcds::cluster::EtcdNode;
use fe2o3_amqp::types::messaging::{Body, Source};
use fe2o3_amqp::types::primitives::Value;
use fe2o3_amqp::sasl_profile::SaslProfile;
use fe2o3_amqp::{Connection, Receiver, Sender, Session};
use tokio::sync::oneshot;
use uuid::Uuid;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

async fn etcd() -> (EtcdNode, String) {
    let addr = format!("127.0.0.1:{}", free_port());
    let mut cfg = etcds::cli::EtcdConfig::with_defaults();
    cfg.listen_client_urls = addr.clone();
    let node = EtcdNode::init(cfg, slog::Logger::root(slog::Discard, slog::o!())).await.unwrap();
    node.serve().await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    (node, format!("http://{}", addr))
}

/// The listener on a free port; the URL a client connects to, and its stop.
async fn listening(etcd: &str, user: Option<(&str, &str)>) -> (String, oneshot::Sender<()>, tokio::task::JoinHandle<Result<(), String>>) {
    listening_logged(etcd, user, slog::Logger::root(slog::Discard, slog::o!())).await
}

/// The bridge's log lines, for a test that checks a path was taken.
#[derive(Clone, Default)]
struct Lines(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

impl slog::Drain for Lines {
    type Ok = ();
    type Err = slog::Never;
    fn log(&self, r: &slog::Record, _: &slog::OwnedKVList) -> Result<(), slog::Never> {
        self.0.lock().unwrap().push(r.msg().to_string());
        Ok(())
    }
}

async fn listening_logged(etcd: &str, user: Option<(&str, &str)>, log: slog::Logger) -> (String, oneshot::Sender<()>, tokio::task::JoinHandle<Result<(), String>>) {
    let bind = format!("127.0.0.1:{}", free_port());
    let template = Config {
        etcd: vec![etcd.to_string()],
        etcd_user: None,
        queue: String::new(),
        prefix: None,
        consumer: Uuid::new_v4(),
        target_url: String::new(),
        address: String::new(),
        shape: Shape::default(),
        dead_letter: None,
        buffer: 16,
        send_timeout: Duration::from_secs(5),
    };
    let l = Listen { bind: bind.clone(), user: user.map(|(u, p)| (u.to_string(), p.to_string())) };
    let (stop, stopped) = oneshot::channel::<()>();
    let h = tokio::spawn(listener::listen(template, l, log, async {
        let _ = stopped.await;
    }));
    tokio::time::sleep(Duration::from_millis(200)).await;
    let url = match user {
        Some((u, p)) => format!("amqp://{}:{}@{}", u, p, bind),
        None => format!("amqp://{}", bind),
    };
    (url, stop, h)
}

/// A client with SASL ANONYMOUS, as a JMS client with no user connects.
async fn anonymous(id: &str, url: &str) -> fe2o3_amqp::connection::ConnectionHandle<()> {
    Connection::builder()
        .container_id(id)
        .sasl_profile(SaslProfile::Anonymous)
        .open(url)
        .await
        .unwrap()
}

fn text(b: &Body<Value>) -> String {
    match b {
        Body::Value(v) => match &v.0 {
            Value::String(s) => s.clone(),
            o => panic!("{:?}", o),
        },
        o => panic!("{:?}", o),
    }
}

/// What a fresh consumer of `queue` receives within `wait`.
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

#[tokio::test(flavor = "multi_thread")]
async fn a_client_receives_and_its_accept_acknowledges() {
    let (_node, etcd) = etcd().await;
    let (url, stop, h) = listening(&etcd, Some(("app", "pw"))).await;

    let mut conn = Connection::open("jms-client", url.as_str()).await.unwrap();
    let mut session = Session::begin(&mut conn).await.unwrap();
    let source = Source::builder().address("orders").build();
    let mut recv = Receiver::builder().name("c1").source(source).attach(&mut session).await.unwrap();

    let mut c = Client::connect([etcd.as_str()], None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    for i in 0..3 {
        c.put(format!("/q/orders/p/o{}", i), format!("order {}", i), None).await.unwrap();
    }
    for i in 0..3 {
        let d = tokio::time::timeout(Duration::from_secs(5), recv.recv::<Body<Value>>()).await.expect("delivered").unwrap();
        assert_eq!(text(d.body()), format!("order {}", i));
        let p = d.message().properties.as_ref().unwrap();
        assert!(format!("{:?}", p.message_id).contains("ID:"));
        recv.accept(&d).await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    recv.close().await.unwrap();
    session.end().await.unwrap();
    conn.close().await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(drain(&etcd, "orders", Duration::from_secs(2)).await.is_empty(), "all accepted, all acknowledged");

    let _ = stop.send(());
    assert_eq!(h.await.unwrap(), Ok(()));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_that_leaves_unaccepted_leaves_it_on_the_line() {
    let (_node, etcd) = etcd().await;
    let (url, stop, h) = listening(&etcd, None).await;

    let mut conn = anonymous("jms-client", &url).await;
    let mut session = Session::begin(&mut conn).await.unwrap();
    let source = Source::builder().address("jobs").build();
    let mut recv = Receiver::builder().name("c1").source(source).attach(&mut session).await.unwrap();

    let mut c = Client::connect([etcd.as_str()], None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    c.put("/q/jobs/p/j", "not done", None).await.unwrap();
    let d = tokio::time::timeout(Duration::from_secs(5), recv.recv::<Body<Value>>()).await.expect("delivered").unwrap();
    assert_eq!(text(d.body()), "not done");
    // gone without a disposition
    drop(recv);
    drop(session);
    drop(conn);

    // the link's etcd consumer ends with it, and the message is the line's again
    assert_eq!(drain(&etcd, "jobs", Duration::from_secs(8)).await, vec![b"not done".to_vec()]);
    let _ = stop.send(());
    assert_eq!(h.await.unwrap(), Ok(()));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_producer_link_is_refused() {
    let (_node, etcd) = etcd().await;
    let (url, stop, h) = listening(&etcd, None).await;
    let mut conn = anonymous("jms-producer", &url).await;
    let mut session = Session::begin(&mut conn).await.unwrap();
    // refused: an error, or - fe2o3's sender waits for credit and may not
    // notice the detach - no send ever completes
    let r = tokio::time::timeout(Duration::from_secs(5), async {
        match Sender::attach(&mut session, "p1", "jobs").await {
            Ok(mut s) => s.send("x").await.map(|_| ()).map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        }
    })
    .await;
    assert!(!matches!(r, Ok(Ok(()))), "a producer gets nowhere: {:?}", r);
    // and nothing reached etcd
    assert!(drain(&etcd, "jobs", Duration::from_secs(1)).await.is_empty());
    let _ = stop.send(());
    assert_eq!(h.await.unwrap(), Ok(()));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_that_closes_while_idle_ends_its_consumer_at_once() {
    let (_node, etcd) = etcd().await;
    let lines = Lines::default();
    let (url, stop, h) = listening_logged(&etcd, None, slog::Logger::root(lines.clone(), slog::o!())).await;

    let mut conn = anonymous("jms-client", &url).await;
    let mut session = Session::begin(&mut conn).await.unwrap();
    let source = Source::builder().address("idle").build();
    let recv = Receiver::builder().name("c1").source(source).attach(&mut session).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    recv.close().await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let seen = lines.0.lock().unwrap().clone();
    assert!(seen.iter().any(|l| l.contains("the client left")), "{:?}", seen);

    // nothing waits on the gone link: a new consumer gets the next message
    let mut c = Client::connect([etcd.as_str()], None).await.unwrap();
    c.put("/q/idle/p/x", "next", None).await.unwrap();
    assert_eq!(drain(&etcd, "idle", Duration::from_secs(3)).await, vec![b"next".to_vec()]);
    let _ = stop.send(());
    assert_eq!(h.await.unwrap(), Ok(()));
}
