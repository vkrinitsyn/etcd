//! Kafka: each delivery is one record on the topic `address`, produced with
//! `acks=all` and taken when the leader answers with its offset.
//!
//! - The record key is the message key, and the partition is the one Kafka's
//!   own producers pick for that key (murmur2, the default partitioner), so a
//!   Java producer and the bridge put one key in one partition.
//! - The value is the record value; the timestamp is when the bridge sent it.
//! - Headers: `etcd_queue`, `etcd_key`, `etcd_index`, `etcd_delivery_key`,
//!   `message_id` and `content-type` - or, with CloudEvents, each attribute as
//!   `ce_{name}` (the CloudEvents Kafka binding, binary mode),
//!   `datacontenttype` being `content-type`.
//!
//! The topic must exist: connecting reads its partitions, and a topic not
//! found is tried again. They are read again every `metadata_max_age_ms` (5
//! minutes, as Kafka's producers), and on every connect. A broker answer that
//! rskafka retries - a moved leader, an unknown partition - and that does not
//! pass within its deadline (two thirds of the send timeout) ends the
//! connection, with the broker's error in the log; the bridge connects again.
//! A record too large or invalid is refused for good; any other broker error is
//! retried.
//!
//! [`KafkaUrl`]: `kafka://` (PLAINTEXT) or `kafkas://` (SSL), then
//! `host:port[,host:port][?options]`. No login: a listener with SASL is not
//! for the bridge.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rskafka::chrono::{DateTime, Utc};
use rskafka::client::error::{Error as KafkaError, ProtocolError};
use rskafka::client::partition::{Compression, PartitionClient, UnknownTopicHandling};
use rskafka::BackoffConfig;
use rskafka::client::{Client, ClientBuilder};
use rskafka::record::Record;
use rustls_pki_types::CertificateDer;
use rustls_pki_types::pem::PemObject;

use super::{Sent, split_url};
use crate::bridge::Config;
use crate::bridge::cloudevents;
use crate::bridge::mapping::{Delivery, KvEvent, Shape, body_text};

/// How often the topic's partitions are read again, unless the URL says.
const METADATA_MAX_AGE: Duration = Duration::from_secs(300);

/// A Kafka target URL, `kafka[s]://host:port[,host:port][?options]`:
///
/// | option | values | default |
/// |---|---|---|
/// | `ca` | a PEM file of the CAs to trust (`kafkas://`) | the public roots |
/// | `metadata_max_age_ms` | how often the topic's partitions are read again | 300000 |
///
/// Brokers are `host:port`, IPv4 or a name: rskafka cannot reach an IPv6
/// literal. A user in the URL is refused: the bridge does not log in to Kafka.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KafkaUrl {
    pub tls: bool,
    pub brokers: Vec<String>,
    pub ca: Option<String>,
    pub metadata_max_age: Duration,
}

impl KafkaUrl {
    pub fn parse(url: &str) -> Result<Self, String> {
        let tls = url.starts_with("kafkas://");
        let (user, rest) = split_url(url);
        if user.is_some() {
            return Err("a user in the Kafka URL: the bridge does not log in to Kafka".into());
        }
        let (hosts, query) = rest.split_once('?').unwrap_or((rest.as_str(), ""));
        let brokers: Vec<String> = hosts.split(',').filter(|h| !h.is_empty()).map(str::to_string).collect();
        if brokers.is_empty() {
            return Err("no Kafka broker".into());
        }
        for b in &brokers {
            if b.starts_with('[') {
                return Err(format!("broker {}: an IPv6 literal, which rskafka cannot reach - a name for it", b));
            }
            match b.rsplit_once(':') {
                Some((h, p)) if !h.is_empty() && !h.contains(['/', ':']) && p.parse::<u16>().is_ok() => {}
                _ => return Err(format!("broker {:?}: host:port", b)),
            }
        }
        let mut ca = None;
        let mut metadata_max_age = METADATA_MAX_AGE;
        for kv in query.split('&').filter(|kv| !kv.is_empty()) {
            match kv.split_once('=') {
                Some(("ca", path)) if !path.is_empty() => ca = Some(path.to_string()),
                Some(("metadata_max_age_ms", ms)) => match ms.parse::<u64>() {
                    Ok(ms) if ms > 0 => metadata_max_age = Duration::from_millis(ms),
                    _ => return Err("metadata_max_age_ms=: milliseconds, above 0".into()),
                },
                _ => return Err(format!("Kafka option {:?}: ca= or metadata_max_age_ms=", kv)),
            }
        }
        if ca.is_some() && !tls {
            return Err("ca= is for kafkas://".into());
        }
        Ok(KafkaUrl { tls, brokers, ca, metadata_max_age })
    }

