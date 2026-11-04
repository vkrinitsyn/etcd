//! [bridge-http] End to end, an HTTP(S) endpoint as the target: a small server of its own,
//! answering as a test says, and an etcds node in-process. Each test checks what reached
//! the endpoint, and whether the message was acknowledged in etcd.
#![cfg(feature = "bridge")]

mod bridge_common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bridge_common::*;
use etcd_client::Client;
use etcds::bridge::Config;
use etcds::bridge::mapping::Shape;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

/// A request as the endpoint saw it
#[derive(Debug, Clone)]
struct Req {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    /// which connection it came on, from 0
    conn: usize,
}

impl Req {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// How the endpoint answers the n-th request it gets (from 0).
#[derive(Clone, Copy, Debug)]
enum Reply {
    Status(u16),
    /// the status, then the connection closed with no `Connection: close` said
    Close(u16),
    /// a redirect to where
    Redirect(&'static str),
    /// the request read, nothing answered, ever
    Ignore,
    /// 200, with a body far over what the bridge reads
    Big,
    /// 200 and part of a body, whose end never comes
    Stall,
}

async fn answer<S: AsyncWrite + Unpin>(s: &mut S, reply: Reply) -> bool {
    let head = |code: u16, more: &str| format!("HTTP/1.1 {} X\r\n{}\r\n", code, more);
    let text = match reply {
        Reply::Status(204) => head(204, ""), // no body: one would break the connection
        Reply::Status(c) | Reply::Close(c) => head(c, "content-length: 2\r\n") + "no",
        Reply::Redirect(to) => head(302, &format!("location: {}\r\ncontent-length: 0\r\n", to)),
        Reply::Big => {
            let body = "x".repeat(300 * 1024);
            head(200, &format!("content-length: {}\r\n", body.len())) + &body
        }
        Reply::Stall => head(200, "content-length: 1000\r\n") + "part",
        Reply::Ignore => return true,
    };
    if s.write_all(text.as_bytes()).await.is_err() || s.flush().await.is_err() {
        return false;
    }
    !matches!(reply, Reply::Close(_))
}

/// One connection: requests read, and answered, until it ends
async fn serve<S>(stream: S, conn: usize, n: Arc<AtomicUsize>, policy: Arc<dyn Fn(usize, &Req) -> Reply + Send + Sync>, tx: mpsc::UnboundedSender<Req>)
where S: AsyncRead + AsyncWrite + Unpin {
    let mut s = BufReader::new(stream);
    loop {
        let mut line = String::new();
        if s.read_line(&mut line).await.unwrap_or(0) == 0 {
            return;
        }
        let mut parts = line.split_whitespace();
        let (method, target) = (parts.next().unwrap_or("").to_string(), parts.next().unwrap_or("").to_string());
        let mut headers = vec![];
        loop {
            let mut h = String::new();
            if s.read_line(&mut h).await.unwrap_or(0) == 0 {
                return;
            }
            let h = h.trim_end();
            if h.is_empty() {
                break;
            }
            let (k, v) = h.split_once(':').unwrap();
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
        let len: usize = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
            .map_or(0, |(_, v)| v.parse().unwrap());
        let mut body = vec![0; len];
        if s.read_exact(&mut body).await.is_err() {
            return;
        }
        let req = Req { method, target, headers, body, conn };
        let reply = policy(n.fetch_add(1, Ordering::SeqCst), &req);
        let _ = tx.send(req);
        if !answer(s.get_mut(), reply).await {
            return;
        }
    }
}

struct Endpoint {
    port: u16,
    rx: mpsc::UnboundedReceiver<Req>,
}

impl Endpoint {
    async fn next(&mut self) -> Req {
        tokio::time::timeout(Duration::from_secs(15), self.rx.recv()).await.expect("no request came").unwrap()
    }

    /// what comes within `wait`
    async fn none_for(&mut self, wait: Duration) -> Option<Req> {
        tokio::time::timeout(wait, self.rx.recv()).await.ok().flatten()
    }
}

async fn endpoint(policy: impl Fn(usize, &Req) -> Reply + Send + Sync + 'static) -> Endpoint {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::unbounded_channel();
    let (policy, n, c): (Arc<dyn Fn(usize, &Req) -> Reply + Send + Sync>, _, _) = (Arc::new(policy), Arc::new(AtomicUsize::new(0)), AtomicUsize::new(0));
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let conn = c.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(serve(stream, conn, n.clone(), policy.clone(), tx.clone()));
        }
    });
    Endpoint { port, rx }
}

