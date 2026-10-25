//! Built-in ClickHouse consumer for queues named `/q/ch:<table>/...` or
//! `/queue/clickhouse:<table>/...`.
//!
//! A message put on such a queue carries a JSON object as its value, and the
//! queue writes it straight into that ClickHouse table as `JSONEachRow` - no
//! watcher, no consumer process, no database round trip in between. The whole
//! batch waiting at flush time goes out in one request, which is what
//! JSONEachRow is for.
//!
//! hyper's low-level client is used directly: tonic already pulls it in, so this
//! adds no new crates.

use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::Request;
use slog::Logger;
use tokio::net::TcpStream;

/// giving up on one insert; a queue must not block forever on a sick server
const CH_TIMEOUT: Duration = Duration::from_secs(30);

/// `ch:` / `clickhouse:` marks a queue whose messages go straight to ClickHouse.
pub(crate) const CH_PREFIX: &str = "ch:";
pub(crate) const CLICKHOUSE_PREFIX: &str = "clickhouse:";

/// The ClickHouse table a queue name targets, if any.
///
/// `ch:events` -> `events` in the configured database, `ch:analytics.events` ->
/// `analytics`.`events`. Returns `None` for an ordinary queue.
pub(crate) fn queue_target(queue_name: &str) -> Option<&str> {
    queue_name.strip_prefix(CH_PREFIX)
        .or_else(|| queue_name.strip_prefix(CLICKHOUSE_PREFIX))
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
}

/// Where the built-in consumer writes. Built from `clickhouse_url` in the config.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ChSink {
    /// `host:port`, scheme and userinfo stripped
    authority: String,
    /// `Basic ...` when the URL carried userinfo
    auth: Option<String>,
    /// database used when the queue name does not carry one
    db: String,
}

impl ChSink {
    /// `None` when no endpoint is configured, so a `ch:` queue simply behaves
    /// like an ordinary one rather than silently eating messages.
    pub(crate) fn new(url: &str, db: &str) -> Option<ChSink> {
        let url = url.trim();
        if url.is_empty() {
            return None;
        }
        // scheme://[user[:pwd]@]host[:port][/...]
        let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
        let rest = rest.split(['/', '?']).next().unwrap_or(rest);
        let (userinfo, hostport) = match rest.rsplit_once('@') {
            Some((u, h)) => (Some(u), h),
            None => (None, rest),
        };
        if hostport.is_empty() {
            return None;
        }
        let authority = if hostport.contains(':') {
            hostport.to_string()
        } else {
            format!("{}:8123", hostport) // ClickHouse HTTP default
        };
        let auth = userinfo.map(|u| format!("Basic {}", b64(u.as_bytes())));
        let db = match db.trim() {
            "" => "default".to_string(),
            d => d.to_string(),
        };
        Some(ChSink { authority, auth, db })
    }

    /// `` `db`.`table` `` for a queue target, honouring an explicit `db.table`.
    fn target(&self, table: &str) -> String {
        match table.split_once('.') {
            Some((db, t)) => format!("{}.{}", quote_ident(db), quote_ident(t)),
            None => format!("{}.{}", quote_ident(&self.db), quote_ident(table)),
        }
    }

