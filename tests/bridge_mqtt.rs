//! [bridge] End to end, MQTT 5 with no broker to install: a small one of its own that speaks
//! the packets the bridge uses - CONNECT, PUBLISH at QoS 1, PINGREQ, DISCONNECT - and answers
//! each PUBLISH with the reason a test says. tests/bridge_live.rs is the same against real
//! brokers, by hand. Each test checks what reached the broker, and what was acknowledged in etcd.
#![cfg(feature = "bridge")]

mod bridge_common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bridge_common::*;
use etcd_client::Client;
use etcds::bridge::Config;
use etcds::bridge::mapping::Shape;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

/// PUBACK reasons (MQTT 5, 3.4.2.1)
const SUCCESS: u8 = 0x00;
const NO_SUBSCRIBERS: u8 = 0x10;
const TOPIC_INVALID: u8 = 0x90;
const QUOTA_EXCEEDED: u8 = 0x97;
const PAYLOAD_INVALID: u8 = 0x99;

#[derive(Debug, Clone)]
struct Published {
    topic: String,
    /// its properties as they came: `has` looks in them
    properties: Vec<u8>,
    payload: Vec<u8>,
}

impl Published {
    fn has(&self, part: &str) -> bool {
        self.properties.windows(part.len()).any(|w| w == part.as_bytes())
    }
}

/// A variable byte integer (MQTT 5, 1.5.5): its value and the bytes it took
fn varint(b: &[u8]) -> (usize, usize) {
    let (mut v, mut n) = (0usize, 0);
    while n < 4 {
        v |= ((b[n] & 0x7f) as usize) << (7 * n);
        n += 1;
        if b[n - 1] & 0x80 == 0 {
            break;
        }
    }
    (v, n)
}

/// A packet: its first byte and what follows the length
async fn packet(s: &mut TcpStream) -> Option<(u8, Vec<u8>)> {
    let first = s.read_u8().await.ok()?;
    let mut len = Vec::new();
    loop {
        let b = s.read_u8().await.ok()?;
        len.push(b);
        if b & 0x80 == 0 {
            break;
        }
    }
    let mut body = vec![0; varint(&len).0];
    s.read_exact(&mut body).await.ok()?;
    Some((first, body))
}

/// A PUBLISH at QoS 1: its packet id, and the message
fn publish(body: &[u8]) -> ([u8; 2], Published) {
    let topic_len = u16::from_be_bytes([body[0], body[1]]) as usize;
    let topic = String::from_utf8_lossy(&body[2..2 + topic_len]).into_owned();
    let at = 2 + topic_len;
    let id = [body[at], body[at + 1]];
    let (props, took) = varint(&body[at + 2..]);
    let start = at + 2 + took;
    (id, Published { topic, properties: body[start..start + props].to_vec(), payload: body[start + props..].to_vec() })
}