fn http_config(etcd: &str, port: u16, path: &str, queue: &str, dead_letter: Option<&str>) -> Config {
    let mut c = config(etcd, &format!("http://127.0.0.1:{}{}", port, path), queue, dead_letter);
    c.send_timeout = Duration::from_secs(2);
    c
}

async fn producer(etcd: &str) -> Client {
    Client::connect([etcd], None).await.unwrap()
}

/// POSTed in order, on one connection kept alive, each acknowledged: the path and query
/// are the URL's, the value the body, its type what the value is, the properties headers.
#[tokio::test(flavor = "multi_thread")]
async fn posted_in_order_on_one_connection_and_acknowledged() {
    let (_node, etcd) = etcd().await;
    let mut ep = endpoint(|_, _| Reply::Status(204)).await;
    let lines = Lines::default();
    let b = start(http_config(&etcd, ep.port, "/hook?a=1&b=2", "order", None), lines.logger());
    assert!(lines.wait_for("consuming /q/order/c/", Duration::from_secs(10)).await);

    let mut c = producer(&etcd).await;
    c.put("/q/order/p/k1", r#"{"deploy":1}"#, None).await.unwrap();
    c.put("/q/order/p/k2", "plain text", None).await.unwrap();
    c.put("/q/order/p/k3", vec![0xff, 0xfe], None).await.unwrap();

    let (r1, r2, r3) = (ep.next().await, ep.next().await, ep.next().await);
    assert_eq!((r1.method.as_str(), r1.target.as_str()), ("POST", "/hook?a=1&b=2"));
    assert_eq!(r1.text(), r#"{"deploy":1}"#);
    assert_eq!(r1.header("content-type"), Some("application/json"));
    assert_eq!(r1.header("host"), Some(format!("127.0.0.1:{}", ep.port).as_str()));
    assert_eq!(r1.header("etcd-queue"), Some("order"));
    assert_eq!(r1.header("etcd-key"), Some("k1"));
    assert!(r1.header("etcd-delivery-key").is_some() && r1.header("message-id").is_some());
    assert!(r1.header("user-agent").unwrap().starts_with("etcds/"));
    assert!(r1.header("authorization").is_none());
    assert_eq!(r1.header("content-length"), Some("12"));
    assert_eq!((r2.text().as_str(), r2.header("content-type")), ("plain text", Some("text/plain; charset=utf-8")));
    assert_eq!((r3.body.as_slice(), r3.header("content-type")), (&[0xff, 0xfe][..], Some("application/octet-stream")));
    assert_eq!((r1.conn, r2.conn, r3.conn), (0, 0, 0), "kept alive");
    tokio::time::sleep(Duration::from_millis(300)).await;
    stop(b).await;
    assert!(drain(&etcd, "order", Duration::from_secs(2)).await.is_empty(), "all acknowledged");
}

/// What the message itself will never do: on the dead-letter queue, then acknowledged.
#[tokio::test(flavor = "multi_thread")]
async fn refused_for_good_goes_to_the_dead_letter_queue() {
    let (_node, etcd) = etcd().await;
    let mut ep = endpoint(|n, _| Reply::Status([400, 413, 415, 422, 200][n.min(4)])).await;
    let lines = Lines::default();
    let b = start(http_config(&etcd, ep.port, "/", "dl", Some("dl-dead")), lines.logger());
    assert!(lines.wait_for("consuming /q/dl/c/", Duration::from_secs(10)).await);
    let mut c = producer(&etcd).await;
    for i in 0..5 {
        c.put(format!("/q/dl/p/k{}", i), format!("v{}", i), None).await.unwrap();
    }
    for i in 0..5 {
        assert_eq!(ep.next().await.text(), format!("v{}", i), "in order, nothing sent twice");
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    stop(b).await;
    assert!(drain(&etcd, "dl", Duration::from_secs(2)).await.is_empty());
    let mut dead = drain(&etcd, "dl-dead", Duration::from_secs(2)).await;
    dead.sort();
    assert_eq!(dead, vec![b"v0".to_vec(), b"v1".to_vec(), b"v2".to_vec(), b"v3".to_vec()]);
}

/// Without a dead-letter queue nothing is dropped: it is retried, and holds the line.
#[tokio::test(flavor = "multi_thread")]
async fn refused_with_no_dead_letter_queue_is_kept() {
    let (_node, etcd) = etcd().await;
    let mut ep = endpoint(|_, _| Reply::Status(400)).await;
    let lines = Lines::default();
    let b = start(http_config(&etcd, ep.port, "/", "keep", None), lines.logger());
    assert!(lines.wait_for("consuming /q/keep/c/", Duration::from_secs(10)).await);
    producer(&etcd).await.put("/q/keep/p/k", "held", None).await.unwrap();
    let (a, again) = (ep.next().await, ep.next().await);
    assert_eq!(a.header("message-id"), again.header("message-id"), "one delivery, one id");
    stop(b).await;
    assert_eq!(drain(&etcd, "keep", Duration::from_secs(5)).await, vec![b"held".to_vec()]);
}

/// An expired token, a hook being deployed, a limit, a server error: fixed there, and the
/// message lands, with its one id. Never dead-lettered, though a queue for it is set.
#[tokio::test(flavor = "multi_thread")]
async fn a_refusal_for_now_is_retried_never_dead_lettered() {
    let (_node, etcd) = etcd().await;
    // each wait doubles from 1 s: the rest of the statuses are in http.rs's unit test
    let mut ep = endpoint(|n, _| match [401, 404, 500].get(n) {
        Some(c) => Reply::Status(*c),
        None => Reply::Status(200),
    }).await;
    let lines = Lines::default();
    let mut cfg = http_config(&etcd, ep.port, "/", "now", Some("now-dead"));
    cfg.send_timeout = Duration::from_secs(5);
    let b = start(cfg, lines.logger());
    assert!(lines.wait_for("consuming /q/now/c/", Duration::from_secs(10)).await);
    producer(&etcd).await.put("/q/now/p/k", "v", None).await.unwrap();
    let first = ep.next().await;
    for _ in 0..2 {
        assert_eq!(ep.next().await.header("message-id"), first.header("message-id"));
    }
    let landed = ep.next().await;
    assert_eq!(landed.header("message-id"), first.header("message-id"));
    tokio::time::sleep(Duration::from_millis(300)).await;
    stop(b).await;
    assert!(lines.any("HTTP 401"), "it says why");
    assert!(drain(&etcd, "now", Duration::from_secs(2)).await.is_empty());
    assert!(drain(&etcd, "now-dead", Duration::from_secs(2)).await.is_empty());
}

/// A redirect is not followed: the POST and its login stay here, and it is retried.
#[tokio::test(flavor = "multi_thread")]
async fn a_redirect_is_not_followed() {
    let (_node, etcd) = etcd().await;
    let mut other = endpoint(|_, _| Reply::Status(200)).await;
    let to: &'static str = Box::leak(format!("http://127.0.0.1:{}/stolen", other.port).into_boxed_str());
    let mut ep = endpoint(move |n, _| if n == 0 { Reply::Redirect(to) } else { Reply::Status(200) }).await;
    let lines = Lines::default();
    let mut cfg = http_config(&etcd, ep.port, "/", "redir", None);
    cfg.target_url = format!("http://bearer:s3cret@127.0.0.1:{}/", ep.port);
    let b = start(cfg, lines.logger());
    assert!(lines.wait_for("consuming /q/redir/c/", Duration::from_secs(10)).await);
    producer(&etcd).await.put("/q/redir/p/k", "v", None).await.unwrap();
    assert_eq!(ep.next().await.header("authorization"), Some("Bearer s3cret"));
    ep.next().await;
    assert!(lines.wait_for("a redirect is not followed", Duration::from_secs(5)).await);
    assert!(lines.any(&format!("sending to http://bearer:***@127.0.0.1:{}", ep.port)), "no queue name for an address");
    assert!(other.none_for(Duration::from_millis(300)).await.is_none(), "nothing went there");
    stop(b).await;
    assert!(!lines.any("s3cret"), "the token in no log line");
}

/// No answer within the send timeout: sent again on a new connection, with the same id.
#[tokio::test(flavor = "multi_thread")]
async fn no_answer_is_sent_again_with_the_same_id() {
    let (_node, etcd) = etcd().await;
    let mut ep = endpoint(|n, _| if n == 0 { Reply::Ignore } else { Reply::Status(200) }).await;
    let lines = Lines::default();
    let mut cfg = http_config(&etcd, ep.port, "/", "silent", None);
    cfg.send_timeout = Duration::from_secs(1);
    let b = start(cfg, lines.logger());
    assert!(lines.wait_for("consuming /q/silent/c/", Duration::from_secs(10)).await);
    producer(&etcd).await.put("/q/silent/p/k", "v", None).await.unwrap();
    let (a, again) = (ep.next().await, ep.next().await);
    assert_eq!(a.header("message-id"), again.header("message-id"));
    assert_ne!(a.conn, again.conn, "a new connection");
    tokio::time::sleep(Duration::from_millis(300)).await;
    stop(b).await;
    assert!(drain(&etcd, "silent", Duration::from_secs(2)).await.is_empty());
}

/// A connection the endpoint closed is made again before the next send: no error said, no
/// wait, and nothing sent twice.
#[tokio::test(flavor = "multi_thread")]
async fn a_connection_the_endpoint_closed_costs_no_retry() {
    let (_node, etcd) = etcd().await;
    let mut ep = endpoint(|_, _| Reply::Close(200)).await;
    let lines = Lines::default();
    let b = start(http_config(&etcd, ep.port, "/", "closed", None), lines.logger());
    assert!(lines.wait_for("consuming /q/closed/c/", Duration::from_secs(10)).await);
    let mut c = producer(&etcd).await;
    c.put("/q/closed/p/k1", "v1", None).await.unwrap();
    assert_eq!(ep.next().await.text(), "v1");
    tokio::time::sleep(Duration::from_millis(200)).await; // the close seen
    let at = Instant::now();
    c.put("/q/closed/p/k2", "v2", None).await.unwrap();
    let r = ep.next().await;
    assert_eq!(r.text(), "v2");
    assert!(at.elapsed() < Duration::from_millis(900), "no backoff: {:?}", at.elapsed());
    assert_eq!(r.conn, 1);
    assert!(ep.none_for(Duration::from_millis(300)).await.is_none(), "nothing sent twice");
    stop(b).await;
    assert!(!lines.any("send "), "no failure said");
    assert!(drain(&etcd, "closed", Duration::from_secs(2)).await.is_empty());
}

/// The endpoint down at the start and in the middle: said, retried, and nothing lost.
#[tokio::test(flavor = "multi_thread")]
async fn an_endpoint_away_holds_the_message() {
    let (_node, etcd) = etcd().await;
    let port = free_port();
    let lines = Lines::default();
    let b = start(http_config(&etcd, port, "/x/s3cret", "away", None), lines.logger());
    assert!(lines.wait_for("consuming /q/away/c/", Duration::from_secs(10)).await);
    producer(&etcd).await.put("/q/away/p/k", "late", None).await.unwrap();
    assert!(lines.wait_for("refused", Duration::from_secs(5)).await || lines.wait_for("Connection", Duration::from_secs(1)).await);

    // comes up on that port
    let listener = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let n = Arc::new(AtomicUsize::new(0));
        let policy: Arc<dyn Fn(usize, &Req) -> Reply + Send + Sync> = Arc::new(|_, _| Reply::Status(200));
        let mut conn = 0;
        while let Ok((s, _)) = listener.accept().await {
            tokio::spawn(serve(s, conn, n.clone(), policy.clone(), tx.clone()));
            conn += 1;
        }
    });
    let r = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.unwrap().unwrap();
    assert_eq!(r.text(), "late");
    tokio::time::sleep(Duration::from_millis(300)).await;
    stop(b).await;
    assert!(!lines.any("s3cret"), "the path in no log line");
    assert!(drain(&etcd, "away", Duration::from_secs(2)).await.is_empty());
}

/// A 2xx is the endpoint's decision: a body too long, or one that never ends, drops the
/// connection, and is no reason to send the message again.
#[tokio::test(flavor = "multi_thread")]
async fn an_answer_is_taken_whatever_its_body_does() {
    let (_node, etcd) = etcd().await;
    let mut ep = endpoint(|n, _| match n { 0 => Reply::Big, 1 => Reply::Stall, _ => Reply::Status(200) }).await;
    let lines = Lines::default();
    let mut cfg = http_config(&etcd, ep.port, "/", "bodies", None);
    cfg.send_timeout = Duration::from_secs(5);
    let b = start(cfg, lines.logger());
    assert!(lines.wait_for("consuming /q/bodies/c/", Duration::from_secs(10)).await);
    let mut c = producer(&etcd).await;
    for i in 0..3 {
        c.put(format!("/q/bodies/p/k{}", i), format!("v{}", i), None).await.unwrap();
    }
    let got = [ep.next().await, ep.next().await, ep.next().await];
    assert_eq!(got.iter().map(Req::text).collect::<Vec<_>>(), ["v0", "v1", "v2"], "each once");
    assert_eq!((got[0].conn, got[1].conn, got[2].conn), (0, 1, 2), "each of those two dropped its connection");
    tokio::time::sleep(Duration::from_millis(300)).await;
    stop(b).await;
    assert!(drain(&etcd, "bodies", Duration::from_secs(2)).await.is_empty());
}

/// A send timeout shorter than the body's usual wait: the answer counts, and the message is
/// not sent again because its body stalled.
#[tokio::test(flavor = "multi_thread")]
async fn a_stalled_body_within_a_short_send_timeout_is_not_sent_again() {
    let (_node, etcd) = etcd().await;
    let mut ep = endpoint(|n, _| if n == 0 { Reply::Stall } else { Reply::Status(200) }).await;
    let lines = Lines::default();
    let mut cfg = http_config(&etcd, ep.port, "/", "short", None);
    cfg.send_timeout = Duration::from_secs(1);
    let b = start(cfg, lines.logger());
    assert!(lines.wait_for("consuming /q/short/c/", Duration::from_secs(10)).await);
    producer(&etcd).await.put("/q/short/p/k", "once", None).await.unwrap();
    assert_eq!(ep.next().await.text(), "once");
    assert!(ep.none_for(Duration::from_secs(3)).await.is_none(), "not sent again");
    stop(b).await;
    assert!(drain(&etcd, "short", Duration::from_secs(2)).await.is_empty());
}

/// Basic auth from the URL, percent-decoded; a CloudEvent in the HTTP binding's binary
/// mode; a key no header could hold.
#[tokio::test(flavor = "multi_thread")]
async fn a_login_and_a_cloudevent() {
    let (_node, etcd) = etcd().await;
    let mut ep = endpoint(|_, _| Reply::Status(200)).await;
    let lines = Lines::default();
    let mut cfg = http_config(&etcd, ep.port, "/", "ce", None);
    cfg.target_url = format!("http://app:p%40ss@127.0.0.1:{}/h", ep.port);
    cfg.shape = Shape { cloudevents: true, ce_type: "yt.roll.done".to_string(), ..Shape::default() };
    let b = start(cfg, lines.logger());
    assert!(lines.wait_for("consuming /q/ce/c/", Duration::from_secs(10)).await);
    producer(&etcd).await.put("/q/ce/p/a b\r\nevil: 1", r#"{"ok":true}"#, None).await.unwrap();
    let r = ep.next().await;
    assert_eq!(r.header("authorization"), Some("Basic YXBwOnBAc3M="), "app:p@ss");
    assert_eq!(r.header("ce-type"), Some("yt.roll.done"));
    assert_eq!(r.header("ce-specversion"), Some("1.0"));
    assert_eq!(r.header("ce-source"), Some("/q/ce"));
    assert_eq!(r.header("ce-subject"), Some("a%20b%0D%0Aevil:%201"));
    assert_eq!(r.header("content-type"), Some("application/json"));
    assert!(r.header("evil").is_none());
    stop(b).await;
    assert!(!lines.any("p%40ss") && !lines.any("p@ss"), "the password in no log line");
}

/// A watched prefix: a put and a delete, marked; the keys stay.
#[tokio::test(flavor = "multi_thread")]
async fn a_key_prefix_sends_puts_and_deletes() {
    let (_node, etcd) = etcd().await;
    let mut ep = endpoint(|_, _| Reply::Status(200)).await;
    let lines = Lines::default();
    let mut cfg = http_config(&etcd, ep.port, "/", "unused", None);
    cfg.queue = String::new();
    cfg.prefix = Some("/config/".to_string());
    let b = start(cfg, lines.logger());
    assert!(lines.wait_for("watching /config/", Duration::from_secs(10)).await);
    let mut c = producer(&etcd).await;
    c.put("/config/a", "1", None).await.unwrap();
    c.delete("/config/a", None).await.unwrap();
    let (put, del) = (ep.next().await, ep.next().await);
    assert_eq!((put.text().as_str(), put.header("etcd-event"), put.header("etcd-key")), ("1", Some("put"), Some("a")));
    assert_eq!(put.header("etcd-prefix"), Some("/config/"));
    assert_eq!((del.body.len(), del.header("etcd-event"), del.header("content-type")), (0, Some("delete"), None));
    assert_eq!(del.header("content-length"), Some("0"));
    stop(b).await;
}

/// Run on the node in this process, as ytserv-all does.
#[tokio::test(flavor = "multi_thread")]
async fn in_process_acknowledged() {
    let (node, etcd) = etcd().await;
    let mut ep = endpoint(|_, _| Reply::Status(200)).await;
    let lines = Lines::default();
    let cfg = Config { etcd: vec![], node: Some(node.clone()), ..http_config("", ep.port, "/", "local", None) };
    let b = start(cfg, lines.logger());
    assert!(lines.wait_for("consuming /q/local/c/", Duration::from_secs(10)).await);
    producer(&etcd).await.put("/q/local/p/k", "v", None).await.unwrap();
    assert_eq!(ep.next().await.text(), "v");
    tokio::time::sleep(Duration::from_millis(300)).await;
    stop(b).await;
    assert!(drain(&etcd, "local", Duration::from_secs(2)).await.is_empty());
}

/// A CA and a server certificate for `localhost` and 127.0.0.1, made with the openssl
/// tool: None without it, and the test says so and passes
fn certs(dir: &std::path::Path, name: &str) -> Option<()> {
    let run = |args: &[&str]| std::process::Command::new("openssl").args(args).current_dir(dir)
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().map(|s| s.success()).unwrap_or(false);
    std::fs::write(dir.join("san.cnf"), "subjectAltName=DNS:localhost,IP:127.0.0.1\n").ok()?;
    let ca = format!("{}-ca", name);
    let ok = run(&["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", &format!("{}.key", ca), "-out", &format!("{}.pem", ca), "-days", "2", "-subj", "/CN=test ca"])
        && run(&["req", "-newkey", "rsa:2048", "-nodes", "-keyout", &format!("{}.key", name), "-out", &format!("{}.csr", name), "-subj", "/CN=localhost"])
        && run(&["x509", "-req", "-in", &format!("{}.csr", name), "-CA", &format!("{}.pem", ca), "-CAkey", &format!("{}.key", ca), "-CAcreateserial",
            "-out", &format!("{}.pem", name), "-days", "2", "-extfile", "san.cnf"]);
    ok.then_some(())
}

async fn tls_endpoint(dir: &std::path::Path, name: &str, policy: impl Fn(usize, &Req) -> Reply + Send + Sync + 'static) -> Endpoint {
    use rustls_pki_types::pem::PemObject;
    use rustls_pki_types::{CertificateDer, PrivateKeyDer};
    let chain: Vec<_> = CertificateDer::pem_file_iter(dir.join(format!("{}.pem", name))).unwrap().collect::<Result<_, _>>().unwrap();
    let key = PrivateKeyDer::from_pem_file(dir.join(format!("{}.key", name))).unwrap();
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider).with_safe_default_protocol_versions().unwrap()
        .with_no_client_auth().with_single_cert(chain, key).unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::unbounded_channel();
    let (policy, n, c): (Arc<dyn Fn(usize, &Req) -> Reply + Send + Sync>, _, _) = (Arc::new(policy), Arc::new(AtomicUsize::new(0)), AtomicUsize::new(0));
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let conn = c.fetch_add(1, Ordering::SeqCst);
            let (acceptor, n, policy, tx) = (acceptor.clone(), n.clone(), policy.clone(), tx.clone());
            tokio::spawn(async move {
                if let Ok(s) = acceptor.accept(stream).await {
                    serve(s, conn, n, policy, tx).await;
                }
            });
        }
    });
    Endpoint { port, rx }
}

