//! [bridge] The binary's bridge (feature `bridge`): the `bridge_*`
//! settings as flags and in the config file, a queue and a key prefix sent to a
//! broker, the AMQP listener, stopped with the node by SIGTERM and SIGINT, and the
//! settings it refuses (exit code 14). the bridge_* suites test the bridge itself.
#![cfg(unix)]

use std::process::{Child, Command, Stdio};
use std::time::Duration;

const BIN: &str = env!("CARGO_BIN_EXE_etcds");
const CLUSTER: &str = "11111111-1111-5111-8111-111111111111";

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn absent() -> String {
    std::env::temp_dir().join(format!("etcds-bridge-{}-absent.yml", uuid::Uuid::new_v4())).display().to_string()
}

/// The binary, its log (stdout) and errors read as it writes them - a full pipe
/// would stop it - and kept for the end.
struct Run {
    child: Child,
    out: Vec<std::thread::JoinHandle<String>>,
}

fn spawn(config: &str, port: u16, args: &[&str]) -> Run {
    let mut child = Command::new(BIN)
        .args(["--config", config, "--cluster", CLUSTER, "--listen-client-urls", &format!("127.0.0.1:{}", port)])
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the etcds binary starts");
    fn read(mut r: impl std::io::Read + Send + 'static) -> std::thread::JoinHandle<String> {
        std::thread::spawn(move || {
            let mut s = String::new();
            let _ = r.read_to_string(&mut s);
            s
        })
    }
    let out = vec![read(child.stdout.take().unwrap()), read(child.stderr.take().unwrap())];
    Run { child, out }
}

fn signal(run: &Run, sig: &str) {
    let ok = Command::new("kill").arg(format!("-{}", sig)).arg(run.child.id().to_string()).status().unwrap().success();
    assert!(ok, "kill -{} {}", sig, run.child.id());
}