    /// A client builder for these brokers and TLS. rskafka retries a connect
    /// and a retriable broker answer for `deadline`, then gives the last
    /// error; without one it retries for ever, and an unknown host or an
    /// untrusted certificate is only a timeout.
    pub fn client_builder(&self, client_id: String, deadline: Duration) -> Result<ClientBuilder, String> {
        let backoff = BackoffConfig { deadline: Some(deadline), ..BackoffConfig::default() };
        let mut b = ClientBuilder::new(self.brokers.clone()).client_id(client_id).backoff_config(backoff);
        if self.tls {
            b = b.tls_config(self.tls_config()?);
        }
        Ok(b)
    }

    /// TLS trusting `ca`, or the public roots - with AWS-LC named: the AMQP
    /// client brings it and rskafka ring, and rustls takes no default then.
    fn tls_config(&self) -> Result<Arc<rustls::ClientConfig>, String> {
        let mut roots = rustls::RootCertStore::empty();
        match &self.ca {
            Some(path) => {
                for cert in CertificateDer::pem_file_iter(path).map_err(|e| format!("ca {}: {}", path, e))? {
                    roots.add(cert.map_err(|e| format!("ca {}: {}", path, e))?).map_err(|e| format!("ca {}: {}", path, e))?;
                }
                if roots.is_empty() {
                    return Err(format!("ca {}: no certificate in it", path));
                }
            }
            None => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
        }
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| e.to_string())?
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(Arc::new(config))
    }
}

pub struct Kafka {
    client: Client,
    topic: String,
    partitions: Vec<i32>,
    read_at: Instant,
    max_age: Duration,
    clients: HashMap<i32, PartitionClient>,
}

impl Kafka {
    pub async fn connect(cfg: &Config) -> Result<Self, String> {
        let url = KafkaUrl::parse(&cfg.target_url)?;
        let deadline = (cfg.send_timeout * 2 / 3).max(Duration::from_millis(500));
        let b = url.client_builder(format!("etcds-bridge-{}", cfg.consumer), deadline)?;
        let client = b.build().await.map_err(|e| e.to_string())?;
        let partitions = partitions(&client, &cfg.address).await?;
        Ok(Kafka {
            client,
            topic: cfg.address.clone(),
            partitions,
            read_at: Instant::now(),
            max_age: url.metadata_max_age,
            clients: HashMap::new(),
        })
    }

    pub async fn send(&mut self, d: &Delivery, shape: &Shape, now_ms: i64) -> Result<Sent, String> {
        let text = match body_text(d, shape.body) {
            Ok(t) => t.is_some(),
            Err(e) => return Ok(Sent::Permanent(e)),
        };
        if self.read_at.elapsed() > self.max_age {
            self.refresh().await?;
        }
        let partition = self.partitions[partition_of(d.key.as_bytes(), self.partitions.len())];
        if !self.clients.contains_key(&partition) {
            let pc = self
                .client
                .partition_client(self.topic.clone(), partition, UnknownTopicHandling::Retry)
                .await
                .map_err(|e| e.to_string())?;
            self.clients.insert(partition, pc);
        }
        let record = Record {
            key: Some(d.key.as_bytes().to_vec()),
            // a key's delete is a tombstone
            value: if d.event == Some(KvEvent::Delete) { None } else { Some(d.value.clone()) },
            headers: headers(d, shape, text, now_ms),
            timestamp: DateTime::<Utc>::from_timestamp_millis(now_ms).unwrap_or_default(),
        };
        match self.clients[&partition].produce(vec![record], Compression::NoCompression).await {
            Ok(_) => Ok(Sent::Accepted),
            Err(KafkaError::ServerError { protocol_error, .. }) if permanent(&protocol_error) => {
                Ok(Sent::Permanent(format!("{:?}", protocol_error)))
            }
            Err(KafkaError::ServerError { protocol_error, .. }) => Ok(Sent::Retry(format!("{:?}", protocol_error))),
            // rskafka's retries gave up - a moved leader, a partition gone, the
            // broker away: connect again, which reads the partitions again
            Err(e) => Err(e.to_string()),
        }
    }

