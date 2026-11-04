//! [bridge-http] An HTTP or HTTPS endpoint as the bridge's target (bridge.md "HTTP and
//! HTTPS"): one POST per message, the value its body, taken when the endpoint answers 2xx.
//! HTTP/1.1 over hyper's low-level client, which tonic already pulls in, and rustls as the
//! other targets, so this adds no new crates.
//!
//! - `user:password@` in the URL is Basic auth; the user `bearer` is `Authorization: Bearer
//!   <password>`. A login goes over TLS, or to this host's loopback.
//! - The URL's path and query are the endpoint's own and sent as they are, `?ca=<PEM file>`
//!   on `https://` aside. They can hold a secret, so the log shows neither ([`shown`]). An `@`
//!   in them is refused, `%40` written instead: it reads as a password cut short by its `/`.
//! - One connection, kept alive. One the endpoint closed while idle is made again before
//!   a send, so an idle close costs no retry.
//! - A redirect is not followed: the POST, with its login, would go to whoever the answer
//!   names. It is retried like any other answer that took nothing, and logged.
//! - What the answer's body says is not read: not logged (it can echo a token), not kept.

use std::net::Ipv6Addr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::client::conn::http1::{self, SendRequest};
use hyper::header::{AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, HOST, HeaderName, HeaderValue, USER_AGENT};
use hyper::http::uri::PathAndQuery;
use hyper::{Request, StatusCode};
use hyper_util::rt::TokioIo;
use rustls_pki_types::ServerName;
use tokio::net::TcpStream;
use tokio::task::JoinHandle;
use tokio_rustls::TlsConnector;

use super::{Sent, percent_decode};
use crate::b64::b64;
use crate::bridge::Config;
use crate::bridge::cloudevents;
use crate::bridge::mapping::{BodyType, Delivery, KvEvent, Shape, body_text};

/// The `Content-Type` of a value that is a JSON object or array, with `bridge_body auto`
pub const JSON: &str = "application/json";
/// The user that means a bearer token: the URL's password is the token
const BEARER: &str = "bearer";
/// Of an answer's body: past this, or this long, the connection is dropped, not the answer.
/// The status came, so the endpoint decided: a body that stalls is no reason to send again
const BODY_MAX: usize = 64 * 1024;
const BODY_WAIT: Duration = Duration::from_secs(1);
/// What the wait for a body leaves of the send timeout: past that the bridge takes the
/// answer for none, and sends the message again
const BODY_MARGIN: Duration = Duration::from_millis(100);

/// A target URL: `http[s]://[user[:password]@]host[:port][/path][?query]`.
#[derive(Clone, PartialEq)]
pub struct HttpUrl {
    pub tls: bool,
    /// as written: an IPv6 literal with its brackets
    pub host: String,
    pub port: u16,
    /// percent-decoded
    pub user: Option<(String, String)>,
    /// the path and query the request names: `/` at least
    pub target: String,
    /// `?ca=<PEM file>` on `https://`: the only CAs then trusted
    pub ca: Option<String>,
}

/// No login, no path and no query: they can hold a secret
impl std::fmt::Debug for HttpUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpUrl").field("tls", &self.tls).field("host", &self.host).field("port", &self.port)
            .field("ca", &self.ca).finish_non_exhaustive()
    }
}

