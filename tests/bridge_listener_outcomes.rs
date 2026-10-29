//! The listener against what a JMS client may do: take several before it
//! acknowledges, release, reject, give no credit, log in wrong, ask for a
//! queue that is no queue. A message is acknowledged in etcd only when the
//! client accepted it - or, refused for good, when it is on the dead-letter
//! queue.
#![cfg(feature = "bridge")]

mod bridge_common;

use std::time::Duration;

use bridge_common::*;
use etcd_client::Client;
use etcds::bridge::Config;
use etcds::bridge::listener::{self, Listen};
use etcds::bridge::mapping::{BodyType, Shape};
use fe2o3_amqp::link::receiver::CreditMode;
use fe2o3_amqp::sasl_profile::SaslProfile;
use fe2o3_amqp::types::definitions::{AmqpError, Error as AmqpErr};
use fe2o3_amqp::types::messaging::{Body, Source};
use fe2o3_amqp::types::primitives::Value;
use fe2o3_amqp::{Connection, Receiver, Session};
use tokio::sync::oneshot;
use uuid::Uuid;

struct Listening {
    url: String,
    stop: oneshot::Sender<()>,
    h: tokio::task::JoinHandle<Result<(), String>>,
}

async fn listening(etcd: &str, user: Option<(&str, &str)>, shape: Shape, dlq: Option<&str>, log: slog::Logger) -> Listening {
    let bind = format!("127.0.0.1:{}", free_port());
    let mut template: Config = config(etcd, "", "", dlq);
    template.shape = shape;
    template.buffer = 8;
    template.send_timeout = Duration::from_secs(1);
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
    Listening { url, stop, h }
}

impl Listening {
    async fn stop(self) {
        let _ = self.stop.send(());
        assert_eq!(self.h.await.unwrap(), Ok(()));
    }
}

async fn client(url: &str) -> fe2o3_amqp::connection::ConnectionHandle<()> {
    let b = Connection::builder().container_id(format!("jms-{}", Uuid::new_v4()));
    if url.contains('@') { b.open(url).await } else { b.sasl_profile(SaslProfile::Anonymous).open(url).await }.unwrap()
}

async fn receiver(session: &mut fe2o3_amqp::session::SessionHandle<()>, queue: &str, credit: CreditMode) -> Receiver {
    Receiver::builder()
        .name(format!("c-{}", Uuid::new_v4()))
        .source(Source::builder().address(queue).build())
        .credit_mode(credit)
        .attach(session)
        .await
        .unwrap()
}

async fn recv(r: &mut Receiver) -> fe2o3_amqp::link::delivery::Delivery<Body<Value>> {
    tokio::time::timeout(Duration::from_secs(10), r.recv::<Body<Value>>()).await.expect("delivered").unwrap()
}

async fn put(etcd: &str, queue: &str, key: &str, v: impl Into<Vec<u8>>) {
    let mut c = Client::connect([etcd], None).await.unwrap();
    c.put(format!("/q/{}/p/{}", queue, key), v.into(), None).await.unwrap();
}