    /// Insert `rows` (one JSON object per entry) into `table`.
    pub(crate) async fn insert(&self, table: &str, rows: &[String], log: &Logger) -> Result<(), String> {
        if rows.is_empty() {
            return Ok(());
        }
        let target = self.target(table);
        // origin-form request target: an absolute URI is absolute-form, reserved
        // for proxies, and ClickHouse answers it with "there is no handle http://..."
        let uri = format!("/?query={}",
            urlencode(&format!("INSERT INTO {} FORMAT JSONEachRow", target)));

        let stream = TcpStream::connect(&self.authority).await
            .map_err(|e| tr!("ch.connect", host = self.authority, err = e))?;
        let (mut sender, conn) = hyper::client::conn::http1::handshake(
            hyper_util::rt::TokioIo::new(stream)).await
            .map_err(|e| tr!("ch.handshake", err = e))?;
        let log_c = log.clone();
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                slog::trace!(log_c, "[ch] {}", tr!("ch.closed", err = e));
            }
        });

        let mut req = Request::post(&uri).header("Host", self.authority.as_str());
        if let Some(a) = &self.auth {
            req = req.header("Authorization", a.as_str());
        }
        let req = req.body(Full::new(Bytes::from(rows.join("\n"))))
            .map_err(|e| tr!("ch.request", err = e))?;

        let resp = tokio::time::timeout(CH_TIMEOUT, sender.send_request(req)).await
            .map_err(|_| tr!("ch.timeout", timeout = format!("{:?}", CH_TIMEOUT)))?
            .map_err(|e| tr!("ch.send", err = e))?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        // ClickHouse puts the reason in the body, not the status line
        let msg = resp.into_body().collect().await
            .map(|b| String::from_utf8_lossy(&b.to_bytes()).trim().to_string())
            .unwrap_or_default();
        Err(tr!("ch.refused", status = status, err = msg))
    }
}

/// ClickHouse quotes identifiers with backticks; an embedded one is doubled.
fn quote_ident(id: &str) -> String {
    format!("`{}`", id.replace('`', "``"))
}

/// Percent-encode a query-string value.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' =>
                out.push(b as char),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// Minimal base64 for the Basic auth header - not worth a dependency.