/// `https://` with `?ca=`: the name and the chain checked, an IP and a host name alike, the
/// login sent, and `ca` not in the request. Another CA, or the public roots: nothing sent,
/// the message held.
#[tokio::test(flavor = "multi_thread")]
async fn over_tls_with_a_private_ca() {
    let dir = std::env::temp_dir().join(format!("etcds-http-tls-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    if certs(&dir, "a").is_none() || certs(&dir, "b").is_none() {
        eprintln!("no openssl tool: not run");
        return;
    }
    let (_node, etcd) = etcd().await;
    let mut ep = tls_endpoint(&dir, "a", |_, _| Reply::Status(200)).await;
    let ca = dir.join("a-ca.pem").display().to_string();

    for (host, queue) in [("localhost", "tls-name"), ("127.0.0.1", "tls-ip")] {
        let lines = Lines::default();
        let mut cfg = http_config(&etcd, ep.port, "/", queue, None);
        cfg.target_url = format!("https://bearer:s3cret@{}:{}/hook?x=1&ca={}", host, ep.port, ca);
        let b = start(cfg, lines.logger());
        assert!(lines.wait_for(&format!("consuming /q/{}/c/", queue), Duration::from_secs(10)).await);
        producer(&etcd).await.put(format!("/q/{}/p/k", queue), "secure", None).await.unwrap();
        let r = ep.next().await;
        assert_eq!((r.text().as_str(), r.target.as_str()), ("secure", "/hook?x=1"), "{}", host);
        assert_eq!(r.header("authorization"), Some("Bearer s3cret"));
        tokio::time::sleep(Duration::from_millis(300)).await;
        stop(b).await;
        assert!(!lines.any("s3cret") && !lines.any(&ca), "no secret, no path in the log");
        assert!(drain(&etcd, queue, Duration::from_secs(2)).await.is_empty());
    }

    // trusting another CA, and the public roots alone
    let other = dir.join("b-ca.pem").display().to_string();
    for (queue, url) in [
        ("tls-other", format!("https://localhost:{}/?ca={}", ep.port, other)),
        ("tls-public", format!("https://localhost:{}/", ep.port)),
    ] {
        let lines = Lines::default();
        let mut cfg = http_config(&etcd, ep.port, "/", queue, None);
        cfg.target_url = url;
        let b = start(cfg, lines.logger());
        assert!(lines.wait_for(&format!("consuming /q/{}/c/", queue), Duration::from_secs(10)).await);
        producer(&etcd).await.put(format!("/q/{}/p/k", queue), "held", None).await.unwrap();
        assert!(lines.wait_for("TLS:", Duration::from_secs(5)).await, "the refusal is said");
        assert!(ep.none_for(Duration::from_millis(300)).await.is_none(), "nothing sent");
        stop(b).await;
        assert_eq!(drain(&etcd, queue, Duration::from_secs(5)).await, vec![b"held".to_vec()]);
    }
    let _ = std::fs::remove_dir_all(&dir);
}
