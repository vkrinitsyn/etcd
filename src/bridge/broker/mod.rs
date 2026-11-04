//! Where the bridge sends: an AMQP 1.0 broker (JMS), an MQTT 5 broker, Kafka or an
//! HTTP(S) endpoint, by the scheme of the target URL. Each tells the bridge, per message, whether
//! the broker took it ([`Sent`]); an `Err` is a connection that no longer
//! works, and is made again.

pub mod amqp;
pub mod http;
pub mod kafka;
pub mod mqtt;

use std::time::Duration;

use crate::bridge::Config;
use crate::bridge::mapping::Delivery;

/// A broker's answer to one message.
#[derive(Debug, Clone, PartialEq)]
pub enum Sent {
    /// Taken for good: acknowledge it in etcd.
    Accepted,
    /// Never taken as it is: the dead-letter queue, or held.
    Permanent(String),
    /// Not taken now: send it again.
    Retry(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Amqp,
    Mqtt,
    Kafka,
    Http,
}

/// The target's kind, by the URL's scheme.
pub fn kind(url: &str) -> Result<Kind, String> {
    match url.split_once("://").map(|(s, _)| s) {
        Some("amqp" | "amqps") => Ok(Kind::Amqp),
        Some("mqtt" | "mqtts") => Ok(Kind::Mqtt),
        Some("kafka" | "kafkas") => Ok(Kind::Kafka),
        Some("http" | "https") => Ok(Kind::Http),
        _ => Err(tr!("bridge.bad_scheme", url = crate::bridge::redact(url))),
    }
}

/// [bridge-ca] An AMQP or MQTT target URL and the CA file it names, `?ca=<PEM file>`:
/// the URL without it, for the client, and the file. Only on `amqps://` and `mqtts://`;
/// any other option refused. Kafka's URL has options of its own (`kafka::KafkaUrl`)
///
/// The options start at the first `?` after the host - after the last `@`, as
/// `split_url` reads the login - so a `?` in a password is the password's. An `@` in
/// the CA's path is written `%40`: one as it is would put the host after it, and the
/// `ca=` is refused rather than left unread.
pub fn tls_ca(url: &str) -> Result<(String, Option<String>), String> {
    let start = url.find("://").map_or(0, |s| s + 3);
    let host = url[start..].rfind('@').map_or(start, |a| start + a + 1);
    let Some(q) = url[host..].find('?').map(|q| host + q) else {
        if url[start..].contains("?ca=") || url[start..].contains("&ca=") {
            return Err(tr!("bridge.ca_unreadable", url = crate::bridge::redact(url)));
        }
        return Ok((url.to_string(), None));
    };
    let (base, query) = (&url[..q], &url[q + 1..]);
    let tls = url.starts_with("amqps://") || url.starts_with("mqtts://");
    let mut ca = None;
    for opt in query.split('&').filter(|o| !o.is_empty()) {
        match opt.split_once('=') {
            Some(("ca", path)) if !path.is_empty() => {
                if !tls {
                    return Err(tr!("bridge.ca_plain", url = crate::bridge::redact(base)));
                }
                ca = Some(percent_decode(path));
            }
            // the option's name only: what follows the `=` is the caller's
            _ => return Err(tr!("bridge.ca_option", url = crate::bridge::redact(base),
                option = format!("{:?}", opt.split('=').next().unwrap_or_default()))),
        }
    }
    Ok((base.to_string(), ca))
}

/// [bridge-ca] The certificates a PEM file holds, every one, read at start: none in
/// it, or a file that does not read, is refused
pub fn ca_certs(path: &str) -> Result<Vec<rustls_pki_types::CertificateDer<'static>>, String> {
    use rustls_pki_types::CertificateDer;
    use rustls_pki_types::pem::PemObject;
    let certs = CertificateDer::pem_file_iter(path).map_err(|e| format!("ca {}: {}", path, e))?
        .collect::<Result<Vec<_>, _>>().map_err(|e| format!("ca {}: {}", path, e))?;
    if certs.is_empty() {
        return Err(tr!("bridge.ca_no_cert", path = path));
    }
    Ok(certs)
}

