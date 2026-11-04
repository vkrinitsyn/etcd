//! AMQP 1.0, for JMS: the message shape is [`crate::bridge::mapping`]. A delivery is
//! sent unsettled and the broker's outcome decides: `accepted` is taken, a
//! rejection as malformed or too large is for good, anything else is retried.

use std::time::Duration;

use fe2o3_amqp::types::definitions::{AmqpError, ErrorCondition, LinkError};
use fe2o3_amqp::types::messaging::{Outcome, Rejected, Target};
use fe2o3_amqp::types::primitives::Symbol;
use fe2o3_amqp::link::{SendError, delivery::DeliveryFut};
use fe2o3_amqp::{Connection, Sender, Session, connection::ConnectionHandle, session::SessionHandle};

use fe2o3_amqp::sasl_profile::SaslProfile;

use super::Sent;
use crate::bridge::Config;
use crate::bridge::mapping::{Delivery, Outgoing, Shape, to_amqp};

/// The AMQP idle timeout: a broker silent this long is taken as gone.
const AMQP_IDLE: Duration = Duration::from_secs(60);

pub struct Amqp {
    conn: ConnectionHandle<()>,
    session: SessionHandle<()>,
    sender: Sender,
}

/// [amqp-login] The URL's `user:password@`, percent-decoded, and the URL without it. A
/// user with no password is no login, as fe2o3 reads a URL (SASL ANONYMOUS, when offered)
fn login(url: &str) -> (Option<(String, String)>, String) {
    let (scheme, _) = url.split_once("://").unwrap_or(("amqp", url));
    match super::split_url(url) {
        (Some((u, p)), hosts) if !u.is_empty() && !p.is_empty() => (Some((u, p)), format!("{}://{}", scheme, hosts)),
        (_, hosts) => (None, format!("{}://{}", scheme, hosts)),
    }
}

impl Amqp {
    pub async fn connect(cfg: &Config) -> Result<Self, String> {
        let id = format!("etcds-bridge-{}", cfg.consumer);
        // [bridge-ca] `amqps://...?ca=<PEM file>`: that CA trusted, not the public roots
        let (url, ca) = super::tls_ca(&cfg.target_url)?;
        // [amqp-login] the login out of the URL, decoded: fe2o3 sends the URL's password
        // as it is written, so a `%40` went as `%40` and an `@` as it is broke the URL
        let (login, url) = login(&url);
        // [amqps-tls] `amqps://` (port 5671) is TLS from the first byte, as Artemis,
        // RabbitMQ and Azure Service Bus serve it (AMQP 1.0 core 5.2.1, the alternative
        // establishment); the default sends the AMQP TLS header first, which such a
        // broker drops as a failed handshake ("early eof"). Plain `amqp://` is unaffected.
        let builder = Connection::builder()
            .container_id(id.clone())
            .idle_time_out(AMQP_IDLE.as_millis() as u32)
            .alt_tls_establishment(true);
        let builder = match login {
            Some((username, password)) => builder.sasl_profile(SaslProfile::Plain { username, password }),
            None => builder,
        };
        let mut conn = match ca {
            Some(path) => {
                let config = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
                    .with_safe_default_protocol_versions()
                    .map_err(|e| e.to_string())?
                    .with_root_certificates(super::ca_roots(&path)?)
                    .with_no_client_auth();
                builder.rustls_connector(tokio_rustls::TlsConnector::from(std::sync::Arc::new(config)))
                    .open(url.as_str()).await
            }
            None => builder.open(url.as_str()).await,
        }
        .map_err(|e| e.to_string())?;
        let mut session = Session::begin(&mut conn).await.map_err(|e| e.to_string())?;
        // "queue": a broker that creates the address on demand (Artemis)
        // makes it anycast, as for a JMS queue, not a topic nobody subscribed to
        let target = Target::builder()
            .address(cfg.address.clone())
            .capabilities(vec![Symbol::from("queue")])
            .build();
        let sender = Sender::builder()
            .name(id)
            .target(target)
            .attach(&mut session)
            .await
            .map_err(|e| e.to_string())?;
        Ok(Amqp { conn, session, sender })
    }

