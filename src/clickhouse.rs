//! Built-in ClickHouse consumer for queues named `/q/ch:<table>/...` or
//! `/queue/clickhouse:<table>/...`.
//!
//! A message put on such a queue carries a JSON object as its value, and the
//! queue writes it straight into that ClickHouse table as `JSONEachRow` — no
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
            .map_err(|e| format!("connect {}: {}", self.authority, e))?;
        let (mut sender, conn) = hyper::client::conn::http1::handshake(
            hyper_util::rt::TokioIo::new(stream)).await
            .map_err(|e| format!("handshake: {}", e))?;
        let log_c = log.clone();
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                slog::trace!(log_c, "[ch] connection closed: {}", e);
            }
        });

        let mut req = Request::post(&uri).header("Host", self.authority.as_str());
        if let Some(a) = &self.auth {
            req = req.header("Authorization", a.as_str());
        }
        let req = req.body(Full::new(Bytes::from(rows.join("\n"))))
            .map_err(|e| format!("request: {}", e))?;

        let resp = tokio::time::timeout(CH_TIMEOUT, sender.send_request(req)).await
            .map_err(|_| format!("timed out after {:?}", CH_TIMEOUT))?
            .map_err(|e| format!("send: {}", e))?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        // ClickHouse puts the reason in the body, not the status line
        let msg = resp.into_body().collect().await
            .map(|b| String::from_utf8_lossy(&b.to_bytes()).trim().to_string())
            .unwrap_or_default();
        Err(format!("{} {}", status, msg))
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
}