/// [bridge-ca] The CAs a PEM file holds, for rustls (AMQP, Kafka)
pub fn ca_roots(path: &str) -> Result<rustls::RootCertStore, String> {
    let mut roots = rustls::RootCertStore::empty();
    for cert in ca_certs(path)? {
        roots.add(cert).map_err(|e| format!("ca {}: {}", path, e))?;
    }
    Ok(roots)
}

/// TLS trusting the CAs in `ca`, or the public (webpki) roots - with AWS-LC named: the
/// AMQP client brings it and rskafka ring, and rustls takes no default then (Kafka, HTTP)
pub(crate) fn rustls_client(ca: Option<&str>) -> Result<rustls::ClientConfig, String> {
    let roots = match ca {
        Some(path) => ca_roots(path)?,
        None => {
            let mut roots = rustls::RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            roots
        }
    };
    let provider = std::sync::Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    Ok(rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_root_certificates(roots)
        .with_no_client_auth())
}

pub enum Target {
    /// boxed: a connection, session and link are large beside the others
    Amqp(Box<amqp::Amqp>),
    Mqtt(mqtt::Mqtt),
    Kafka(kafka::Kafka),
    Http(http::Http),
}

impl Target {
    pub async fn connect(cfg: &Config) -> Result<Self, String> {
        tls_provider();
        let connect = async {
            Ok(match kind(&cfg.target_url)? {
                Kind::Amqp => Target::Amqp(Box::new(amqp::Amqp::connect(cfg).await?)),
                Kind::Mqtt => Target::Mqtt(mqtt::Mqtt::connect(cfg).await?),
                Kind::Kafka => Target::Kafka(kafka::Kafka::connect(cfg).await?),
                Kind::Http => Target::Http(http::Http::connect(cfg).await?),
            })
        };
        match tokio::time::timeout(cfg.send_timeout, connect).await {
            Ok(r) => r,
            Err(_) => Err(tr!("bridge.no_answer", timeout = format!("{:?}", cfg.send_timeout))),
        }
    }

    pub async fn send(&mut self, d: &Delivery, cfg: &Config, now_ms: i64) -> Result<Sent, String> {
        match self {
            Target::Amqp(t) => t.send(d, &cfg.shape, now_ms).await,
            Target::Mqtt(t) => t.send(d, &cfg.shape, now_ms).await,
            Target::Kafka(t) => t.send(d, &cfg.shape, now_ms).await,
            Target::Http(t) => t.send(d, &cfg.shape, now_ms).await,
        }
    }

    /// A close the broker may not answer: given `wait`, then dropped.
    pub async fn close(self, wait: Duration) {
        let _ = tokio::time::timeout(wait, async {
            match self {
                Target::Amqp(t) => t.close().await,
                Target::Mqtt(t) => t.close().await,
                Target::Kafka(_) | Target::Http(_) => {}
            }
        })
        .await;
    }
}

/// AWS-LC as the process's TLS provider, unless one is set: the AMQP client
/// brings AWS-LC and Kafka's ring, and with both rustls takes no default - its
/// `ClientConfig::builder()`, which `amqps://` and `mqtts://` use, would panic.
pub fn tls_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

/// `scheme://[user[:password]@]rest` as (user, password, rest), the user and
/// password percent-decoded; a `@` in a password is the last one's left.
pub(crate) fn split_url(url: &str) -> (Option<(String, String)>, String) {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let rest = rest.trim_end_matches('/');
    match rest.rsplit_once('@') {
        Some((cred, hosts)) => {
            let (u, p) = cred.split_once(':').unwrap_or((cred, ""));
            (Some((percent_decode(u), percent_decode(p))), hosts.to_string())
        }
        None => (None, rest.to_string()),
    }
}

