//! End to end, when things go wrong: etcd away at the start or in the middle
//! (a proxy between, cut or refusing), a broker that drops the connection,
//! answers nothing, or answers `modified`. Each test checks the message is
//! neither lost nor acknowledged before it landed, and the bridge's log shows
//! the path it took.
#![cfg(feature = "bridge")]

mod bridge_common;

use std::time::Duration;

use bridge_common::*;
use etcd_client::Client;
use fe2o3_amqp::types::definitions::AmqpError;
use tokio::net::TcpListener;

/// etcd not there when the bridge starts: it tries again, and consumes once
/// etcd answers.
#[tokio::test(flavor = "multi_thread")]
async fn etcd_away_at_start_then_there() {
    let (_node, etcd) = etcd().await;
    let proxy = Proxy::to(etcd.trim_start_matches("http://").to_string()).await;
    proxy.pause();
    let (amqp, mut rx) = broker(|_, _| Settle::Accept).await;
    let lines = Lines::default();
    let b = start(config(&format!("http://{}", proxy.addr), &amqp, "late", None), lines.logger());

    assert!(lines.wait_for("etcd [", Duration::from_secs(5)).await, "the failed connect is logged");
    proxy.resume();
    assert!(lines.wait_for("consuming /q/late/c/", Duration::from_secs(10)).await);

    let mut c = Client::connect([etcd.as_str()], None).await.unwrap();
    c.put("/q/late/p/x", "after all", None).await.unwrap();
    assert_eq!(text(&next(&mut rx).await), "after all");
    tokio::time::sleep(Duration::from_millis(300)).await;
    stop(b).await;
    assert!(drain(&etcd, "late", Duration::from_secs(2)).await.is_empty());
}

/// etcds has no Auth service: a user set is refused there, said in the log,
/// and the bridge keeps trying rather than consuming unauthenticated.
#[tokio::test(flavor = "multi_thread")]
async fn etcd_credentials_are_used_and_refused_by_etcds() {
    let (_node, etcd) = etcd().await;
    let (amqp, _rx) = broker(|_, _| Settle::Accept).await;
    let mut cfg = config(&etcd, &amqp, "auth", None);
    cfg.etcd_user = Some(("u".into(), "p".into()));
    let lines = Lines::default();
    let b = start(cfg, lines.logger());
    assert!(lines.wait_for("etcd [", Duration::from_secs(5)).await);
    assert!(!lines.any("consuming"), "no consumer without the login");
    stop(b).await;
}

