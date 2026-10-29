//! MQTT 5: each delivery is published QoS 1 to `{address}/{key}` and taken
//! when the broker's PUBACK says so.
//!
//! - The key is one topic level: `%`, `+`, `#` and `/` in it are written
//!   `%25`, `%2B`, `%23`, `%2F`, so it can be neither a wildcard nor a level
//!   more.
//! - The value is the payload; a text value has the payload format indicator 1
//!   and `text/plain; charset=utf-8`, bytes `application/octet-stream`, as the
//!   content type.
//! - User properties: `etcd_queue`, `etcd_key`, `etcd_index`,
//!   `etcd_delivery_key` and `message_id` - or, with CloudEvents, each
//!   attribute under its own name (the CloudEvents MQTT binding, binary mode),
//!   `datacontenttype` being the content type.
//!
//! PUBACK `success` and `no matching subscribers` are taken: MQTT is not a
//! queue, and a message nobody subscribed to is gone - subscribe with a
//! persistent session (clean start off) before the bridge publishes. `topic
//! name invalid`, `payload format invalid` and a packet over the broker's
//! maximum are for good; any other reason is retried.

use std::time::Duration;

use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::mqttbytes::v5::{Packet, PubAckReason, PublishProperties};
use rumqttc::v5::{AsyncClient, Event, MqttOptions};
use rumqttc::{TlsConfiguration, Transport};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::{Sent, split_url};
use crate::bridge::Config;
use crate::bridge::cloudevents;
use crate::bridge::mapping::{Delivery, Shape, body_text};

const KEEP_ALIVE: Duration = Duration::from_secs(30);

pub struct Mqtt {
    client: AsyncClient,
    events: mpsc::Receiver<Result<Event, String>>,
    poll: JoinHandle<()>,
    address: String,
}

impl Mqtt {
    pub async fn connect(cfg: &Config) -> Result<Self, String> {
        let (user, hostport) = split_url(&cfg.target_url);
        let tls = cfg.target_url.starts_with("mqtts://");
        let (host, port) = match hostport.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse::<u16>().map_err(|e| format!("port {:?}: {}", p, e))?),
            None => (hostport.clone(), if tls { 8883 } else { 1883 }),
        };
        let mut opts = MqttOptions::new(format!("etcds-bridge-{}", cfg.consumer), host, port);
        opts.set_keep_alive(KEEP_ALIVE).set_clean_start(true);
        if let Some((u, p)) = user {
            opts.set_credentials(u, p);
        }
        if tls {
            // native-tls: the system's TLS and trusted roots
            opts.set_transport(Transport::Tls(TlsConfiguration::Native));
        }
        let (client, mut eventloop) = AsyncClient::new(opts, 10);
        let (tx, mut events) = mpsc::channel(16);
        // the event loop is the connection: it runs while it is polled, and
        // stops at its first error - the bridge connects again. Only what
        // the bridge waits for is passed on: pings and the like would fill
        // the channel while the bridge is idle, and stop the polling
        let poll = tokio::spawn(async move {
            loop {
                let ev = match eventloop.poll().await {
                    Ok(ev @ Event::Incoming(Packet::ConnAck(_) | Packet::PubAck(_))) => Ok(ev),
                    Ok(Event::Incoming(Packet::Disconnect(d))) => Err(format!("the broker disconnected: {:?}", d.reason_code)),
                    Ok(_) => continue,
                    Err(e) => Err(e.to_string()),
                };
                let stop = ev.is_err();
                if tx.send(ev).await.is_err() || stop {
                    break;
                }
            }
        });
        loop {
            match events.recv().await {
                Some(Ok(Event::Incoming(Packet::ConnAck(_)))) => break,
                Some(Ok(_)) => continue,
                Some(Err(e)) => {
                    poll.abort();
                    return Err(e);
                }
                None => return Err("the connection closed".into()),
            }
        }
        Ok(Mqtt { client, events, poll, address: cfg.address.clone() })
    }

    pub async fn send(&mut self, d: &Delivery, shape: &Shape, now_ms: i64) -> Result<Sent, String> {
        let text = match body_text(d, shape.body) {
            Ok(t) => t.is_some(),
            Err(e) => return Ok(Sent::Permanent(e)),
        };
        let topic = topic(&self.address, &d.key);
        if let Err(why) = valid_topic(&topic) {
            return Ok(Sent::Permanent(why));
        }
        let props = properties(d, shape, text, now_ms);
        self.client
            .publish_with_properties(topic, QoS::AtLeastOnce, false, d.value.clone(), props)
            .await
            .map_err(|e| e.to_string())?;
        // one message in flight at a time: the next PUBACK is this one's
        loop {
            match self.events.recv().await {
                Some(Ok(Event::Incoming(Packet::PubAck(ack)))) => return Ok(puback(ack.reason)),
                Some(Ok(_)) => continue,
                // this message over the broker's limit - not the client's limit
                // on what it receives
                Some(Err(e)) if e.contains("broker's maximum packet size") => return Ok(Sent::Permanent(e)),
                Some(Err(e)) => return Err(e),
                None => return Err("the connection closed".into()),
            }
        }
    }

    pub async fn close(mut self) {
        let _ = self.client.disconnect().await;
        // let the DISCONNECT go out; dropping stops the polling
        let _ = tokio::time::timeout(Duration::from_secs(1), async {
            while let Some(Ok(_)) = self.events.recv().await {}
        })
        .await;
    }
}

