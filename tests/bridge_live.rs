//! Against real brokers, `cargo test --features bridge --test bridge_live -- --ignored`, each test
//! named by its variable (a test whose variable is unset passes, saying so):
//!
//! | test | variable | e.g. |
//! |---|---|---|
//! | `live_amqp` | `ETCDS_BRIDGE_LIVE_AMQP` | `amqp://app:app@lab10:5672` |
//! | `live_mqtt` | `ETCDS_BRIDGE_LIVE_MQTT`, comma separated | `mqtt://lab10:1884,mqtt://app:app@lab10:1883` |
//! | `live_kafka` | `ETCDS_BRIDGE_LIVE_KAFKA` | `kafka://lab10:9092` |
//!
//! Each starts an etcds node in-process and the bridge, puts text, Unicode and
//! binary messages, reads them back from the broker with a client of its own
//! (MQTT, Kafka) and checks every one was acknowledged in etcd. What a JMS
//! client reads of the AMQP ones is checked by `live/jms-listener` (README,
//! Tests).
#![cfg(feature = "bridge")]

use std::time::Duration;

use etcd_client::{Client, EventType};
use etcds::bridge::{self, Config};
use etcds::bridge::cloudevents;
use etcds::bridge::mapping::Shape;
use etcds::bridge::broker::kafka::partition_of;
use etcds::cluster::EtcdNode;
use uuid::Uuid;

const MESSAGES: [(&str, &[u8]); 3] = [("t1", b"hello broker"), ("t2", "привет 日本".as_bytes()), ("b1", &[0xff, 0x00, 0xfe])];

fn var(name: &str) -> Option<String> {
    let v = std::env::var(name).ok().filter(|v| !v.is_empty());
    if v.is_none() {
        eprintln!("{} not set: skipped", name);
    }
    v
}

fn log() -> slog::Logger {
    use sloggers::Build;
    let mut b = sloggers::terminal::TerminalLoggerBuilder::new();
    b.level(sloggers::types::Severity::Debug).destination(sloggers::terminal::Destination::Stderr);
    b.build().unwrap()
}