    /// The topic's partitions read again; a changed count drops the partition
    /// clients, and the keys go where Kafka's producers now put them.
    async fn refresh(&mut self) -> Result<(), String> {
        let now = partitions(&self.client, &self.topic).await?;
        if now != self.partitions {
            self.clients.clear();
            self.partitions = now;
        }
        self.read_at = Instant::now();
        Ok(())
    }
}

async fn partitions(client: &Client, topic: &str) -> Result<Vec<i32>, String> {
    let topics = client.list_topics().await.map_err(|e| e.to_string())?;
    let partitions: Vec<i32> = topics
        .into_iter()
        .find(|t| t.name == topic)
        .map(|t| t.partitions.into_iter().collect())
        .unwrap_or_default();
    if partitions.is_empty() {
        return Err(format!("topic {} not found", topic));
    }
    Ok(partitions)
}

fn permanent(e: &ProtocolError) -> bool {
    matches!(
        e,
        // not CorruptMessage: Java's producer retries it, a CRC broken on the way
        ProtocolError::MessageTooLarge
            | ProtocolError::InvalidRecord
            | ProtocolError::RecordListTooLarge
    )
}

fn headers(d: &Delivery, shape: &Shape, text: bool, now_ms: i64) -> BTreeMap<String, Vec<u8>> {
    let content_type = if text { cloudevents::TEXT } else { cloudevents::BYTES };
    let mut h = BTreeMap::new();
    h.insert("content-type".to_string(), content_type.as_bytes().to_vec());
    if shape.cloudevents {
        for (k, v) in cloudevents::attributes(d, &shape.ce_type, text, now_ms) {
            if k != "datacontenttype" {
                h.insert(format!("ce_{}", k), v.into_bytes());
            }
        }
    } else {
        for (k, v) in d.properties() {
            h.insert(k.to_string(), v.to_string().into_bytes());
        }
        h.insert("message_id".into(), d.event_id().as_bytes().to_vec());
    }
    h
}

/// The partition index Kafka's default partitioner gives a key.
pub fn partition_of(key: &[u8], partitions: usize) -> usize {
    ((murmur2(key) & 0x7fff_ffff) as usize) % partitions
}