impl HttpUrl {
    /// What the bridge refuses to start with. The errors never show the URL's login, and a
    /// password holding `/ ? # @` must be percent-encoded, or it is read as the host's end.
    pub fn parse(url: &str) -> Result<Self, String> {
        let tls = match url.split_once("://").map(|(s, _)| s) {
            Some("https") => true,
            Some("http") => false,
            _ => return Err(tr!("bridge.http_scheme", url = shown(url))),
        };
        let whole = url.split_once("://").map_or(url, |(_, r)| r);
        // a fragment is the client's own: never sent
        let rest = whole.split('#').next().unwrap_or(whole);
        let (authority, tail) = rest.split_at(rest.find(['/', '?']).unwrap_or(rest.len()));
        let (login, hostport) = match authority.rsplit_once('@') {
            Some((l, h)) => (Some(l), h),
            None => (None, authority),
        };
        let user = login.map(|l| {
            let (u, p) = l.split_once(':').unwrap_or((l, ""));
            (percent_decode(u), percent_decode(p))
        });
        let (host, port) = host_port(hostport, tls)?;
        // a password's `/`, `?` or `#` ends the authority, so `app:2024/rest@ci/hook` reads as
        // host `app`, port 2024, and `u:p@ss/w@ci/x` sends its login to `ss`: the POST would take
        // it to whoever that is. Nothing tells those from a path with an `@`, and `%40` writes
        // that as well
        if whole[authority.len()..].contains('@') {
            return Err(tr!("bridge.http_at_in_tail", url = shown(url)));
        }
        if let Some((u, p)) = &user {
            if u.is_empty() {
                return Err(tr!("bridge.http_no_user", url = shown(url)));
            }
            if u == BEARER && p.is_empty() {
                return Err(tr!("bridge.http_no_token", url = shown(url)));
            }
            if !tls && !loopback(&host) {
                return Err(tr!("bridge.http_login_plain", url = shown(url)));
            }
        }
        let (path, query) = tail.split_once('?').unwrap_or((tail, ""));
        // `ca` is ours on https://, and the rest of the query the endpoint's. On http:// it
        // is the endpoint's too: there is nothing to trust
        let mut ca = None;
        let mut kept = vec![];
        for opt in query.split('&').filter(|o| !o.is_empty()) {
            match opt.strip_prefix("ca=") {
                Some(path) if tls => {
                    if path.is_empty() {
                        return Err(tr!("bridge.http_ca_empty", url = shown(url)));
                    }
                    ca = Some(percent_decode(path));
                }
                _ => kept.push(opt),
            }
        }
        let path = if path.is_empty() { "/" } else { path };
        let target = if kept.is_empty() { path.to_string() } else { format!("{}?{}", path, kept.join("&")) };
        // the error names the part: its text would show the path, which can hold a secret
        target.parse::<PathAndQuery>().map_err(|_| tr!("bridge.http_bad_path", url = shown(url)))?;
        let parsed = HttpUrl { tls, host, port, user, target, ca };
        // a line end from a stored secret, say: refused, not sent with no login at all
        if parsed.user.is_some() && parsed.authorization().is_none() {
            return Err(tr!("bridge.http_bad_login", url = shown(url)));
        }
        Ok(parsed)
    }