/// CLIENT_ACKNOWLEDGE's `receive(); receive(); receive(); acknowledge()`:
/// three out at once, each acknowledged in etcd as the client settles it.
#[tokio::test(flavor = "multi_thread")]
async fn a_client_that_acknowledges_in_a_batch() {
    let (_node, etcd) = etcd().await;
    let l = listening(&etcd, None, Shape::default(), None, slog::Logger::root(slog::Discard, slog::o!())).await;
    let mut conn = client(&l.url).await;
    let mut session = Session::begin(&mut conn).await.unwrap();
    let mut r = receiver(&mut session, "batch", CreditMode::default()).await;
    for i in 0..3 {
        put(&etcd, "batch", &format!("k{}", i), format!("b{}", i)).await;
    }
    let mut ds = vec![];
    for _ in 0..3 {
        ds.push(recv(&mut r).await); // none settled yet
    }
    for d in &ds {
        r.accept(d).await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    r.close().await.unwrap();
    session.end().await.unwrap();
    conn.close().await.unwrap();
    assert!(drain(&etcd, "batch", Duration::from_secs(2)).await.is_empty(), "all three acknowledged");
    l.stop().await;
}

/// A release is not taken: sent again, then acknowledged when accepted.
#[tokio::test(flavor = "multi_thread")]
async fn a_released_message_is_sent_again() {
    let (_node, etcd) = etcd().await;
    let lines = Lines::default();
    let l = listening(&etcd, Some(("app", "pw")), Shape::default(), None, lines.logger()).await;
    let mut conn = client(&l.url).await;
    let mut session = Session::begin(&mut conn).await.unwrap();
    let mut r = receiver(&mut session, "rel", CreditMode::default()).await;
    put(&etcd, "rel", "x", "back").await;
    let d = recv(&mut r).await;
    r.release(&d).await.unwrap();
    let d = recv(&mut r).await;
    assert_eq!(text_of(d.body()), "back");
    r.accept(&d).await.unwrap();
    assert!(lines.wait_for("sending again", Duration::from_secs(2)).await);
    tokio::time::sleep(Duration::from_millis(300)).await;
    r.close().await.unwrap();
    assert!(drain(&etcd, "rel", Duration::from_secs(2)).await.is_empty());
    l.stop().await;
}

/// A client's rejection as malformed: dead-lettered, then acknowledged; with
/// no dead-letter queue, kept - back on the line when the client goes.
#[tokio::test(flavor = "multi_thread")]
async fn a_rejected_message_is_dead_lettered_or_kept() {
    let (_node, etcd) = etcd().await;
    for dlq in [Some("rej-dlq"), None] {
        let l = listening(&etcd, None, Shape::default(), dlq, slog::Logger::root(slog::Discard, slog::o!())).await;
        let mut conn = client(&l.url).await;
        let mut session = Session::begin(&mut conn).await.unwrap();
        let mut r = receiver(&mut session, "rej", CreditMode::default()).await;
        put(&etcd, "rej", "x", "malformed").await;
        let d = recv(&mut r).await;
        r.reject(&d, AmqpErr::new(AmqpError::DecodeError, None, None)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        r.close().await.unwrap();
        session.end().await.unwrap();
        conn.close().await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        match dlq {
            Some(q) => {
                assert_eq!(drain(&etcd, q, Duration::from_secs(2)).await, vec![b"malformed".to_vec()]);
                assert!(drain(&etcd, "rej", Duration::from_secs(1)).await.is_empty());
            }
            None => assert_eq!(drain(&etcd, "rej", Duration::from_secs(3)).await, vec![b"malformed".to_vec()]),
        }
        l.stop().await;
    }
}

/// `--body text` and a value that is not UTF-8: never sent, dead-lettered.
#[tokio::test(flavor = "multi_thread")]
async fn a_value_that_cannot_be_text_is_dead_lettered() {
    let (_node, etcd) = etcd().await;
    let shape = Shape { body: BodyType::Text, ..Shape::default() };
    let l = listening(&etcd, None, shape, Some("txt-dlq"), slog::Logger::root(slog::Discard, slog::o!())).await;
    let mut conn = client(&l.url).await;
    let mut session = Session::begin(&mut conn).await.unwrap();
    let mut r = receiver(&mut session, "txt", CreditMode::default()).await;
    put(&etcd, "txt", "bin", vec![0xffu8, 0xfe]).await;
    put(&etcd, "txt", "ok", "text").await;
    let d = recv(&mut r).await;
    assert_eq!(text_of(d.body()), "text", "the binary one never reached the client");
    r.accept(&d).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    r.close().await.unwrap();
    assert_eq!(drain(&etcd, "txt-dlq", Duration::from_secs(2)).await, vec![vec![0xffu8, 0xfe]]);
    l.stop().await;
}

/// A client that gives no credit gets nothing, and is waited for - not
/// dropped; with credit, the message arrives.
#[tokio::test(flavor = "multi_thread")]
async fn a_client_without_credit_is_waited_for() {
    let (_node, etcd) = etcd().await;
    let lines = Lines::default();
    let l = listening(&etcd, None, Shape::default(), None, lines.logger()).await;
    let mut conn = client(&l.url).await;
    let mut session = Session::begin(&mut conn).await.unwrap();
    let mut r = receiver(&mut session, "credit", CreditMode::Manual).await;
    put(&etcd, "credit", "x", "patience").await;
    tokio::time::sleep(Duration::from_secs(3)).await; // past the 1 s send timeout, twice
    assert!(!lines.any("the link failed"), "a client without credit is no failed link");
    r.set_credit(1).await.unwrap();
    let d = recv(&mut r).await;
    assert_eq!(text_of(d.body()), "patience");
    r.accept(&d).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    r.close().await.unwrap();
    assert!(drain(&etcd, "credit", Duration::from_secs(2)).await.is_empty());
    l.stop().await;
}

/// A wrong password is refused at the connection; a queue name that is no
/// queue, at the link.
#[tokio::test(flavor = "multi_thread")]
async fn wrong_login_and_no_queue_are_refused() {
    let (_node, etcd) = etcd().await;
    let lines = Lines::default();
    let l = listening(&etcd, Some(("app", "pw")), Shape::default(), None, lines.logger()).await;
    let wrong = l.url.replace("app:pw@", "app:nope@");
    let r = Connection::builder().container_id("intruder").open(wrong.as_str()).await;
    assert!(r.is_err(), "a wrong password gets no connection");

    let mut conn = client(&l.url).await;
    let mut session = Session::begin(&mut conn).await.unwrap();
    let attached = Receiver::builder()
        .name("c-bad")
        .source(Source::builder().address("not/a/queue").build())
        .attach(&mut session)
        .await;
    if let Ok(mut r) = attached {
        let got = tokio::time::timeout(Duration::from_secs(2), r.recv::<Body<Value>>()).await;
        assert!(!matches!(got, Ok(Ok(_))), "nothing on a refused link");
    }
    assert!(lines.wait_for("link refused", Duration::from_secs(2)).await);
    l.stop().await;
}