    pub async fn send(&mut self, d: &Delivery, shape: &Shape, now_ms: i64) -> Result<Sent, String> {
        let outcome = match to_amqp(d, shape, now_ms) {
            Ok(Outgoing::Text(m)) => self.sender.send(m).await,
            Ok(Outgoing::Bytes(m)) => self.sender.send(m).await,
            Err(e) => return Ok(Sent::Permanent(e)),
        }
        .map_err(|e| e.to_string())?;
        Ok(sent(outcome))
    }

    pub async fn close(mut self) {
        let _ = self.sender.close().await;
        let _ = self.session.end().await;
        let _ = self.conn.close().await;
    }
}



/// The broker's or client's outcome as the bridge takes it.
pub fn sent(outcome: Outcome) -> Sent {
    match outcome {
        Outcome::Accepted(_) => Sent::Accepted,
        Outcome::Rejected(r) if permanent(&r) => Sent::Permanent(tr!("bridge.rejected", err = format!("{:?}", r.error))),
        Outcome::Rejected(r) => Sent::Retry(tr!("bridge.rejected_now", err = format!("{:?}", r.error))),
        other => Sent::Retry(tr!("bridge.not_taken", err = format!("{:?}", other))),
    }
}

/// The outcome of one message on a client's link ([`crate::listener`]), still
/// to come: the client's disposition.
pub type Pending = DeliveryFut<Result<Outcome, SendError>>;

/// Sends on a client's link without waiting for the outcome: the transfer is
/// out when this returns. `Ok(Err(_))` is a message that cannot take the shape.
pub async fn send_unsettled(sender: &mut Sender, d: &Delivery, shape: &Shape, now_ms: i64) -> Result<Result<Pending, String>, String> {
    let sent = match to_amqp(d, shape, now_ms) {
        Ok(Outgoing::Text(m)) => sender.send_batchable(m).await,
        Ok(Outgoing::Bytes(m)) => sender.send_batchable(m).await,
        Err(e) => return Ok(Err(e)),
    };
    sent.map(Ok).map_err(|e| e.to_string())
}