    /// The `Host` header: the port only when it is not the scheme's
    fn authority(&self) -> String {
        if self.port == if self.tls { 443 } else { 80 } {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// The `Authorization` header, marked sensitive
    fn authorization(&self) -> Option<HeaderValue> {
        let (user, password) = self.user.as_ref()?;
        let text = if user == BEARER {
            format!("Bearer {}", password)
        } else {
            format!("Basic {}", b64(format!("{}:{}", user, password).as_bytes()))
        };
        let mut v = HeaderValue::from_str(&text).ok()?;
        v.set_sensitive(true);
        Some(v)
    }
}

/// `host[:port]`: a name, an IPv4 or a bracketed IPv6, the scheme's port when none is given
fn host_port(s: &str, tls: bool) -> Result<(String, u16), String> {
    let default = if tls { 443 } else { 80 };
    let port = |p: &str| p.parse::<u16>().map_err(|_| tr!("bridge.http_bad_port"));
    if let Some(r) = s.strip_prefix('[') {
        let (h, after) = r.split_once(']').ok_or_else(|| tr!("bridge.http_open_literal"))?;
        h.parse::<Ipv6Addr>().map_err(|_| tr!("bridge.http_bad_literal"))?;
        return Ok((format!("[{}]", h), match after.strip_prefix(':') {
            Some(p) => port(p)?,
            None if after.is_empty() => default,
            None => return Err(tr!("bridge.http_after_literal")),
        }));
    }
    let (h, p) = match s.rsplit_once(':') {
        Some((h, p)) => (h, port(p)?),
        None => (s, default),
    };
    if h.is_empty() || h.contains(':') {
        return Err(tr!("bridge.http_no_host"));
    }
    Ok((h.to_string(), p))
}

/// a host of this machine alone: a login to it crosses no network
fn loopback(host: &str) -> bool {
    let h = host.trim_matches(['[', ']']);
    h == "localhost" || h.parse::<std::net::IpAddr>().map(|a| a.is_loopback()).unwrap_or(false)
}

/// What the log says of a URL: scheme, user and host, with `/...` for a path or query
/// there is. Those can hold the secret - a Slack hook's path, a `?token=`.
pub fn shown(url: &str) -> String {
    // text before a later `://` is no scheme: `https:/bearer:pw@ci/x?cb=https://me/`
    let Some((scheme, rest)) = url.split_once("://").filter(|(s, _)| s.bytes().all(|b| b.is_ascii_alphanumeric() || b"+-.".contains(&b))) else {
        return tr!("bridge.http_no_scheme");
    };
    let (authority, tail) = rest.split_at(rest.find(['/', '?', '#']).unwrap_or(rest.len()));
    // an `@` further on: a password's `/` cut this short, so what is before it may be its start
    if tail.contains('@') {
        return format!("{}://***/...", scheme);
    }
    let authority = match authority.rsplit_once('@') {
        // a user with a password is a name; one alone may be the token itself
        Some((login, host)) => match login.split_once(':') {
            Some((user, _)) => format!("{}:***@{}", user, host),
            None => format!("***@{}", host),
        },
        None => shown_host(authority),
    };
    format!("{}://{}{}", scheme, authority, if tail.trim_matches('/').is_empty() { "" } else { "/..." })
}

/// `host[:port]` as it is, when that is what it is: anything but a port after a host, or a
/// broken IPv6 literal, is a password's start
fn shown_host(authority: &str) -> String {
    if let Some(rest) = authority.strip_prefix('[') {
        let valid = rest.split_once(']').is_some_and(|(ip, after)| ip.parse::<Ipv6Addr>().is_ok()
            && (after.is_empty() || after.strip_prefix(':').is_some_and(|p| p.parse::<u16>().is_ok())));
        return if valid { authority.to_string() } else { "***".to_string() };
    }
    match authority.split_once(':') {
        Some((host, port)) if port.parse::<u16>().is_err() => format!("{}:***", host),
        _ => authority.to_string(),
    }
}

/// A connection, and the task that drives it
struct Conn {
    sender: SendRequest<Full<Bytes>>,
    driver: JoinHandle<()>,
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.driver.abort();
    }
}

async fn over<T>(io: T) -> Result<Conn, String>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (sender, conn) = http1::handshake(TokioIo::new(io)).await.map_err(|e| e.to_string())?;
    // what ends it is seen at the next send: `ready` says closed
    let driver = tokio::spawn(async move {
        let _ = conn.await;
    });
    Ok(Conn { sender, driver })
}

pub struct Http {
    url: HttpUrl,
    tls: Option<TlsConnector>,
    conn: Option<Conn>,
    send_timeout: Duration,
}

impl Http {
    /// Connects now, as the other targets do: a refused certificate or a closed port is
    /// said and tried again before the first message is out, not at it
    pub async fn connect(cfg: &Config) -> Result<Self, String> {
        let url = HttpUrl::parse(&cfg.target_url)?;
        let tls = match url.tls {
            true => {
                let mut c = super::rustls_client(url.ca.as_deref())?;
                c.alpn_protocols = vec![b"http/1.1".to_vec()];
                Some(TlsConnector::from(Arc::new(c)))
            }
            false => None,
        };
        let mut http = Http { url, tls, conn: None, send_timeout: cfg.send_timeout };
        http.conn = Some(http.dial().await?);
        Ok(http)
    }