/// Runs the bridge from a fresh etcds queue to `target`, puts [`MESSAGES`],
/// and checks they were all acknowledged.
async fn bridge_once(target: &str, address: &str, shape: Shape) {
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let mut ec = etcds::cli::EtcdConfig::with_defaults();
    ec.listen_client_urls = format!("127.0.0.1:{}", port);
    let node = EtcdNode::init(ec, slog::Logger::root(slog::Discard, slog::o!())).await.unwrap();
    node.serve().await.unwrap();
    let etcd = format!("http://127.0.0.1:{}", port);
    tokio::time::sleep(Duration::from_millis(300)).await;

    let cfg = Config {
        etcd: vec![etcd.clone()],
        etcd_user: None,
        queue: "live".into(),
        prefix: None,
        consumer: Uuid::new_v4(),
        target_url: target.to_string(),
        address: address.to_string(),
        shape,
        dead_letter: None,
        buffer: 16,
        send_timeout: Duration::from_secs(10),
    };
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let run = tokio::spawn(bridge::run(cfg, log(), async {
        let _ = stopped.await;
    }));

    let mut c = Client::connect([etcd.as_str()], None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    for (k, v) in MESSAGES {
        c.put(format!("/q/live/p/{}", k), v, None).await.unwrap();
    }
    tokio::time::sleep(Duration::from_secs(5)).await;
    let _ = stop.send(());
    assert_eq!(run.await.unwrap(), Ok(()));

    // nothing left in etcd: all acknowledged
    let mut w = c.watch(format!("/q/live/c/{}", Uuid::new_v4()), None).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while let Ok(Ok(Some(resp))) = tokio::time::timeout_at(deadline, w.message()).await {
        for ev in resp.events() {
            assert!(ev.event_type() != EventType::Put, "left unacknowledged: {:?}", ev.kv().map(|kv| kv.key_str().unwrap_or_default().to_string()));
        }
    }
    drop(node);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn live_amqp() {
    let Some(amqp) = var("ETCDS_BRIDGE_LIVE_AMQP") else { return };
    let address = std::env::var("ETCDS_BRIDGE_LIVE_ADDRESS").unwrap_or_else(|_| "e2j.live".into());
    bridge_once(&amqp, &address, Shape { cloudevents: true, ..Shape::default() }).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn live_mqtt() {
    use rumqttc::v5::mqttbytes::QoS;
    use rumqttc::v5::mqttbytes::v5::Packet;
    use rumqttc::v5::{AsyncClient, Event, MqttOptions};

    let Some(urls) = var("ETCDS_BRIDGE_LIVE_MQTT") else { return };
    for url in urls.split(',') {
        for cloudevents in [false, true] {
            eprintln!("--- {} cloudevents={}", url, cloudevents);
            let prefix = format!("e2j/{}", Uuid::new_v4().simple());
            // a subscriber before the bridge publishes: MQTT holds nothing for
            // a topic nobody subscribed to
            let (user, hostport) = url.split_once("://").unwrap().1.rsplit_once('@').map_or((None, url.split_once("://").unwrap().1), |(u, h)| (Some(u), h));
            let (host, port) = hostport.rsplit_once(':').unwrap();
            let mut opts = MqttOptions::new(format!("e2j-sub-{}", Uuid::new_v4().simple()), host, port.parse().unwrap());
            if let Some(u) = user {
                let (n, p) = u.split_once(':').unwrap();
                opts.set_credentials(n, p);
            }
            let (sub, mut el) = AsyncClient::new(opts, 10);
            sub.subscribe(format!("{}/#", prefix), QoS::AtLeastOnce).await.unwrap();
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let poll = tokio::spawn(async move {
                loop {
                    match el.poll().await {
                        Ok(Event::Incoming(Packet::Publish(p))) => { let _ = tx.send(p); }
                        Ok(_) => {}
                        Err(e) => { eprintln!("subscriber: {}", e); tokio::time::sleep(Duration::from_millis(200)).await; }
                    }
                }
            });
            tokio::time::sleep(Duration::from_millis(500)).await;

            bridge_once(url, &prefix, Shape { cloudevents, ..Shape::default() }).await;

            for (k, v) in MESSAGES {
                let p = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.expect("published").unwrap();
                assert_eq!(std::str::from_utf8(&p.topic).unwrap(), format!("{}/{}", prefix, k));
                assert_eq!(p.payload.as_ref(), v);
                let props = p.properties.expect("properties");
                let text = std::str::from_utf8(v).is_ok();
                assert_eq!(props.content_type.as_deref(), Some(if text { cloudevents::TEXT } else { cloudevents::BYTES }));
                let user = |n: &str| props.user_properties.iter().find(|(a, _)| a == n).map(|(_, b)| b.clone());
                if cloudevents {
                    assert_eq!(user("specversion").as_deref(), Some("1.0"));
                    assert_eq!(user("subject").as_deref(), Some(k));
                    assert_eq!(user("source").as_deref(), Some("/q/live"));
                    assert!(user("id").is_some() && user("time").is_some());
                } else {
                    assert_eq!(user("etcd_key").as_deref(), Some(k));
                    assert_eq!(user("etcd_queue").as_deref(), Some("live"));
                    assert!(user("message_id").is_some());
                }
            }
            poll.abort();
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn live_kafka() {
    use rskafka::client::ClientBuilder;
    use rskafka::client::partition::UnknownTopicHandling;

    let Some(url) = var("ETCDS_BRIDGE_LIVE_KAFKA") else { return };
    let brokers: Vec<String> = url.split_once("://").unwrap().1.split(',').map(str::to_string).collect();
    let client = ClientBuilder::new(brokers).build().await.unwrap();
    let topic = format!("e2j-{}", Uuid::new_v4().simple());
    client.controller_client().unwrap().create_topic(topic.clone(), 3, 1, 5_000).await.unwrap();

    bridge_once(&url, &topic, Shape { cloudevents: true, ..Shape::default() }).await;

    let mut got = Vec::new();
    for partition in 0..3 {
        let pc = client.partition_client(topic.clone(), partition, UnknownTopicHandling::Retry).await.unwrap();
        let (records, _) = pc.fetch_records(0, 1..1_000_000, 1_000).await.unwrap();
        for r in records {
            got.push((partition, r.record));
        }
    }
    assert_eq!(got.len(), MESSAGES.len(), "each once");
    for (k, v) in MESSAGES {
        let (partition, r) = got.iter().find(|(_, r)| r.key.as_deref() == Some(k.as_bytes())).expect(k);
        assert_eq!(r.value.as_deref(), Some(v));
        assert_eq!(*partition as usize, partition_of(k.as_bytes(), 3), "the key's partition, as Kafka's own producer picks it");
        let text = std::str::from_utf8(v).is_ok();
        assert_eq!(r.headers["content-type"], if text { cloudevents::TEXT } else { cloudevents::BYTES }.as_bytes());
        assert_eq!(r.headers["ce_specversion"], b"1.0");
        assert_eq!(r.headers["ce_subject"], k.as_bytes());
        assert_eq!(r.headers["ce_source"], b"/q/live");
    }
}

/// A watched key prefix to Kafka: a put is a record, a delete a tombstone
/// (no value) with the same key, in the key's partition.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn live_kafka_tombstones() {
    use rskafka::client::ClientBuilder;
    use rskafka::client::partition::UnknownTopicHandling;

    let Some(url) = var("ETCDS_BRIDGE_LIVE_KAFKA") else { return };
    let brokers: Vec<String> = url.split_once("://").unwrap().1.split(',').map(str::to_string).collect();
    let client = ClientBuilder::new(brokers).build().await.unwrap();
    let topic = format!("e2j-kv-{}", Uuid::new_v4().simple());
    client.controller_client().unwrap().create_topic(topic.clone(), 3, 1, 5_000).await.unwrap();

    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let mut ec = etcds::cli::EtcdConfig::with_defaults();
    ec.listen_client_urls = format!("127.0.0.1:{}", port);
    let node = EtcdNode::init(ec, slog::Logger::root(slog::Discard, slog::o!())).await.unwrap();
    node.serve().await.unwrap();
    let etcd = format!("http://127.0.0.1:{}", port);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let cfg = Config {
        etcd: vec![etcd.clone()],
        etcd_user: None,
        queue: String::new(),
        prefix: Some("/cfg/".into()),
        consumer: Uuid::new_v4(),
        target_url: url.clone(),
        address: topic.clone(),
        shape: Shape::default(),
        dead_letter: None,
        buffer: 16,
        send_timeout: Duration::from_secs(10),
    };
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let run = tokio::spawn(bridge::run(cfg, log(), async {
        let _ = stopped.await;
    }));
    let mut c = Client::connect([etcd.as_str()], None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    c.put("/cfg/db", "pg://a", None).await.unwrap();
    c.delete("/cfg/db", None).await.unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;
    let _ = stop.send(());
    assert_eq!(run.await.unwrap(), Ok(()));

    let p = partition_of(b"db", 3) as i32;
    let pc = client.partition_client(topic.clone(), p, UnknownTopicHandling::Retry).await.unwrap();
    let (records, _) = pc.fetch_records(0, 1..1_000_000, 1_000).await.unwrap();
    let got: Vec<_> = records.iter().map(|r| (r.record.key.clone(), r.record.value.clone(), r.record.headers.get("etcd_event").cloned())).collect();
    assert_eq!(got, vec![
        (Some(b"db".to_vec()), Some(b"pg://a".to_vec()), Some(b"put".to_vec())),
        (Some(b"db".to_vec()), None, Some(b"delete".to_vec())),
    ], "a record, then its tombstone, in the key's partition");
    drop(node);
}