/// Kafka's `Utils.murmur2`.
pub fn murmur2(data: &[u8]) -> i32 {
    const SEED: u32 = 0x9747_b28c;
    const M: u32 = 0x5bd1_e995;
    const R: u32 = 24;
    let len = data.len();
    let mut h: u32 = SEED ^ len as u32;
    let (chunks, tail) = data.as_chunks::<4>();
    for c in chunks {
        let mut k = u32::from_le_bytes(*c);
        k = k.wrapping_mul(M);
        k ^= k >> R;
        k = k.wrapping_mul(M);
        h = h.wrapping_mul(M);
        h ^= k;
    }
    if tail.len() == 3 {
        h ^= (tail[2] as u32) << 16;
    }
    if tail.len() >= 2 {
        h ^= (tail[1] as u32) << 8;
    }
    if !tail.is_empty() {
        h ^= tail[0] as u32;
        h = h.wrapping_mul(M);
    }
    h ^= h >> 13;
    h = h.wrapping_mul(M);
    h ^= h >> 15;
    h as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Kafka's own vectors (`UtilsTest.testMurmur2`).
    #[test]
    fn murmur2_as_kafka() {
        assert_eq!(murmur2(b"21"), -973932308);
        assert_eq!(murmur2(b"foobar"), -790332482);
        assert_eq!(murmur2(b"a-little-bit-long-string"), -985981536);
        assert_eq!(murmur2(b"a-little-bit-longer-string"), -1486304829);
        assert_eq!(murmur2(b"lkjh234lh9fiuh90y23oiuhsafujhadof229phr9h19h89h8"), -58897971);
        assert_eq!(murmur2(b"abc"), 479470107);
        assert!(partition_of(b"21", 3) < 3);
    }

    #[test]
    fn record_headers() {
        let d = Delivery::new("jobs", b"/q/jobs/3/k", &[0xff]);
        let h = headers(&d, &Shape::default(), false, 0);
        assert_eq!(h["content-type"], cloudevents::BYTES.as_bytes());
        assert_eq!(h["etcd_index"], b"3");
        assert_eq!(h["message_id"], d.event_id().as_bytes());

        let ce = Shape { cloudevents: true, ..Shape::default() };
        let h = headers(&d, &ce, false, 0);
        assert_eq!(h["ce_specversion"], b"1.0");
        assert_eq!(h["ce_id"], d.event_id().as_bytes());
        assert_eq!(h["ce_subject"], b"k");
        assert!(!h.contains_key("ce_datacontenttype"));
        assert!(!h.contains_key("etcd_key"));
    }

    #[test]
    fn kafka_urls() {
        let u = KafkaUrl::parse("kafka://h1:9092,h2:9092").unwrap();
        assert_eq!((u.tls, u.brokers.len()), (false, 2));
        let u = KafkaUrl::parse("kafkas://h:9093?ca=/etc/ca.pem").unwrap();
        assert_eq!((u.tls, u.ca.as_deref()), (true, Some("/etc/ca.pem")));
        assert_eq!(KafkaUrl::parse("kafka://h:1").unwrap().metadata_max_age, METADATA_MAX_AGE);
        assert_eq!(KafkaUrl::parse("kafka://h:1?metadata_max_age_ms=1500").unwrap().metadata_max_age, Duration::from_millis(1500));

        // no login: a user is refused, and not echoed
        let e = KafkaUrl::parse("kafka://app:secret@h:9092").unwrap_err();
        assert!(e.contains("does not log in") && !e.contains("secret"), "{}", e);
        for bad in ["kafka://", "kafka://host", "kafka://h:1?sasl=plain",
                    "kafka://[::1]:9092", "kafka://h:", "kafka://h:abc", "kafka://h:9092/topic", "kafka://:9092",
                    "kafka://h:1?ca=/x.pem", "kafka://h:1?acks=1", "kafkas://h:1?ca=",
                    "kafka://h:1?metadata_max_age_ms=0", "kafka://h:1?metadata_max_age_ms=soon"] {
            assert!(KafkaUrl::parse(bad).is_err(), "{}", bad);
        }
    }

    #[test]
    fn tls_trusts_the_given_ca_or_the_public_roots() {
        let public = KafkaUrl::parse("kafkas://h:9093").unwrap();
        assert!(public.tls_config().is_ok());
        let missing = KafkaUrl::parse("kafkas://h:9093?ca=/nonexistent/ca.pem").unwrap();
        assert!(missing.tls_config().unwrap_err().contains("/nonexistent/ca.pem"));
        let dir = std::env::temp_dir().join(format!("e2j-ca-{}", std::process::id()));
        std::fs::write(&dir, "not a certificate\n").unwrap();
        let empty = KafkaUrl::parse(&format!("kafkas://h:9093?ca={}", dir.display())).unwrap();
        assert!(empty.tls_config().unwrap_err().contains("no certificate"));
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn refused_for_good() {
        assert!(permanent(&ProtocolError::MessageTooLarge));
        assert!(!permanent(&ProtocolError::NotLeaderOrFollower));
        assert!(!permanent(&ProtocolError::CorruptMessage), "retried, as Java's producer does");
    }
}