/// `%XX` as its byte; anything else as it is.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            out.push((h * 16 + l) as u8);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [bridge-ca] `?ca=` on amqps:// and mqtts://: the URL without it for the client and
    /// the file; on a plain one, or any other option, refused; a file that does not read,
    /// or holds no certificate, refused at start
    #[test]
    fn a_ca_on_amqps_and_mqtts_and_nothing_else() {
        assert_eq!(tls_ca("amqps://app:pw@h:5671?ca=/etc/ca.pem").unwrap(),
            ("amqps://app:pw@h:5671".to_string(), Some("/etc/ca.pem".to_string())));
        assert_eq!(tls_ca("mqtts://h:8883?ca=/c.pem").unwrap(), ("mqtts://h:8883".to_string(), Some("/c.pem".to_string())));
        assert_eq!(tls_ca("mqtts://h:8883").unwrap(), ("mqtts://h:8883".to_string(), None));
        assert!(tls_ca("amqp://h:5672?ca=/c.pem").unwrap_err().contains("plain connection"));
        assert!(tls_ca("mqtts://h:8883?qos=2").unwrap_err().contains("only ca="));
        assert!(tls_ca("amqps://h:5671?ca=").is_err(), "an empty path");
        // a `?` in a password is the password's; the error never shows it
        assert_eq!(tls_ca("mqtts://app:ab?cd@h:8883?ca=/c.pem").unwrap(),
            ("mqtts://app:ab?cd@h:8883".to_string(), Some("/c.pem".to_string())));
        assert_eq!(tls_ca("mqtts://app:ab?cd@h:8883").unwrap(), ("mqtts://app:ab?cd@h:8883".to_string(), None));
        let e = tls_ca("amqps://app:ab?cd@h:5671?qos=2").unwrap_err();
        assert!(e.contains("\"qos\"") && !e.contains("ab?cd") && !e.contains("cd@"), "{}", e);
        assert!(!tls_ca("mqtts://h:8883?token=s3cret").unwrap_err().contains("s3cret"), "an option's value not shown");
        // an `@` in the path: %40, or refused - never read as no ca at all
        assert_eq!(tls_ca("amqps://app:pw@h:5671?ca=/etc/a%40b.pem").unwrap().1, Some("/etc/a@b.pem".to_string()));
        assert!(tls_ca("amqps://app:pw@h:5671?ca=/etc/a@b.pem").unwrap_err().contains("%40"));
        assert!(ca_roots("/nonexistent/ca.pem").unwrap_err().contains("/nonexistent/ca.pem"));
        let f = std::env::temp_dir().join(format!("etcds-ca-{}", std::process::id()));
        std::fs::write(&f, "not a certificate\n").unwrap();
        assert!(ca_roots(&f.display().to_string()).unwrap_err().contains("no certificate"));
        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn a_tls_default_once_both_providers_are_in() {
        tls_provider();
        tls_provider(); // twice is fine
        let _ = rustls::ClientConfig::builder(); // panics without a default
    }

    #[test]
    fn kinds_and_urls() {
        assert_eq!(kind("kafkas://h:9093"), Ok(Kind::Kafka));
        assert_eq!(kind("amqps://h:5671"), Ok(Kind::Amqp));
        assert_eq!(kind("mqtt://h"), Ok(Kind::Mqtt));
        assert_eq!(kind("kafka://h:9092,g:9092"), Ok(Kind::Kafka));
        assert_eq!(kind("http://h"), Ok(Kind::Http));
        assert_eq!(kind("https://h/x"), Ok(Kind::Http));
        assert!(kind("gopher://h").is_err());
        assert!(!kind("ftp://u:pw@h").unwrap_err().contains("pw"), "no password in the error");

        assert_eq!(split_url("kafka://u:p@h:1,g:2/"), (Some(("u".into(), "p".into())), "h:1,g:2".into()));
        assert_eq!(split_url("mqtt://h:1883"), (None, "h:1883".into()));
        assert_eq!(split_url("mqtt://u%3A1:p%25%40@h"), (Some(("u:1".into(), "p%@".into())), "h".into()));
        assert_eq!(percent_decode("100%"), "100%", "a bare % stays");
        assert_eq!(percent_decode("%zz%4"), "%zz%4");
    }
}
