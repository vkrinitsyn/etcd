//! Where the bridge sends: an AMQP 1.0 broker (JMS), an MQTT 5 broker or Kafka,
//! by the scheme of the target URL. Each tells the bridge, per message, whether
//! the broker took it ([`Sent`]); an `Err` is a connection that no longer
//! works, and is made again.

pub mod amqp;
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
}

/// The target's kind, by the URL's scheme.
pub fn kind(url: &str) -> Result<Kind, String> {
    match url.split_once("://").map(|(s, _)| s) {
        Some("amqp" | "amqps") => Ok(Kind::Amqp),
        Some("mqtt" | "mqtts") => Ok(Kind::Mqtt),
        Some("kafka" | "kafkas") => Ok(Kind::Kafka),
        _ => Err(format!("{}: amqp://, amqps://, mqtt://, mqtts://, kafka:// or kafkas://", crate::bridge::redact(url))),
    }
}

pub enum Target {
    /// boxed: a connection, session and link are large beside the others
    Amqp(Box<amqp::Amqp>),
    Mqtt(mqtt::Mqtt),
    Kafka(kafka::Kafka),
}

impl Target {
    pub async fn connect(cfg: &Config) -> Result<Self, String> {
        tls_provider();
        let connect = async {
            Ok(match kind(&cfg.target_url)? {
                Kind::Amqp => Target::Amqp(Box::new(amqp::Amqp::connect(cfg).await?)),
                Kind::Mqtt => Target::Mqtt(mqtt::Mqtt::connect(cfg).await?),
                Kind::Kafka => Target::Kafka(kafka::Kafka::connect(cfg).await?),
            })
        };
        match tokio::time::timeout(cfg.send_timeout, connect).await {
            Ok(r) => r,
            Err(_) => Err(format!("no answer in {:?}", cfg.send_timeout)),
        }
    }

    pub async fn send(&mut self, d: &Delivery, cfg: &Config, now_ms: i64) -> Result<Sent, String> {
        match self {
            Target::Amqp(t) => t.send(d, &cfg.shape, now_ms).await,
            Target::Mqtt(t) => t.send(d, &cfg.shape, now_ms).await,
            Target::Kafka(t) => t.send(d, &cfg.shape, now_ms).await,
        }
    }

    /// A close the broker may not answer: given `wait`, then dropped.
    pub async fn close(self, wait: Duration) {
        let _ = tokio::time::timeout(wait, async {
            match self {
                Target::Amqp(t) => t.close().await,
                Target::Mqtt(t) => t.close().await,
                Target::Kafka(_) => {}
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
        assert!(kind("http://h").is_err());
        assert!(!kind("ftp://u:pw@h").unwrap_err().contains("pw"), "no password in the error");

        assert_eq!(split_url("kafka://u:p@h:1,g:2/"), (Some(("u".into(), "p".into())), "h:1,g:2".into()));
        assert_eq!(split_url("mqtt://h:1883"), (None, "h:1883".into()));
        assert_eq!(split_url("mqtt://u%3A1:p%25%40@h"), (Some(("u:1".into(), "p%@".into())), "h".into()));
        assert_eq!(percent_decode("100%"), "100%", "a bare % stays");
        assert_eq!(percent_decode("%zz%4"), "%zz%4");
    }
}