/// A rejection no resend will change: the message as it is cannot go in.
pub fn permanent(r: &Rejected) -> bool {
    matches!(
        r.error.as_ref().map(|e| &e.condition),
        Some(ErrorCondition::AmqpError(AmqpError::DecodeError | AmqpError::InvalidField | AmqpError::NotImplemented))
            | Some(ErrorCondition::LinkError(LinkError::MessageSizeExceeded))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use fe2o3_amqp::types::definitions::Error as AmqpErr;

    #[test]
    fn only_a_malformed_message_is_rejected_for_good() {
        let r = |c: Option<ErrorCondition>| Rejected { error: c.map(|c| AmqpErr::new(c, None, None)) };
        assert!(permanent(&r(Some(AmqpError::DecodeError.into()))));
        assert!(permanent(&r(Some(LinkError::MessageSizeExceeded.into()))));
        assert!(!permanent(&r(Some(AmqpError::ResourceLimitExceeded.into()))), "a full address frees up");
        assert!(!permanent(&r(Some(AmqpError::UnauthorizedAccess.into()))));
        assert!(!permanent(&r(None)), "no reason given: retried");
    }

    /// [amqp-login] What fe2o3 is given: the login decoded, the URL without it
    #[test]
    fn an_amqp_login_is_decoded_and_out_of_the_url() {
        assert_eq!(login("amqps://app:p%40ss%3Fw%25rd@h:5671"),
            (Some(("app".to_string(), "p@ss?w%rd".to_string())), "amqps://h:5671".to_string()));
        assert_eq!(login("amqp://app:a@b@h:5672"), (Some(("app".to_string(), "a@b".to_string())), "amqp://h:5672".to_string()),
            "an @ in the password: the host is after the last one");
        assert_eq!(login("amqp://h:5672"), (None, "amqp://h:5672".to_string()));
        assert_eq!(login("amqp://app@h:5672"), (None, "amqp://h:5672".to_string()), "no password: no login");
        assert_eq!(login("amqp://u%3Ab:x@h:5672").0, Some(("u:b".to_string(), "x".to_string())));
    }

    /// A CA for the test below only: its key was thrown away when it was made
    const TEST_CA: &str = "-----BEGIN CERTIFICATE-----
MIIBhzCCAS2gAwIBAgIUMjIR90zspukRt8KuavNoChqJM+owCgYIKoZIzj0EAwIw
GDEWMBQGA1UEAwwNZXRjZHMtdGVzdC1jYTAgFw0yNjExMDEwNDQzNTBaGA8yMTI2
MTAwODA0NDM1MFowGDEWMBQGA1UEAwwNZXRjZHMtdGVzdC1jYTBZMBMGByqGSM49
AgEGCCqGSM49AwEHA0IABGDopVX/Beo1fFh48CFmAaFfUgBvyIAyPKF+33xlhhsK
WyqPDkLoe+veXogZW0HYOjgoUVPCqs7nqZL/9eB1t7KjUzBRMB0GA1UdDgQWBBQV
rJDWQ6KEGu/Ql2fkuuP7xRLNVjAfBgNVHSMEGDAWgBQVrJDWQ6KEGu/Ql2fkuuP7
xRLNVjAPBgNVHRMBAf8EBTADAQH/MAoGCCqGSM49BAMCA0gAMEUCIQCr3E90eBm/
mi9JvQlVX0w4Fq9Hf6iu+qpOi8sUN0/KHAIgExLgAQ8ZyonRQMKC7eXSFw4X92Xt
Du0hcjhXu39ZvTA=
-----END CERTIFICATE-----
";

    /// [amqps-tls] `amqps://` is TLS from its first byte - a handshake record (0x16),
    /// not the AMQP TLS header `AMQP\x02`, which a pure-TLS broker drops ("early eof"
    /// against Artemis) - with the public roots and with `?ca=`
    #[tokio::test]
    async fn amqps_is_tls_from_the_first_byte() {
        use tokio::io::AsyncReadExt;
        super::super::tls_provider();
        let pem = std::env::temp_dir().join(format!("etcds-amqps-ca-{}", std::process::id()));
        std::fs::write(&pem, TEST_CA).unwrap();
        for query in [String::new(), format!("?ca={}", pem.display())] {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("amqps://app:pw@127.0.0.1:{}{}", l.local_addr().unwrap().port(), query);
            let first = tokio::spawn(async move {
                let (mut s, _) = l.accept().await.unwrap();
                let mut b = [0u8; 1];
                s.read_exact(&mut b).await.unwrap();
                b[0]
            });
            let cfg = Config {
                etcd: vec!["http://localhost:2379".into()],
                etcd_user: None,
                queue: "jobs".into(),
                prefix: None,
                consumer: uuid::Uuid::new_v4(),
                target_url: url.clone(),
                address: "jobs".into(),
                shape: Shape::default(),
                dead_letter: None,
                buffer: 64,
                send_timeout: Duration::from_secs(5),
                node: None,
                policy: None,
            };
            let r = tokio::time::timeout(Duration::from_secs(5), Amqp::connect(&cfg)).await;
            assert!(!matches!(r, Ok(Ok(_))), "{}: nothing answered, so no connection", url);
            let b = tokio::time::timeout(Duration::from_secs(5), first).await.expect("a first byte").unwrap();
            assert_eq!(b, 0x16, "{}: a TLS handshake first, got {:#04x} ('A' is the AMQP header)", url, b);
        }
        let _ = std::fs::remove_file(&pem);
    }
}