impl Drop for Mqtt {
    fn drop(&mut self) {
        self.poll.abort();
    }
}

fn puback(reason: PubAckReason) -> Sent {
    match reason {
        PubAckReason::Success | PubAckReason::NoMatchingSubscribers => Sent::Accepted,
        PubAckReason::TopicNameInvalid | PubAckReason::PayloadFormatInvalid => Sent::Permanent(format!("PUBACK {:?}", reason)),
        other => Sent::Retry(format!("PUBACK {:?}", other)),
    }
}

/// What MQTT refuses in a topic name (MQTT 5, 4.7.3 and 1.5.4), which a
/// broker answers by dropping the connection rather than with a PUBACK.
pub fn valid_topic(t: &str) -> Result<(), String> {
    if t.len() > u16::MAX as usize {
        return Err(format!("topic of {} bytes: at most {}", t.len(), u16::MAX));
    }
    if t.contains('\0') {
        return Err("a NUL in the topic".into());
    }
    Ok(())
}

/// `{address}/{key}`, the key one level.
pub fn topic(address: &str, key: &str) -> String {
    let mut t = String::with_capacity(address.len() + key.len() + 1);
    t.push_str(address);
    t.push('/');
    for c in key.chars() {
        match c {
            '%' => t.push_str("%25"),
            '+' => t.push_str("%2B"),
            '#' => t.push_str("%23"),
            '/' => t.push_str("%2F"),
            c => t.push(c),
        }
    }
    t
}

fn properties(d: &Delivery, shape: &Shape, text: bool, now_ms: i64) -> PublishProperties {
    let mut user = Vec::new();
    let content_type = if shape.cloudevents {
        for (k, v) in cloudevents::attributes(d, &shape.ce_type, text, now_ms) {
            if k != "datacontenttype" {
                user.push((k.to_string(), v));
            }
        }
        if text { cloudevents::TEXT } else { cloudevents::BYTES }
    } else {
        for (k, v) in d.properties() {
            user.push((k.to_string(), v.to_string()));
        }
        user.push(("message_id".into(), d.event_id().to_string()));
        if text { cloudevents::TEXT } else { cloudevents::BYTES }
    };
    PublishProperties {
        payload_format_indicator: Some(text as u8),
        content_type: Some(content_type.to_string()),
        user_properties: user,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_is_one_level() {
        assert_eq!(topic("etcd/jobs", "job-1"), "etcd/jobs/job-1");
        assert_eq!(topic("a", "x+y#z/w%"), "a/x%2By%23z%2Fw%25");
        assert!(valid_topic("a/b").is_ok());
        assert!(valid_topic("a/\0").is_err());
        assert!(valid_topic(&"x".repeat(70_000)).is_err());
    }

    #[test]
    fn puback_reasons() {
        assert_eq!(puback(PubAckReason::Success), Sent::Accepted);
        assert_eq!(puback(PubAckReason::NoMatchingSubscribers), Sent::Accepted);
        assert!(matches!(puback(PubAckReason::PayloadFormatInvalid), Sent::Permanent(_)));
        assert!(matches!(puback(PubAckReason::TopicNameInvalid), Sent::Permanent(_)));
        assert!(matches!(puback(PubAckReason::QuotaExceeded), Sent::Retry(_)));
        assert!(matches!(puback(PubAckReason::NotAuthorized), Sent::Retry(_)));
    }

    #[test]
    fn user_properties() {
        let d = Delivery::new("jobs", b"/q/jobs/3/k", b"hi");
        let p = properties(&d, &Shape::default(), true, 0);
        assert_eq!(p.payload_format_indicator, Some(1));
        assert_eq!(p.content_type.as_deref(), Some(cloudevents::TEXT));
        assert!(p.user_properties.contains(&("etcd_index".into(), "3".into())));
        assert!(p.user_properties.contains(&("message_id".into(), d.event_id().into())));

        let ce = Shape { cloudevents: true, ..Shape::default() };
        let p = properties(&d, &ce, false, 0);
        assert_eq!(p.payload_format_indicator, Some(0));
        assert_eq!(p.content_type.as_deref(), Some(cloudevents::BYTES));
        assert!(p.user_properties.contains(&("specversion".into(), "1.0".into())));
        assert!(p.user_properties.contains(&("id".into(), d.event_id().into())));
        assert!(p.user_properties.iter().all(|(k, _)| k != "datacontenttype"));
    }
}