    async fn dial(&self) -> Result<Conn, String> {
        let host = self.url.host.trim_matches(['[', ']']);
        let stream = TcpStream::connect((host, self.url.port)).await.map_err(|e| e.to_string())?;
        let _ = stream.set_nodelay(true);
        match &self.tls {
            Some(connector) => {
                let name = ServerName::try_from(host.to_string()).map_err(|e| format!("TLS {}: {}", host, e))?;
                let tls = connector.connect(name, stream).await.map_err(|e| format!("TLS: {}", e))?;
                over(tls).await
            }
            None => over(stream).await,
        }
    }

    /// The connection, made again when the endpoint closed the last one: its idle timeout
    /// or `Connection: close` is no failure of this message
    async fn ready(&mut self) -> Result<(), String> {
        if let Some(c) = &mut self.conn {
            if c.sender.ready().await.is_ok() {
                return Ok(());
            }
        }
        self.conn = None;
        self.conn = Some(self.dial().await?);
        Ok(())
    }

    /// An `Err` is a connection that no longer works, and may have taken the message: the
    /// bridge connects again and sends it again, at least once, with its one message id
    pub async fn send(&mut self, d: &Delivery, shape: &Shape, now_ms: i64) -> Result<Sent, String> {
        let started = Instant::now();
        let req = match request(&self.url, d, shape, now_ms) {
            Ok(r) => r,
            Err(why) => return Ok(Sent::Permanent(why)),
        };
        self.ready().await?;
        let conn = self.conn.as_mut().ok_or_else(|| tr!("bridge.http_no_conn"))?;
        let answer = match conn.sender.send_request(req).await {
            Ok(a) => a,
            Err(e) => {
                self.conn = None;
                return Err(e.to_string());
            }
        };
        let status = answer.status();
        let left = self.send_timeout.saturating_sub(started.elapsed()).saturating_sub(BODY_MARGIN);
        if !drained(answer.into_body(), BODY_WAIT.min(left)).await {
            self.conn = None;
        }
        Ok(sent(status))
    }
}

/// An answer's body read to its end, so its connection can be used again: false when it
/// cannot be, one too long, too slow, or broken
async fn drained(mut body: Incoming, wait: Duration) -> bool {
    let read = async {
        let mut n = 0;
        while let Some(frame) = body.frame().await {
            let Ok(frame) = frame else { return false };
            n += frame.data_ref().map_or(0, |d| d.len());
            if n > BODY_MAX {
                return false;
            }
        }
        true
    };
    tokio::time::timeout(wait, read).await.unwrap_or(false)
}

/// What a status means, as the other targets' refusals do: for good only what says the
/// message itself will never do. An expired token (401, 403) or an endpoint being deployed
/// (404) is fixed there, and the message then lands: not dead-lettered with the rest. 431
/// is the message's too: its key rides in the headers
pub fn sent(status: StatusCode) -> Sent {
    match status.as_u16() {
        200..=299 => Sent::Accepted,
        400 | 413 | 415 | 422 | 431 => Sent::Permanent(format!("HTTP {}", status.as_u16())),
        300..=399 => Sent::Retry(tr!("bridge.http_redirect", status = status.as_u16())),
        _ => Sent::Retry(format!("HTTP {}", status.as_u16())),
    }
}

/// The body and its `Content-Type`; a key's delete has neither
fn content(d: &Delivery, body: BodyType) -> Result<(Bytes, Option<&'static str>), String> {
    if d.event == Some(KvEvent::Delete) {
        return Ok((Bytes::new(), None));
    }
    let kind = match (body_text(d, body)?, body) {
        (Some(t), BodyType::Auto) if is_json(t) => JSON,
        (Some(_), _) => cloudevents::TEXT,
        (None, _) => cloudevents::BYTES,
    };
    Ok((Bytes::copy_from_slice(&d.value), Some(kind)))
}

/// A JSON object or array: many endpoints refuse one sent as `text/plain`
fn is_json(t: &str) -> bool {
    let start = t.trim_start();
    (start.starts_with('{') || start.starts_with('[')) && serde_json::from_str::<serde::de::IgnoredAny>(t).is_ok()
}