/// A broker on a free port. `reason` says how the n-th PUBLISH (from 0) is answered; `refuse`
/// answers each CONNECT with that reason instead of taking it
async fn broker(refuse: Option<u8>, reason: impl Fn(usize) -> u8 + Send + Sync + 'static) -> (String, mpsc::UnboundedReceiver<Published>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("mqtt://{}", listener.local_addr().unwrap());
    let (tx, rx) = mpsc::unbounded_channel();
    let (reason, n) = (Arc::new(reason), Arc::new(AtomicUsize::new(0)));
    tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            let (tx, reason, n) = (tx.clone(), reason.clone(), n.clone());
            tokio::spawn(async move {
                while let Some((first, body)) = packet(&mut s).await {
                    let answer = match first >> 4 {
                        1 => vec![0x20, 3, 0, refuse.unwrap_or(0), 0], // CONNACK, no properties
                        3 => {
                            let (id, m) = publish(&body);
                            let _ = tx.send(m);
                            vec![0x40, 4, id[0], id[1], reason(n.fetch_add(1, Ordering::SeqCst)), 0] // PUBACK
                        }
                        12 => vec![0xd0, 0], // PINGRESP
                        14 => return,        // DISCONNECT
                        _ => continue,
                    };
                    if s.write_all(&answer).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    (url, rx)
}

async fn got(rx: &mut mpsc::UnboundedReceiver<Published>) -> Published {
    tokio::time::timeout(Duration::from_secs(15), rx.recv()).await.expect("nothing published").unwrap()
}

fn mqtt_config(etcd: &str, url: &str, queue: &str, dead_letter: Option<&str>) -> Config {
    Config { address: "etcd/jobs".to_string(), ..config(etcd, url, queue, dead_letter) }
}

/// Published in order at QoS 1 to `{address}/{key}`, with the properties, acknowledged once
/// the broker took it; a key is one topic level. A PUBACK of no subscribers is taken.
#[tokio::test(flavor = "multi_thread")]
async fn published_in_order_and_acknowledged() {
    let (_node, etcd) = etcd().await;
    let (url, mut rx) = broker(None, |n| if n == 1 { NO_SUBSCRIBERS } else { SUCCESS }).await;
    let lines = Lines::default();
    let b = start(mqtt_config(&etcd, &url, "mq", None), lines.logger());
    assert!(lines.wait_for("consuming /q/mq/c/", Duration::from_secs(10)).await);
    let mut c = Client::connect([etcd.as_str()], None).await.unwrap();
    c.put("/q/mq/p/a%2Bb", "first", None).await.unwrap();
    c.put("/q/mq/p/k2", vec![0xff, 0xfe], None).await.unwrap();
    c.put("/q/mq/p/k3", "third", None).await.unwrap();

    let (m1, m2, m3) = (got(&mut rx).await, got(&mut rx).await, got(&mut rx).await);
    assert_eq!((m1.topic.as_str(), m1.payload.as_slice()), ("etcd/jobs/a%252Bb", &b"first"[..]));
    assert!(m1.has("etcd_queue") && m1.has("etcd_key") && m1.has("message_id") && m1.has("text/plain"), "{:?}", m1);
    assert_eq!(m2.payload, vec![0xff, 0xfe]);
    assert!(m2.has("application/octet-stream"), "{:?}", m2);
    assert_eq!(m3.payload, b"third");
    tokio::time::sleep(Duration::from_millis(300)).await;
    stop(b).await;
    assert!(drain(&etcd, "mq", Duration::from_secs(2)).await.is_empty(), "all acknowledged");
}

/// A CloudEvent in MQTT's binding: the attributes as user properties.
#[tokio::test(flavor = "multi_thread")]
async fn a_cloudevent_as_user_properties() {
    let (_node, etcd) = etcd().await;
    let (url, mut rx) = broker(None, |_| SUCCESS).await;
    let lines = Lines::default();
    let mut cfg = mqtt_config(&etcd, &url, "mce", None);
    cfg.shape = Shape { cloudevents: true, ce_type: "yt.done".to_string(), ..Shape::default() };
    let b = start(cfg, lines.logger());
    assert!(lines.wait_for("consuming /q/mce/c/", Duration::from_secs(10)).await);
    Client::connect([etcd.as_str()], None).await.unwrap().put("/q/mce/p/k", "v", None).await.unwrap();
    let m = got(&mut rx).await;
    assert!(m.has("specversion") && m.has("yt.done") && m.has("etcdqueue"), "{:?}", m);
    assert!(!m.has("message_id"), "the attributes instead");
    stop(b).await;
}

/// What the message itself will never do goes to the dead-letter queue, acknowledged.
#[tokio::test(flavor = "multi_thread")]
async fn refused_for_good_goes_to_the_dead_letter_queue() {
    let (_node, etcd) = etcd().await;
    let (url, mut rx) = broker(None, |n| [TOPIC_INVALID, PAYLOAD_INVALID][n.min(1)]).await;
    let lines = Lines::default();
    let b = start(mqtt_config(&etcd, &url, "mdl", Some("mdl-dead")), lines.logger());
    assert!(lines.wait_for("consuming /q/mdl/c/", Duration::from_secs(10)).await);
    let mut c = Client::connect([etcd.as_str()], None).await.unwrap();
    c.put("/q/mdl/p/k1", "v1", None).await.unwrap();
    c.put("/q/mdl/p/k2", "v2", None).await.unwrap();
    got(&mut rx).await;
    got(&mut rx).await;
    assert!(lines.wait_for("put on dead-letter queue", Duration::from_secs(5)).await);
    tokio::time::sleep(Duration::from_millis(300)).await;
    stop(b).await;
    assert!(drain(&etcd, "mdl", Duration::from_secs(2)).await.is_empty());
    let mut dead = drain(&etcd, "mdl-dead", Duration::from_secs(2)).await;
    dead.sort();
    assert_eq!(dead, vec![b"v1".to_vec(), b"v2".to_vec()]);
}

/// A refusal for now is sent again with its one id, and then lands.
#[tokio::test(flavor = "multi_thread")]
async fn refused_for_now_is_sent_again() {
    let (_node, etcd) = etcd().await;
    let (url, mut rx) = broker(None, |n| if n == 0 { QUOTA_EXCEEDED } else { SUCCESS }).await;
    let lines = Lines::default();
    let b = start(mqtt_config(&etcd, &url, "mnow", Some("mnow-dead")), lines.logger());
    assert!(lines.wait_for("consuming /q/mnow/c/", Duration::from_secs(10)).await);
    Client::connect([etcd.as_str()], None).await.unwrap().put("/q/mnow/p/k", "v", None).await.unwrap();
    let (a, again) = (got(&mut rx).await, got(&mut rx).await);
    assert_eq!(a.properties, again.properties, "the same id and properties");
    assert!(lines.any("retrying"), "it says so");
    tokio::time::sleep(Duration::from_millis(300)).await;
    stop(b).await;
    assert!(drain(&etcd, "mnow", Duration::from_secs(2)).await.is_empty());
    assert!(drain(&etcd, "mnow-dead", Duration::from_secs(2)).await.is_empty());
}

/// A broker that refuses the connection: said, tried again, the message held. A port that
/// does not parse is said the same way.
#[tokio::test(flavor = "multi_thread")]
async fn a_connection_refused_holds_the_message() {
    let (_node, etcd) = etcd().await;
    let (url, mut rx) = broker(Some(0x87), |_| SUCCESS).await; // not authorized
    let lines = Lines::default();
    let b = start(mqtt_config(&etcd, &url, "mno", None), lines.logger());
    assert!(lines.wait_for("consuming /q/mno/c/", Duration::from_secs(10)).await);
    Client::connect([etcd.as_str()], None).await.unwrap().put("/q/mno/p/k", "held", None).await.unwrap();
    assert!(lines.wait_for(&url, Duration::from_secs(5)).await, "the refusal is said");
    assert!(tokio::time::timeout(Duration::from_millis(500), rx.recv()).await.is_err(), "nothing published");
    stop(b).await;
    assert_eq!(drain(&etcd, "mno", Duration::from_secs(5)).await, vec![b"held".to_vec()]);
    // a port that does not parse is refused at the connect, and said
    let lines = Lines::default();
    let b = start(mqtt_config(&etcd, "mqtt://127.0.0.1:notaport", "mbad", None), lines.logger());
    assert!(lines.wait_for("consuming /q/mbad/c/", Duration::from_secs(10)).await);
    Client::connect([etcd.as_str()], None).await.unwrap().put("/q/mbad/p/k", "held", None).await.unwrap();
    assert!(lines.wait_for("notaport", Duration::from_secs(5)).await);
    stop(b).await;
}