fn b64(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((input.len() + 2) / 3 * 4);
    for c in input.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(if c.len() > 1 { T[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if c.len() > 2 { T[(n & 63) as usize] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_name_selects_a_clickhouse_table() {
        assert_eq!(queue_target("ch:events"), Some("events"));
        assert_eq!(queue_target("clickhouse:events"), Some("events"));
        assert_eq!(queue_target("ch:analytics.events"), Some("analytics.events"));
        // an ordinary queue must stay ordinary
        assert_eq!(queue_target("analytics"), None);
        assert_eq!(queue_target("ch:"), None);
        assert_eq!(queue_target(""), None);
    }

    #[test]
    fn target_uses_the_configured_db_unless_the_name_carries_one() {
        let s = ChSink::new("http://h:8123", "").unwrap();
        assert_eq!(s.db, "default"); // empty config falls back
        assert_eq!(s.target("events"), "`default`.`events`");
        let s = ChSink::new("http://h:8123", "analytics").unwrap();
        assert_eq!(s.target("events"), "`analytics`.`events`");
        // an explicit db.table in the queue name wins
        assert_eq!(s.target("other.events"), "`other`.`events`");
        assert_eq!(quote_ident("a`b"), "`a``b`");
    }

    #[test]
    fn url_forms_and_basic_auth() {
        assert_eq!(ChSink::new("http://127.0.0.1:8123", "").unwrap().authority, "127.0.0.1:8123");
        assert_eq!(ChSink::new("ch.lan", "").unwrap().authority, "ch.lan:8123");
        assert_eq!(ChSink::new("", ""), None);
        let s = ChSink::new("http://user:pw@ch.lan:8123", "").unwrap();
        assert_eq!(s.authority, "ch.lan:8123");
        assert_eq!(s.auth.as_deref(), Some("Basic dXNlcjpwdw=="));
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(b64(b"user:pw"), "dXNlcjpwdw==");
        assert_eq!(b64(b""), "");
        assert_eq!(b64(b"a"), "YQ==");
        assert_eq!(b64(b"ab"), "YWI=");
        assert_eq!(b64(b"abc"), "YWJj");
    }

    #[test]
    fn a_url_without_a_host_is_no_endpoint() {
        assert_eq!(ChSink::new("http://", ""), None);
        assert_eq!(ChSink::new("http://user:pw@", ""), None);
        // a path and a query are ignored: only host and port are used
        assert_eq!(ChSink::new("http://h:9000/some/path?x=1", "").unwrap().authority, "h:9000");
    }

    #[test]
    fn the_insert_statement_is_percent_encoded() {
        assert_eq!(urlencode("INSERT INTO `d`.`t`"), "INSERT%20INTO%20%60d%60.%60t%60");
        assert_eq!(urlencode("a-b_c.d~e"), "a-b_c.d~e", "unreserved characters stay");
    }

    fn log() -> Logger {
        Logger::root(slog::Discard, slog::o!())
    }

    /// One request as the fake ClickHouse saw it.
    #[derive(Debug, Clone)]
    pub(crate) struct Seen {
        pub(crate) head: String,
        pub(crate) body: String,
    }

    /// A ClickHouse HTTP interface stand-in on a port of its own: it records each
    /// request and answers with the next scripted `(status, body)`, then 200.
    pub(crate) struct FakeCh {
        pub(crate) url: String,
        pub(crate) seen: tokio::sync::mpsc::Receiver<Seen>,
    }

    pub(crate) async fn fake_clickhouse(script: Vec<(u16, &'static str)>) -> FakeCh {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = tokio::sync::mpsc::channel(100);
        tokio::spawn(async move {
            let mut script = std::collections::VecDeque::from(script);
            loop {
                let Ok((mut s, _)) = listener.accept().await else { return };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                // headers, then as much body as Content-Length says
                let (head, body) = loop {
                    let n = s.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 { break (String::new(), String::new()); }
                    buf.extend_from_slice(&chunk[..n]);
                    let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
                    let head = String::from_utf8_lossy(&buf[..end]).to_string();
                    let len = head.lines()
                        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                        .unwrap_or(0);
                    while buf.len() < end + 4 + len {
                        let n = s.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 { break; }
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    break (head, String::from_utf8_lossy(&buf[end + 4..]).to_string());
                };
                let (status, msg) = script.pop_front().unwrap_or((200, ""));
                let _ = tx.send(Seen { head, body }).await;
                let resp = format!("HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    status, msg.len(), msg);
                let _ = s.write_all(resp.as_bytes()).await;
                let _ = s.shutdown().await;
            }
        });
        FakeCh { url, seen: rx }
    }

    /// One batch is one POST of `INSERT INTO db.table FORMAT JSONEachRow`, the
    /// rows newline separated, with the Host and Basic auth headers.
    #[tokio::test]
    async fn an_insert_posts_the_rows_as_json_each_row() {
        let mut ch = fake_clickhouse(vec![]).await;
        let url = ch.url.replace("http://", "http://user:pw@");
        let sink = ChSink::new(&url, "analytics").unwrap();
        let rows = vec![r#"{"id":1}"#.to_string(), r#"{"id":2}"#.to_string()];
        sink.insert("events", &rows, &log()).await.expect("a 200 is a landed batch");

        let seen = ch.seen.recv().await.unwrap();
        let first = seen.head.lines().next().unwrap();
        assert_eq!(first,
            "POST /?query=INSERT%20INTO%20%60analytics%60.%60events%60%20FORMAT%20JSONEachRow HTTP/1.1");
        let head = seen.head.to_ascii_lowercase();
        assert!(head.contains(&format!("host: {}", sink.authority)), "{}", seen.head);
        assert!(head.contains("authorization: basic dxnlcjpwdw=="), "{}", seen.head);
        assert_eq!(seen.body, "{\"id\":1}\n{\"id\":2}");
    }

    /// No rows, no request.
    #[tokio::test]
    async fn an_empty_batch_sends_nothing() {
        let mut ch = fake_clickhouse(vec![]).await;
        let sink = ChSink::new(&ch.url, "").unwrap();
        sink.insert("events", &[], &log()).await.unwrap();
        assert!(ch.seen.try_recv().is_err(), "nothing reached ClickHouse");
    }

    /// A refusal carries ClickHouse's own reason, which it puts in the body.
    #[tokio::test]
    async fn a_refused_batch_reports_status_and_reason() {
        let ch = fake_clickhouse(vec![(400, "Code: 27. Cannot parse input\n")]).await;
        let sink = ChSink::new(&ch.url, "").unwrap();
        let err = sink.insert("events", &["not json".to_string()], &log()).await.unwrap_err();
        assert!(err.starts_with("ETCD-E075 ") && err.contains(" 400 "), "{}", err);
        assert!(err.ends_with("Code: 27. Cannot parse input"), "the body, trimmed: {}", err);
    }

    /// Nothing listening is an error, not a hang.
    #[tokio::test]
    async fn a_closed_port_is_a_connect_error() {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let sink = ChSink::new(&format!("127.0.0.1:{}", port), "").unwrap();
        let err = sink.insert("events", &["{}".to_string()], &log()).await.unwrap_err();
        assert!(err.starts_with("ETCD-E070 ") && err.contains("127.0.0.1:"), "{}", err);
    }
}

#[cfg(test)]
mod queue_tests {
    //! A `ch:` queue end to end, in-process: produced with a put, inserted by
    //! the queue's own dispatcher (clickhouse.md "How a batch goes out").
    use super::tests::fake_clickhouse;
    use crate::cluster::EtcdNode;
    use crate::etcdpb::etcdserverpb::kv_server::Kv;
    use crate::etcdpb::etcdserverpb::RangeRequest;
    use std::time::Duration;

    async fn node(clickhouse_url: &str) -> EtcdNode {
        let mut cfg = crate::cli::EtcdConfig::with_defaults();
        cfg.cluster = "11111111-1111-5111-8111-111111111111".to_string();
        cfg.listen_client_urls = "127.0.0.1:0".to_string();
        cfg.clickhouse_url = clickhouse_url.to_string();
        EtcdNode::init(cfg, slog::Logger::root(slog::Discard, slog::o!()),
            #[cfg(feature = "tracer")] None).await.unwrap()
    }

    /// Is the message with this delivery key still held by its dispatcher?
    async fn held(n: &EtcdNode, key: &str) -> bool {
        n.range(tonic::Request::new(RangeRequest { key: key.as_bytes().to_vec(), ..Default::default() }))
            .await.unwrap().into_inner().count == 1
    }

    async fn wait_gone(n: &EtcdNode, key: &str) -> bool {
        for _ in 0..100 {
            if !held(n, key).await { return true; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    #[tokio::test]
    async fn a_put_on_a_ch_queue_lands_in_its_table_and_leaves_the_queue() {
        let mut ch = fake_clickhouse(vec![]).await;
        let n = node(&ch.url).await;
        n.kv_put(b"/q/ch:events/p/1".to_vec(), br#"{"id":1}"#.to_vec()).await.unwrap();

        let seen = tokio::time::timeout(Duration::from_secs(5), ch.seen.recv()).await
            .expect("the dispatcher inserts without a consumer").unwrap();
        assert!(seen.head.contains("%60default%60.%60events%60"), "empty clickhouse_db is `default`");
        assert_eq!(seen.body, r#"{"id":1}"#);
        assert!(wait_gone(&n, "/q/ch:events/1/1").await, "a landed row is dropped from the queue");
        assert!(n.kv_get(b"/q/ch:events/p/1").await.is_none(), "a message is never a stored key");
    }

    /// A refused batch keeps every message, and the next wake-up (the next put)
    /// sends them again with the new one - oldest first.
    #[tokio::test]
    async fn a_refused_batch_is_kept_and_retried_on_the_next_put() {
        let mut ch = fake_clickhouse(vec![(500, "down")]).await;
        let n = node(&ch.url).await;
        n.kv_put(b"/queue/clickhouse:db1.t/producer/a".to_vec(), br#"{"n":1}"#.to_vec()).await.unwrap();
        let first = tokio::time::timeout(Duration::from_secs(5), ch.seen.recv()).await.unwrap().unwrap();
        assert!(first.head.contains("%60db1%60.%60t%60"), "db.table in the name wins: {}", first.head);
        // give the failed answer time to be handled, then check it is still held
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(held(&n, "/queue/clickhouse:db1.t/1/a").await, "a failed batch keeps its rows");

        n.kv_put(b"/queue/clickhouse:db1.t/producer/b".to_vec(), br#"{"n":2}"#.to_vec()).await.unwrap();
        let second = tokio::time::timeout(Duration::from_secs(5), ch.seen.recv()).await.unwrap().unwrap();
        assert_eq!(second.body, "{\"n\":1}\n{\"n\":2}", "the kept row goes again, first");
        assert!(wait_gone(&n, "/queue/clickhouse:db1.t/1/a").await);
        assert!(wait_gone(&n, "/queue/clickhouse:db1.t/2/b").await);
    }

    /// An empty value is never sent - and never removed.
    #[tokio::test]
    async fn an_empty_value_is_neither_sent_nor_dropped() {
        let mut ch = fake_clickhouse(vec![]).await;
        let n = node(&ch.url).await;
        n.kv_put(b"/q/ch:events/p/blank".to_vec(), b"  ".to_vec()).await.unwrap();
        n.kv_put(b"/q/ch:events/p/real".to_vec(), br#"{"id":7}"#.to_vec()).await.unwrap();
        let seen = tokio::time::timeout(Duration::from_secs(5), ch.seen.recv()).await.unwrap().unwrap();
        assert_eq!(seen.body, r#"{"id":7}"#, "the blank row is not in the batch");
        assert!(wait_gone(&n, "/q/ch:events/2/real").await);
        assert!(held(&n, "/q/ch:events/1/blank").await, "and it stays queued");
    }

    /// No `clickhouse_url`: the messages stay queued rather than being eaten.
    #[tokio::test]
    async fn without_a_url_the_messages_wait() {
        let n = node("").await;
        n.kv_put(b"/q/ch:events/p/1".to_vec(), br#"{"id":1}"#.to_vec()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(held(&n, "/q/ch:events/1/1").await);
    }

    /// A reconfigure with a URL turns the sink on for the next batch; one with
    /// an empty URL keeps the endpoint already set (agent.md, "Embedding it").
    #[tokio::test]
    async fn a_reconfigure_sets_the_endpoint_and_an_empty_one_keeps_it() {
        use crate::{EtcdEvents, EtcdMgmtEvent};
        let mut ch = fake_clickhouse(vec![]).await;
        let n = node("").await;
        n.kv_put(b"/q/ch:events/p/1".to_vec(), br#"{"id":1}"#.to_vec()).await.unwrap();
        // its wake-up found no endpoint and left it queued
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(held(&n, "/q/ch:events/1/1").await);

        let mut c = crate::cli::EtcdConfig::default();
        c.clickhouse_url = ch.url.clone();
        n.event.send(EtcdEvents::Mgmt(EtcdMgmtEvent::Config(c))).await.unwrap();
        let mut c = crate::cli::EtcdConfig::default();
        c.clickhouse_url = String::new();
        n.event.send(EtcdEvents::Mgmt(EtcdMgmtEvent::Config(c))).await.unwrap();
        for _ in 0..100 {
            if n.cfg.read().await.clickhouse_url == ch.url { break; }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(n.cfg.read().await.clickhouse_url, ch.url, "an empty url kept the endpoint");

        n.kv_put(b"/q/ch:events/p/2".to_vec(), br#"{"id":2}"#.to_vec()).await.unwrap();
        let seen = tokio::time::timeout(Duration::from_secs(5), ch.seen.recv()).await.unwrap().unwrap();
        assert_eq!(seen.body, "{\"id\":1}\n{\"id\":2}");
    }
}