/// A value for a header, as CloudEvents' HTTP binding writes one: percent-encoded but for
/// printable ASCII less `"` and `%`. A key is any bytes: this is what keeps a CR or LF in
/// one from making a header of its own
pub fn header_text(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for b in v.bytes() {
        if (0x21..=0x7e).contains(&b) && b != b'"' && b != b'%' {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

fn request(url: &HttpUrl, d: &Delivery, shape: &Shape, now_ms: i64) -> Result<Request<Full<Bytes>>, String> {
    let (body, kind) = content(d, shape.body)?;
    // said even at 0 - a key's delete: hyper would leave it out, and a server may answer 411
    let mut r = Request::post(url.target.as_str())
        .header(HOST, url.authority())
        .header(CONTENT_LENGTH, body.len())
        .header(USER_AGENT, concat!("etcds/", env!("CARGO_PKG_VERSION")));
    if let Some(kind) = kind {
        r = r.header(CONTENT_TYPE, kind);
    }
    if let Some(a) = url.authorization() {
        r = r.header(AUTHORIZATION, a);
    }
    for (name, value) in headers(d, shape, now_ms) {
        r = r.header(name, value);
    }
    r.body(Full::new(body)).map_err(|e| e.to_string())
}

/// The CloudEvents attributes as `ce-{name}` (`Content-Type` is `datacontenttype`), or the
/// `etcd_*` properties and the message id, each `_` a `-`: nginx drops a header with an
/// underscore unless told not to
fn headers(d: &Delivery, shape: &Shape, now_ms: i64) -> Vec<(HeaderName, HeaderValue)> {
    let pairs: Vec<(String, String)> = if shape.cloudevents {
        cloudevents::attributes(d, &shape.ce_type, true, now_ms).into_iter()
            .filter(|(k, _)| *k != "datacontenttype")
            .map(|(k, v)| (format!("ce-{}", k), v))
            .collect()
    } else {
        let mut p: Vec<(String, String)> = d.properties().into_iter()
            .map(|(k, v)| (k.replace('_', "-"), v.to_string()))
            .collect();
        p.push(("message-id".to_string(), d.event_id().to_string()));
        p
    };
    pairs.into_iter().filter_map(|(k, v)| {
        Some((HeaderName::from_bytes(k.as_bytes()).ok()?, HeaderValue::from_str(&header_text(&v)).ok()?))
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(u: &str) -> HttpUrl {
        HttpUrl::parse(u).unwrap_or_else(|e| panic!("{}: {}", u, e))
    }

    #[test]
    fn a_url_and_its_parts() {
        let u = url("https://ci.example.com/hook?a=1&b=2#frag");
        assert_eq!((u.tls, u.host.as_str(), u.port, u.target.as_str()), (true, "ci.example.com", 443, "/hook?a=1&b=2"));
        assert_eq!((u.authority().as_str(), u.user.clone(), u.ca.clone()), ("ci.example.com", None, None));
        let u = url("http://h:8080");
        assert_eq!((u.tls, u.port, u.target.as_str(), u.authority().as_str()), (false, 8080, "/", "h:8080"));
        assert_eq!(url("http://h?x=1").target, "/?x=1");
        assert_eq!(url("http://h:80/a").authority(), "h", "the scheme's port is not said");
        assert_eq!(url("https://h:80/a").authority(), "h:80");
        let u = url("https://[2001:db8::1]:8443/x");
        assert_eq!((u.host.as_str(), u.port, u.authority().as_str()), ("[2001:db8::1]", 8443, "[2001:db8::1]:8443"));
        assert_eq!(url("http://[::1]/").port, 80);
    }

    /// an `@` after the host may be a password's, cut short by its `/`: refused, `%40` is the
    /// same to the endpoint. Written so, it is the path's, and no login: the other targets'
    /// `split_url` takes the last `@` in the whole URL
    #[test]
    fn an_at_sign_after_the_host_is_refused_and_written_percent_40() {
        for bad in ["https://h/a@b/c:d@e", "https://app:2024/rest@ci/hook", "http://app:2024?x@ci/hook", "https://app:2024/rest#x@ci/hook",
            "https://app:2024#x@ci/", "https://s3/cret@ci/x", "https://u:p@ss/w@ci/x", "https://u:p@h:8443/a@b", "http://[::1]:8/a@b", "https://h?email=a@b"] {
            let e = HttpUrl::parse(bad).unwrap_err();
            assert!(e.contains("%40"), "{}: {}", bad, e);
            assert!(e.contains("://***/..."), "shown masked: {}: {}", bad, e);
        }
        let u = url("https://h:8443/a%40b");
        assert_eq!((u.host.as_str(), u.port, u.user.clone(), u.target.as_str()), ("h", 8443, None, "/a%40b"));
        let u = url("https://app:p%40ss%2F@h/a%40b");
        assert_eq!(u.user, Some(("app".to_string(), "p@ss/".to_string())));
        assert_eq!(u.target, "/a%40b");
    }

    #[test]
    fn a_ca_comes_out_of_the_query_on_https_only() {
        let u = url("https://h/x?a=1&ca=/etc/a%40b.pem&token=t");
        assert_eq!((u.ca.as_deref(), u.target.as_str()), (Some("/etc/a@b.pem"), "/x?a=1&token=t"));
        let u = url("https://h/x?ca=/c.pem");
        assert_eq!((u.ca.as_deref(), u.target.as_str()), (Some("/c.pem"), "/x"));
        let u = url("http://h/x?ca=1");
        assert_eq!((u.ca, u.target.as_str()), (None, "/x?ca=1"), "on http:// it is the endpoint's");
        assert_eq!(url("https://h/x?cat=1").target, "/x?cat=1");
    }

    #[test]
    fn what_a_url_is_refused_for() {
        for bad in [
            "ftp://h", "h:80", "http://", "https:///x", "http://h:port/", "http://h:99999/", "http://:80/",
            "http://[::1", "http://[nope]/", "http://[::1]x/", "http://a:b:c/",
            "http://u:p@ci.example.com/", "http://bearer:t@10.0.0.1/", // a login in the clear
            "https://bearer@h/", "https://bearer:@h/", "https://:p@h/",
            "https://h/x?ca=", "http://h/a b",
        ] {
            assert!(HttpUrl::parse(bad).is_err(), "{} taken", bad);
        }
        // the loopback's own is allowed: it crosses no network
        for ok in ["http://u:p@127.0.0.1/", "http://u:p@localhost:8/", "http://u:p@[::1]:8/", "https://u:p@ci/", "https://bearer:t@ci/"] {
            url(ok);
        }
        for secret in ["http://user:s3cret@ci.example.com/", "http://h:port/s3cret", "http://h/s3cret x", "https://h/s3cret?ca="] {
            assert!(!HttpUrl::parse(secret).unwrap_err().contains("s3cret"), "{}", secret);
        }
    }

    #[test]
    fn the_log_shows_no_secret() {
        assert_eq!(shown("https://ci.example.com"), "https://ci.example.com");
        assert_eq!(shown("https://ci.example.com/"), "https://ci.example.com");
        assert_eq!(shown("https://hooks.slack.com/services/T0/B0/s3cret"), "https://hooks.slack.com/...");
        assert_eq!(shown("https://ci/x?token=s3cret"), "https://ci/...");
        assert_eq!(shown("https://ci?token=s3cret"), "https://ci/...");
        assert_eq!(shown("https://bearer:s3cret@ci:8443/x"), "https://bearer:***@ci:8443/...");
        assert_eq!(shown("https://app:p%40s3cret@ci"), "https://app:***@ci");
        assert_eq!(crate::bridge::redact("https://bearer:s3cret@ci/s3cret"), "https://bearer:***@ci/...");
        assert_eq!(crate::bridge::redact("amqp://app:s3cret@h:5672"), "amqp://app:***@h:5672", "the others as before");
        assert_eq!(shown("s3cret"), "(no scheme)");
        // a user alone may be the token; a password's `/` cuts the URL short
        assert_eq!(shown("https://sk_live_s3cret@api.example.com/v1"), "https://***@api.example.com/...");
        assert_eq!(shown("https://bearer:s3cret/def@ci/hook"), "https://***/...");
        assert_eq!(shown("https://app:2024/rest@ci/hook"), "https://***/...");
        assert_eq!(shown("https://app:s3cr]/et@ci/hook"), "https://***/...");
        assert_eq!(shown("https://u:p@ss/w@ci/x"), "https://***/...");
        assert_eq!(shown("https://bearer:s3cret/x"), "https://bearer:***/...");
        assert_eq!(shown("https://[u:s3cret"), "https://***");
        assert_eq!(shown("https://[::1]:s3cret/x"), "https://***/...");
        assert_eq!(shown("https://a:s3cret:x/y"), "https://a:***/...");
        assert_eq!(shown("https:/bearer:s3cret@ci/hook?cb=https://me/x"), "(no scheme)");
        assert_eq!(shown("https://h:8443/x"), "https://h:8443/...", "a port is shown");
        assert_eq!(shown("https://[::1]:8443"), "https://[::1]:8443");
        // a URL no target takes: no telling where its secret is
        let r = crate::bridge::redact;
        assert_eq!(r("htps://hooks.slack.com/services/T0/B0/s3cret"), "htps://hooks.slack.com/...");
        assert_eq!(r("HTTPS://bearer:s3cret@ci/hook?token=s3cret"), "HTTPS://bearer:***@ci/...");
        assert_eq!(r("https:/bearer:s3cret@ci/hook"), "(no scheme)");
        assert!(!crate::bridge::broker::kind("htps://ci/s3cret").unwrap_err().contains("s3cret"));
        // nor does Debug show them
        let d = format!("{:?}", url("https://bearer:s3cret@h/x?token=s3cret"));
        assert!(!d.contains("s3cret") && d.contains("\"h\""), "{}", d);
    }

    #[test]
    fn the_login_header() {
        let a = |u: &str| url(u).authorization();
        assert_eq!(a("https://h/"), None);
        assert_eq!(a("https://bearer:tok%20en@h/").unwrap().to_str().unwrap(), "Bearer tok en");
        assert_eq!(a("https://user:pw@h/").unwrap().to_str().unwrap(), "Basic dXNlcjpwdw==");
        assert_eq!(a("https://user@h/").unwrap().to_str().unwrap(), "Basic dXNlcjo=", "a user alone, an empty password");
        assert!(a("https://user:pw@h/").unwrap().is_sensitive());
        // a token no header can hold: refused, never sent with no login at all
        for bad in ["https://bearer:s3cret%0Ab@h/", "https://bearer:s3cret%0A@h/", "https://bearer:s3cret%0D@h/", "https://bearer:s3cret%00@h/"] {
            let e = HttpUrl::parse(bad).unwrap_err();
            assert!(e.contains("no header can hold") && !e.contains("s3cret"), "{}: {}", bad, e);
        }
        // not ASCII: a header carries the bytes, so it is sent, not dropped
        assert!(a("https://bearer:%D1%82@h/").is_some());
    }

    #[test]
    fn each_status() {
        let s = |c: u16| sent(StatusCode::from_u16(c).unwrap());
        for c in [200, 201, 202, 204, 299] {
            assert_eq!(s(c), Sent::Accepted, "{}", c);
        }
        for c in [400, 413, 415, 422, 431] {
            assert!(matches!(s(c), Sent::Permanent(_)), "{}", c);
        }
        // nothing else is for good: an expired token, a hook being deployed, a limit, a redirect
        for c in [301, 302, 307, 308, 401, 403, 404, 405, 408, 409, 410, 425, 429, 500, 502, 503, 504] {
            assert!(matches!(s(c), Sent::Retry(_)), "{}", c);
        }
        assert!(matches!(s(302), Sent::Retry(w) if w.contains("redirect")));
    }

    #[test]
    fn a_value_and_its_type() {
        let kind = |v: &[u8], b| content(&Delivery::new("q", b"/q/q/p/k", v), b).unwrap().1;
        assert_eq!(kind(br#"{"a":1}"#, BodyType::Auto), Some(JSON));
        assert_eq!(kind(b" [1, 2]\n", BodyType::Auto), Some(JSON));
        assert_eq!(kind(b"{not json", BodyType::Auto), Some(cloudevents::TEXT));
        assert_eq!(kind(b"5", BodyType::Auto), Some(cloudevents::TEXT), "only an object or an array");
        assert_eq!(kind(b"\xff\xfe", BodyType::Auto), Some(cloudevents::BYTES));
        assert_eq!(kind(br#"{"a":1}"#, BodyType::Text), Some(cloudevents::TEXT), "what was asked for");
        assert_eq!(kind(br#"{"a":1}"#, BodyType::Bytes), Some(cloudevents::BYTES));
        assert!(content(&Delivery::new("q", b"/q/q/p/k", b"\xff"), BodyType::Text).is_err());
        let d = Delivery::kv("/c/", b"/c/k", b"", KvEvent::Delete);
        assert_eq!(content(&d, BodyType::Auto).unwrap(), (Bytes::new(), None));
    }

    #[test]
    fn a_key_cannot_make_a_header() {
        assert_eq!(header_text("job-1:a/b"), "job-1:a/b");
        assert_eq!(header_text("a b\"c%d"), "a%20b%22c%25d");
        assert_eq!(header_text("x\r\nevil: 1"), "x%0D%0Aevil:%201");
        assert_eq!(header_text("ключ"), "%D0%BA%D0%BB%D1%8E%D1%87");
        let d = Delivery::new("q", b"/q/q/7/x\r\nevil: 1", b"v");
        let u = url("https://bearer:t@ci:8443/hook?a=1");
        let r = request(&u, &d, &Shape::default(), 0).unwrap();
        assert_eq!(r.uri().to_string(), "/hook?a=1");
        assert_eq!(r.headers()["host"], "ci:8443");
        assert!(r.headers().get("evil").is_none());
        assert_eq!(r.headers()["etcd-key"], "x%0D%0Aevil:%201");
        assert_eq!(r.headers()["etcd-index"], "7");
        assert_eq!(r.headers()["message-id"], d.event_id());
        assert!(r.headers()["user-agent"].to_str().unwrap().starts_with("etcds/"));
    }

    #[test]
    fn cloudevents_in_the_http_binding() {
        let d = Delivery::new("q", b"/q/q/7/k", br#"{"a":1}"#);
        let ce = Shape { cloudevents: true, ce_type: "x.y".to_string(), ..Shape::default() };
        let r = request(&url("http://h/"), &d, &ce, 1_700_000_000_123).unwrap();
        let h = r.headers();
        assert_eq!(h["ce-specversion"], "1.0");
        assert_eq!(h["ce-id"], d.event_id());
        assert_eq!(h["ce-type"], "x.y");
        assert_eq!(h["ce-source"], "/q/q");
        assert_eq!(h["ce-subject"], "k");
        assert_eq!(h["ce-etcdindex"], "7");
        assert_eq!(h["content-type"], JSON);
        assert!(h.get("ce-datacontenttype").is_none(), "it is the Content-Type");
        assert!(h.get("etcd-key").is_none(), "the attributes instead, as on the other targets");
    }

    /// no login goes out unmarked, and a value that is not text is for the dead-letter queue
    #[test]
    fn a_value_that_cannot_be_sent_is_refused_for_good() {
        let d = Delivery::new("q", b"/q/q/p/k", b"\xff");
        let s = Shape { body: BodyType::Text, ..Shape::default() };
        assert!(request(&url("http://h/"), &d, &s, 0).is_err());
    }
}