/// etcd cut while a message is at the broker: the watch fails, the
/// acknowledge cannot reach etcd and is given up, what the bridge held from the
/// old stream is dropped, and the new stream gets all of it again - the one
/// that was in flight sent twice (at least once), none lost.
#[tokio::test(flavor = "multi_thread")]
async fn etcd_cut_mid_run_loses_nothing() {
    let (_node, etcd) = etcd().await;
    let proxy = Proxy::to(etcd.trim_start_matches("http://").to_string()).await;
    let (amqp, mut rx) = broker(|n, _| if n == 0 { Settle::Delay(Duration::from_secs(2)) } else { Settle::Accept }).await;
    let lines = Lines::default();
    let b = start(config(&format!("http://{}", proxy.addr), &amqp, "cut", None), lines.logger());
    assert!(lines.wait_for("consuming", Duration::from_secs(5)).await);

    let mut c = Client::connect([etcd.as_str()], None).await.unwrap();
    for i in 0..3 {
        c.put(format!("/q/cut/p/k{}", i), format!("m{}", i), None).await.unwrap();
    }
    assert_eq!(text(&next(&mut rx).await), "m0"); // at the broker, its outcome 2 s away
    // m1 and m2 handed to the bridge too, waiting behind m0
    assert!(lines.wait_for("received /q/cut/", Duration::from_secs(5)).await);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while lines.0.lock().unwrap().iter().filter(|l| l.starts_with("received")).count() < 3 {
        assert!(tokio::time::Instant::now() < deadline, "etcd handed all three over");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    proxy.cut();
    assert!(lines.wait_for("not acknowledged", Duration::from_secs(15)).await, "the acknowledge gave up");
    proxy.resume();

    // the new stream: m0 again (it was never acknowledged), m1 and m2 once -
    // in the order etcd hands them out again, which is its own
    let mut got = vec![];
    while got.len() < 3 {
        got.push(text(&next(&mut rx).await));
    }
    got.sort();
    assert_eq!(got, vec!["m0", "m1", "m2"]);
    assert!(lines.any("from a closed stream"), "what the old stream held was dropped, not sent");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(rx.try_recv().is_err(), "nothing more");
    stop(b).await;
    assert!(drain(&etcd, "cut", Duration::from_secs(2)).await.is_empty(), "all acknowledged in the end");
}

/// A rejected message whose dead-letter put cannot reach etcd is not
/// acknowledged: it is kept, and dead-lettered once etcd is back.
#[tokio::test(flavor = "multi_thread")]
async fn a_dead_letter_put_that_fails_keeps_the_message() {
    let (_node, etcd) = etcd().await;
    let proxy = Proxy::to(etcd.trim_start_matches("http://").to_string()).await;
    let (amqp, mut rx) = broker(|_, _| Settle::RejectAfter(Duration::from_secs(1), AmqpError::DecodeError)).await;
    let lines = Lines::default();
    let mut cfg = config(&format!("http://{}", proxy.addr), &amqp, "bad", Some("bad-dlq"));
    cfg.send_timeout = Duration::from_secs(2);
    let b = start(cfg, lines.logger());
    assert!(lines.wait_for("consuming", Duration::from_secs(5)).await);

    // a direct client, not behind the proxy: put, and cut etcd as soon as
    // the broker has the message, before its rejection is handled
    let mut c = Client::connect([etcd.as_str()], None).await.unwrap();
    c.put("/q/bad/p/x", "bad one", None).await.unwrap();
    let _ = next(&mut rx).await;
    proxy.cut();
    assert!(lines.wait_for("dead-letter queue bad-dlq", Duration::from_secs(20)).await, "the failed put is logged");
    assert!(!lines.any("put on dead-letter queue"));
    proxy.resume();
    assert!(lines.wait_for("put on dead-letter queue bad-dlq", Duration::from_secs(30)).await, "dead-lettered once etcd is back");
    stop(b).await;
    assert_eq!(drain(&etcd, "bad-dlq", Duration::from_secs(2)).await, vec![b"bad one".to_vec()]);
    assert!(drain(&etcd, "bad", Duration::from_secs(1)).await.is_empty());
}

/// The broker drops the connection with the message unsettled: the bridge
/// connects again and sends it again; acknowledged once taken.
#[tokio::test(flavor = "multi_thread")]
async fn a_broker_that_drops_the_connection_gets_it_again() {
    let (_node, etcd) = etcd().await;
    let (amqp, mut rx) = broker(|n, _| if n == 0 { Settle::Drop } else { Settle::Accept }).await;
    let lines = Lines::default();
    let b = start(config(&etcd, &amqp, "drop", None), lines.logger());
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut c = Client::connect([etcd.as_str()], None).await.unwrap();
    c.put("/q/drop/p/x", "again", None).await.unwrap();
    let first = next(&mut rx).await;
    let second = next(&mut rx).await;
    assert_eq!((text(&first), text(&second)), ("again".into(), "again".into()));
    assert_eq!(first.properties.as_ref().unwrap().message_id, second.properties.as_ref().unwrap().message_id);
    assert!(lines.wait_for("send /q/drop/", Duration::from_secs(2)).await, "the lost link is logged");
    tokio::time::sleep(Duration::from_millis(300)).await;
    stop(b).await;
    assert!(drain(&etcd, "drop", Duration::from_secs(2)).await.is_empty());
}

/// `modified` is not taken: sent again, then acknowledged when accepted.
#[tokio::test(flavor = "multi_thread")]
async fn modified_is_sent_again() {
    let (_node, etcd) = etcd().await;
    let (amqp, mut rx) = broker(|n, _| if n == 0 { Settle::Modify } else { Settle::Accept }).await;
    let lines = Lines::default();
    let b = start(config(&etcd, &amqp, "mod", None), lines.logger());
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut c = Client::connect([etcd.as_str()], None).await.unwrap();
    c.put("/q/mod/p/x", "twice", None).await.unwrap();
    let _ = next(&mut rx).await;
    assert_eq!(text(&next(&mut rx).await), "twice");
    assert!(lines.any("not taken"));
    tokio::time::sleep(Duration::from_millis(300)).await;
    stop(b).await;
    assert!(drain(&etcd, "mod", Duration::from_secs(2)).await.is_empty());
}

/// A broker that takes the TCP connection and never says a word: the connect
/// times out and is tried again, and nothing is acknowledged.
#[tokio::test(flavor = "multi_thread")]
async fn a_silent_broker_times_out() {
    let (_node, etcd) = etcd().await;
    let silent = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("amqp://{}", silent.local_addr().unwrap());
    tokio::spawn(async move {
        let mut held = vec![];
        while let Ok((s, _)) = silent.accept().await {
            held.push(s);
        }
    });
    let lines = Lines::default();
    let mut cfg = config(&etcd, &url, "silent", None);
    cfg.send_timeout = Duration::from_secs(1);
    let b = start(cfg, lines.logger());
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut c = Client::connect([etcd.as_str()], None).await.unwrap();
    c.put("/q/silent/p/x", "kept", None).await.unwrap();
    assert!(lines.wait_for("no answer in", Duration::from_secs(5)).await);
    stop(b).await;
    assert_eq!(drain(&etcd, "silent", Duration::from_secs(3)).await, vec![b"kept".to_vec()]);
}