/// The exit code and everything it wrote, once it exits on its own.
async fn exits(mut run: Run) -> (i32, String) {
    for _ in 0..800 {
        if let Some(status) = run.child.try_wait().unwrap() {
            let out = run.out.into_iter().map(|h| h.join().unwrap()).collect::<Vec<_>>().join("");
            return (status.code().expect("exited, not signalled"), out);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let _ = run.child.kill();
    panic!("etcds did not exit");
}

async fn connect(port: u16) -> etcd_client::Client {
    let url = format!("http://127.0.0.1:{}", port);
    for _ in 0..100 {
        if let Ok(mut c) = etcd_client::Client::connect([url.as_str()], None).await
            && c.status().await.is_ok()
        {
            return c;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("etcds on {} never answered", url);
}

/// What a fresh consumer of `queue` receives within `wait`.
async fn drain(c: &mut etcd_client::Client, queue: &str, wait: Duration) -> Vec<Vec<u8>> {
    let mut w = c.watch(format!("/q/{}/c/{}", queue, uuid::Uuid::new_v4()), None).await.unwrap();
    let mut got = vec![];
    let deadline = tokio::time::Instant::now() + wait;
    while let Ok(Ok(Some(resp))) = tokio::time::timeout_at(deadline, w.message()).await {
        for ev in resp.events() {
            if ev.event_type() == etcd_client::EventType::Put
                && let Some(kv) = ev.kv()
            {
                got.push(kv.value().to_vec());
                c.delete(kv.key(), None).await.unwrap();
            }
        }
    }
    got
}

#[cfg(feature = "bridge")]
mod with_the_feature {
    use super::*;
    use fe2o3_amqp::acceptor::{ConnectionAcceptor, LinkAcceptor, LinkEndpoint, SessionAcceptor};
    use fe2o3_amqp::sasl_profile::SaslProfile;
    use fe2o3_amqp::types::messaging::{Body, Message, Source};
    use fe2o3_amqp::types::primitives::Value;
    use fe2o3_amqp::{Connection, Receiver, Session};
    use tokio::sync::mpsc;

    type Got = Message<Body<Value>>;

    /// A broker that accepts all it gets, each message to the channel.
    async fn broker() -> (String, mpsc::UnboundedReceiver<Got>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("amqp://{}", listener.local_addr().unwrap());
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let tx = tx.clone();
                tokio::spawn(async move {
                    let Ok(mut conn) = ConnectionAcceptor::new("test-broker").accept(stream).await else { return };
                    let Ok(mut session) = SessionAcceptor::new().accept(&mut conn).await else { return };
                    let Ok(LinkEndpoint::Receiver(mut recv)) = LinkAcceptor::new().accept(&mut session).await else { return };
                    while let Ok(d) = recv.recv::<Body<Value>>().await {
                        let _ = tx.send(d.message().clone());
                        let _ = recv.accept(&d).await;
                    }
                });
            }
        });
        (url, rx)
    }

    async fn next(rx: &mut mpsc::UnboundedReceiver<Got>) -> Got {
        tokio::time::timeout(Duration::from_secs(20), rx.recv()).await.expect("the broker got nothing").unwrap()
    }

    fn text(b: &Body<Value>) -> String {
        match b {
            Body::Value(v) => match &v.0 {
                Value::String(s) => s.clone(),
                other => panic!("not a string: {:?}", other),
            },
            other => panic!("not an amqp-value: {:?}", other),
        }
    }

    /// A queue of the node to a broker, from flags; SIGTERM stops both, exit 0.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_queue_to_a_broker_stopped_by_sigterm() {
        let (amqp, mut rx) = broker().await;
        let port = free_port();
        let child = spawn(&absent(), port, &["--bridge-queue", "cliq", "--bridge-to", &amqp, "--bridge-address", "jms.cliq",
            "--bridge-cloudevents", "--bridge-ce-type", "my.event", "--log-level", "debug"]);
        let mut c = connect(port).await;
        c.put("/q/cliq/p/a", "from the node", None).await.unwrap();
        let m = next(&mut rx).await;
        assert_eq!(text(&m.body), "from the node");
        let app = &m.application_properties.as_ref().unwrap().0;
        assert_eq!(format!("{:?}", app.get("cloudEvents_type")), "Some(String(\"my.event\"))");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(drain(&mut c, "cliq", Duration::from_secs(2)).await.is_empty(), "acknowledged");
        signal(&child, "TERM");
        let (code, out) = exits(child).await;
        assert_eq!(code, 0, "SIGTERM is an orderly stop:\n{}", out);
        assert!(out.contains("bridge: queue cliq"), "{}", out);
        assert!(out.contains("acknowledged /q/cliq/"), "{}", out);
    }

    /// A key prefix from the config file; the flags' names are its keys.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_key_prefix_from_the_config_file() {
        let (amqp, mut rx) = broker().await;
        let port = free_port();
        let file = std::env::temp_dir().join(format!("etcds-bridge-{}.yml", uuid::Uuid::new_v4()));
        std::fs::write(&file, format!(
            "bridge_prefix: /svc/conf/\nbridge_to: {}\nbridge_body: bytes\nbridge_group_by_key: true\nbridge_buffer: 4\nbridge_send_timeout: 5\n", amqp)).unwrap();
        let child = spawn(&file.display().to_string(), port, &[]);
        let mut c = connect(port).await;
        // a prefix watch misses what was put before it began: put until it is seen
        let mut got = None;
        for _ in 0..40 {
            c.put("/svc/conf/limit", "10", None).await.unwrap();
            if let Ok(Some(m)) = tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
                got = Some(m);
                break;
            }
        }
        let m = got.expect("the broker got nothing in 20 s");
        match &m.body {
            Body::Data(d) => assert_eq!(d.iter().next().unwrap().0.as_ref(), b"10"),
            other => panic!("bridge_body bytes: {:?}", other),
        }
        assert_eq!(m.properties.as_ref().unwrap().group_id.as_deref(), Some("limit"));
        assert_eq!(c.get("/svc/conf/limit", None).await.unwrap().count(), 1, "a watched key stays");
        signal(&child, "TERM");
        let (code, out) = exits(child).await;
        let _ = std::fs::remove_file(&file);
        assert_eq!(code, 0, "{}", out);
        assert!(out.contains("prefix /svc/conf/") && out.contains("address svc.conf"), "the address from the prefix: {}", out);
    }

    /// The node as an AMQP peer for JMS clients; SIGINT stops both.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_listener_stopped_by_sigint() {
        let port = free_port();
        let bind = format!("127.0.0.1:{}", free_port());
        let child = spawn(&absent(), port, &["--bridge-listen", &bind, "--bridge-listen-user", "app", "--bridge-listen-password", "pw"]);
        let mut c = connect(port).await;
        c.put("/q/clil/p/x", "to a client", None).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await; // the listener's bind

        let url = format!("amqp://app:pw@{}", bind);
        let mut conn = Connection::builder().container_id("jms")
            .sasl_profile(SaslProfile::Plain { username: "app".into(), password: "pw".into() })
            .open(url.as_str()).await.unwrap();
        let mut session = Session::begin(&mut conn).await.unwrap();
        let mut r = Receiver::builder().name("c").source(Source::builder().address("clil").build()).attach(&mut session).await.unwrap();
        let d = tokio::time::timeout(Duration::from_secs(10), r.recv::<Body<Value>>()).await.unwrap().unwrap();
        assert_eq!(text(d.body()), "to a client");
        r.accept(&d).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;

        signal(&child, "INT");
        let (code, out) = exits(child).await;
        assert_eq!(code, 0, "SIGINT is an orderly stop:\n{}", out);
        assert!(out.contains("bridge: AMQP clients on"), "{}", out);
        drop(conn);
        // the node is gone with the bridge: its queue with it
    }

    /// Settings that make no bridge: refused before the node starts, exit 14.
    #[tokio::test]
    async fn what_it_refuses() {
        for args in [
            &["--bridge-queue", "a/b", "--bridge-to", "amqp://h:5672"][..], // Config::validate
            &["--bridge-queue", "q"][..],                                    // no broker
            &["--bridge-to", "amqp://h:5672"][..],                           // no queue
            &["--bridge-queue", "q", "--bridge-prefix", "/a/", "--bridge-to", "amqp://h"][..],
            &["--bridge-listen", "127.0.0.1:1", "--bridge-to", "amqp://h"][..],
            &["--bridge-queue", "q", "--bridge-to", "amqp://h", "--bridge-body", "blob"][..],
        ] {
            let (code, out) = exits(spawn(&absent(), free_port(), args)).await;
            assert_eq!(code, 14, "{:?}: {}", args, out);
            assert!(out.contains("ETCD-E104"), "{:?}: {}", args, out);
        }
    }
}

/// Without the feature, bridge settings are refused, not ignored: exit 14.
#[cfg(not(feature = "bridge"))]
#[tokio::test]
async fn without_the_feature_bridge_settings_are_refused() {
    let (code, out) = exits(spawn(&absent(), free_port(), &["--bridge-queue", "q", "--bridge-to", "amqp://h:5672"])).await;
    assert_eq!(code, 14, "{}", out);
    assert!(out.contains("ETCD-E105"), "{}", out);
}

/// No bridge settings, no bridge: the node as before.
#[tokio::test]
async fn no_bridge_settings_no_bridge() {
    let port = free_port();
    let child = spawn(&absent(), port, &[]);
    let mut c = connect(port).await;
    assert!(drain(&mut c, "none", Duration::from_millis(200)).await.is_empty());
    signal(&child, "TERM");
    let (code, out) = exits(child).await;
    assert_eq!(code, 0, "{}", out);
    assert!(!out.contains("bridge"), "{}", out);
}
