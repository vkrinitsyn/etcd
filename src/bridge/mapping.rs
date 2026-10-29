//! An etcd queue delivery as an AMQP 1.0 message a JMS client reads.
//!
//! The JMS side follows the AMQP JMS Mapping (OASIS amqp-bindmap-jms v1.0), the
//! one Qpid JMS and ActiveMQ Artemis implement:
//!
//! | etcd | AMQP | JMS |
//! |---|---|---|
//! | the value as text | `amqp-value` string, `x-opt-jms-msg-type` 5 | `TextMessage` |
//! | the value as bytes | one `data` section, `application/octet-stream`, type 3 | `BytesMessage` |
//! | one per delivery | `message-id` `ID:<uuid>` | `JMSMessageID` |
//! | the message key (`{key}`), with `group_by_key` | `group-id` | `JMSXGroupID` |
//! | the bridge's clock | `creation-time` | `JMSTimestamp` |
//! | durable, always | `header.durable` | `JMSDeliveryMode` PERSISTENT |
//!
//! and application properties, named as JMS identifiers: `etcd_queue`,
//! `etcd_key`, `etcd_index` (when the delivery key carries one) and
//! `etcd_delivery_key`.
//!
//! With `Shape::cloudevents` the message is also a CloudEvent in the AMQP
//! binding's binary mode: each attribute an application property prefixed
//! `cloudEvents_` (the binding's spelling for JMS, whose property names cannot
//! hold `:`), and `datacontenttype` the AMQP `content-type`.

use std::str::FromStr;

use fe2o3_amqp::types::messaging::{
    AmqpValue, ApplicationProperties, Data, Header, Message, MessageAnnotations, Properties,
};
use fe2o3_amqp::types::primitives::{Binary, Symbol, Timestamp, Value};
use uuid::Uuid;

/// `x-opt-jms-msg-type` values (JMS Mapping, 3.2.4.1)
const JMS_BYTES_MESSAGE: i8 = 3;
const JMS_TEXT_MESSAGE: i8 = 5;
/// `x-opt-jms-dest`: the destination is a queue
const JMS_QUEUE: i8 = 0;

/// Which JMS message type a value becomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BodyType {
    /// `TextMessage` when the value is valid UTF-8, `BytesMessage` otherwise:
    /// one queue can carry both, and a listener has to take either.
    #[default]
    Auto,
    /// Always a `TextMessage`; a value that is not UTF-8 cannot be sent.
    Text,
    /// Always a `BytesMessage`.
    Bytes,
}

impl FromStr for BodyType {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "auto" => Ok(BodyType::Auto),
            "text" => Ok(BodyType::Text),
            "bytes" => Ok(BodyType::Bytes),
            _ => Err(format!("body type {:?}: auto, text or bytes", s)),
        }
    }
}

/// How deliveries are shaped as messages.
#[derive(Debug, Clone)]
pub struct Shape {
    pub body: BodyType,
    /// `JMSXGroupID` = the message key. A broker pins each group to one
    /// consumer (Artemis) and keeps a map of them, so it is off by default.
    /// AMQP only.
    pub group_by_key: bool,
    /// Add the CloudEvents attributes ([`crate::bridge::cloudevents`]).
    pub cloudevents: bool,
    /// The CloudEvents `type`.
    pub ce_type: String,
}

impl Default for Shape {
    fn default() -> Self {
        Shape {
            body: BodyType::default(),
            group_by_key: false,
            cloudevents: false,
            ce_type: crate::bridge::cloudevents::DEFAULT_TYPE.to_string(),
        }
    }
}

/// The value as text, `None` for bytes; an error when it cannot take the body
/// type asked for: no broker will ever take it.
pub fn body_text(d: &Delivery, body: BodyType) -> Result<Option<&str>, String> {
    if d.event == Some(KvEvent::Delete) {
        return Ok(None); // no value: an empty body
    }
    match body {
        BodyType::Bytes => Ok(None),
        BodyType::Auto => Ok(std::str::from_utf8(&d.value).ok()),
        BodyType::Text => std::str::from_utf8(&d.value).map(Some).map_err(|e| format!("not text: {}", e)),
    }
}

/// A key's change, when the bridge watches a key prefix instead of a queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KvEvent {
    Put,
    Delete,
}

impl KvEvent {
    pub fn name(self) -> &'static str {
        match self {
            KvEvent::Put => "put",
            KvEvent::Delete => "delete",
        }
    }
}

/// An `etcd_*` property's value.
#[derive(Debug, Clone, PartialEq)]
pub enum Prop {
    Str(String),
    Long(i64),
}

impl std::fmt::Display for Prop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Prop::Str(s) => f.write_str(s),
            Prop::Long(i) => write!(f, "{}", i),
        }
    }
}

/// What the watch handed over: the key to acknowledge and the message.
#[derive(Debug, Clone, PartialEq)]
pub struct Delivery {
    /// The queue, or with a key prefix watched, the prefix.
    pub queue: String,
    /// The delivery key as etcd sent it: the acknowledge deletes these bytes.
    pub raw_key: Vec<u8>,
    /// The delivery key as text, for the log and the properties.
    pub delivery_key: String,
    /// The producer's `{key}`, the last segment of the delivery key; for a
    /// key's change, the key after the prefix.
    pub key: String,
    /// The dispatcher's index, when the delivery key carries one. A message
    /// forwarded to the dispatcher's own node comes as `/q/{name}/p/{key}`,
    /// with none (etcd agent.md, known gaps).
    pub index: Option<u64>,
    pub value: Vec<u8>,
    /// `JMSMessageID`, made when the delivery arrived: every send of this one
    /// delivery carries it, so a broker's duplicate check can drop a resend
    /// after a timeout. A delivery etcd hands out again gets a new one.
    pub message_id: String,
    /// `None` for a queue message; a key's change otherwise, which is never
    /// acknowledged - a watch has nothing to acknowledge, and a delete would
    /// remove the key.
    pub event: Option<KvEvent>,
}

impl Delivery {
    pub fn new(queue: &str, delivery_key: &[u8], value: &[u8]) -> Self {
        let text = String::from_utf8_lossy(delivery_key).into_owned();
        let mut segments = text.rsplit('/');
        let key = segments.next().unwrap_or_default().to_string();
        let index = segments.next().and_then(|s| s.parse().ok());
        Delivery {
            queue: queue.to_string(),
            raw_key: delivery_key.to_vec(),
            delivery_key: text,
            key,
            index,
            value: value.to_vec(),
            message_id: format!("ID:{}", Uuid::new_v4()),
            event: None,
        }
    }

    /// A change of `key` under the watched `prefix`.
    pub fn kv(prefix: &str, key: &[u8], value: &[u8], event: KvEvent) -> Self {
        let text = String::from_utf8_lossy(key).into_owned();
        Delivery {
            queue: prefix.to_string(),
            raw_key: key.to_vec(),
            key: text.strip_prefix(prefix).unwrap_or(&text).to_string(),
            delivery_key: text,
            index: None,
            value: if event == KvEvent::Delete { Vec::new() } else { value.to_vec() },
            message_id: format!("ID:{}", Uuid::new_v4()),
            event: Some(event),
        }
    }

    /// The `etcd_*` properties: `etcd_queue` - or `etcd_prefix` and
    /// `etcd_event` (`put`, `delete`) for a key's change - `etcd_key`,
    /// `etcd_index` when there is one, and `etcd_delivery_key`.
    pub fn properties(&self) -> Vec<(&'static str, Prop)> {
        let mut p = Vec::with_capacity(5);
        match self.event {
            None => p.push(("etcd_queue", Prop::Str(self.queue.clone()))),
            Some(e) => {
                p.push(("etcd_prefix", Prop::Str(self.queue.clone())));
                p.push(("etcd_event", Prop::Str(e.name().to_string())));
            }
        }
        p.push(("etcd_key", Prop::Str(self.key.clone())));
        if let Some(i) = self.index {
            p.push(("etcd_index", Prop::Long(i as i64)));
        }
        p.push(("etcd_delivery_key", Prop::Str(self.delivery_key.clone())));
        p
    }

    /// The message id without JMS's `ID:` prefix: the CloudEvents `id`.
    pub fn event_id(&self) -> &str {
        self.message_id.strip_prefix("ID:").unwrap_or(&self.message_id)
    }
}

/// The two body shapes a JMS client tells apart.
pub enum Outgoing {
    Text(Message<AmqpValue<String>>),
    Bytes(Message<Data>),
}

/// The AMQP message for a delivery, `now_ms` its creation time. An error is a
/// value that cannot take the shape asked for: no broker will ever take it.
pub fn to_amqp(d: &Delivery, shape: &Shape, now_ms: i64) -> Result<Outgoing, String> {
    let text = body_text(d, shape.body)?;

    let mut props = Properties::builder()
        .message_id(d.message_id.clone())
        .creation_time(Timestamp::from_milliseconds(now_ms));
    if shape.group_by_key {
        props = props.group_id(d.key.clone());
    }
    if shape.cloudevents {
        let ct = if text.is_some() { crate::bridge::cloudevents::TEXT } else { crate::bridge::cloudevents::BYTES };
        props = props.content_type(Symbol::from(ct));
    } else if text.is_none() {
        props = props.content_type(Symbol::from("application/octet-stream"));
    }

    let mut app = ApplicationProperties::builder();
    for (k, v) in d.properties() {
        app = match v {
            Prop::Str(s) => app.insert(k, s),
            Prop::Long(i) => app.insert(k, i),
        };
    }
    if shape.cloudevents {
        for (k, v) in crate::bridge::cloudevents::attributes(d, &shape.ce_type, text.is_some(), now_ms) {
            if k != "datacontenttype" {
                app = app.insert(format!("cloudEvents_{}", k), v);
            }
        }
    }

    let msg_type = if text.is_some() { JMS_TEXT_MESSAGE } else { JMS_BYTES_MESSAGE };
    let annotations = MessageAnnotations::builder()
        .insert(Symbol::from("x-opt-jms-msg-type"), Value::Byte(msg_type))
        .insert(Symbol::from("x-opt-jms-dest"), Value::Byte(JMS_QUEUE))
        .build();

    let builder = Message::builder()
        .header(Header::builder().durable(true).build())
        .message_annotations(annotations)
        .properties(props.build())
        .application_properties(app.build());

    Ok(match text {
        Some(s) => Outgoing::Text(builder.value(s.to_string()).build()),
        None => Outgoing::Bytes(builder.data(Binary::from(d.value.clone())).build()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fe2o3_amqp::types::messaging::annotations::OwnedKey;
    use fe2o3_amqp::types::primitives::SimpleValue;

    fn annotation(m: &Option<MessageAnnotations>, name: &str) -> Option<Value> {
        m.as_ref().unwrap().get(&OwnedKey::from(Symbol::from(name))).cloned()
    }

    #[test]
    fn delivery_key_shapes() {
        let d = Delivery::new("jobs", b"/q/jobs/42/job-1", b"x");
        assert_eq!((d.key.as_str(), d.index), ("job-1", Some(42)));

        let d = Delivery::new("jobs", b"/q/jobs/c/0b6e2e7c-5f7e-4bd6-9c43-3b0b8f7f7a10/7/job-2", b"x");
        assert_eq!((d.key.as_str(), d.index), ("job-2", Some(7)));

        // the forwarded-message gap: a producer key, no index
        let d = Delivery::new("jobs", b"/queue/jobs/producer/job-3", b"x");
        assert_eq!((d.key.as_str(), d.index), ("job-3", None));
    }

    #[test]
    fn raw_key_kept_and_ids_unique() {
        let raw = b"/q/jobs/1/k\xff";
        let a = Delivery::new("jobs", raw, b"x");
        assert_eq!(a.raw_key, raw.to_vec(), "the acknowledge deletes the bytes etcd sent");
        let b = Delivery::new("jobs", raw, b"x");
        assert!(a.message_id.starts_with("ID:"));
        assert_ne!(a.message_id, b.message_id, "the same key handed out twice is two messages");
    }

    #[test]
    fn text_value_is_a_text_message() {
        let d = Delivery::new("jobs", b"/q/jobs/42/job-1", "привет".as_bytes());
        let Ok(Outgoing::Text(m)) = to_amqp(&d, &Shape::default(), 1_000) else { panic!("expected text") };
        assert_eq!(m.body.0, "привет");
        assert!(m.header.as_ref().unwrap().durable);

        let p = m.properties.as_ref().unwrap();
        assert_eq!(p.message_id, Some(d.message_id.clone().into()));
        assert_eq!(p.group_id, None, "no grouping unless asked");
        assert_eq!(p.creation_time, Some(Timestamp::from_milliseconds(1_000)));
        assert_eq!(p.content_type, None);

        assert_eq!(annotation(&m.message_annotations, "x-opt-jms-msg-type"), Some(Value::Byte(JMS_TEXT_MESSAGE)));
        assert_eq!(annotation(&m.message_annotations, "x-opt-jms-dest"), Some(Value::Byte(JMS_QUEUE)));

        let app = &m.application_properties.as_ref().unwrap().0;
        assert_eq!(app.get("etcd_queue"), Some(&SimpleValue::String("jobs".into())));
        assert_eq!(app.get("etcd_key"), Some(&SimpleValue::String("job-1".into())));
        assert_eq!(app.get("etcd_index"), Some(&SimpleValue::Long(42)));
        assert_eq!(app.get("etcd_delivery_key"), Some(&SimpleValue::String("/q/jobs/42/job-1".into())));
    }

    #[test]
    fn binary_value_is_a_bytes_message() {
        let d = Delivery::new("jobs", b"/q/jobs/p/job-3", &[0xff, 0x00, 0xfe]);
        let Ok(Outgoing::Bytes(m)) = to_amqp(&d, &Shape::default(), 1_000) else { panic!("expected bytes") };
        assert_eq!(m.body.0.as_ref(), &[0xff, 0x00, 0xfe]);

        let p = m.properties.as_ref().unwrap();
        assert_eq!(p.content_type, Some(Symbol::from("application/octet-stream")));
        assert_eq!(annotation(&m.message_annotations, "x-opt-jms-msg-type"), Some(Value::Byte(JMS_BYTES_MESSAGE)));

        let app = &m.application_properties.as_ref().unwrap().0;
        assert!(app.get("etcd_index").is_none());
    }

    #[test]
    fn fixed_body_types_and_grouping() {
        let d = Delivery::new("jobs", b"/q/jobs/1/k", b"plain text");
        let bytes = Shape { body: BodyType::Bytes, group_by_key: true, ..Shape::default() };
        let Ok(Outgoing::Bytes(m)) = to_amqp(&d, &bytes, 0) else { panic!("bytes asked") };
        assert_eq!(m.properties.as_ref().unwrap().group_id.as_deref(), Some("k"));

        let text = Shape { body: BodyType::Text, ..Shape::default() };
        assert!(matches!(to_amqp(&d, &text, 0), Ok(Outgoing::Text(_))));
        let bin = Delivery::new("jobs", b"/q/jobs/1/k", &[0xff]);
        assert!(to_amqp(&bin, &text, 0).is_err(), "not UTF-8 cannot be a TextMessage");

        assert_eq!("bytes".parse::<BodyType>(), Ok(BodyType::Bytes));
        assert!("blob".parse::<BodyType>().is_err());
    }

    #[test]
    fn a_key_change() {
        let d = Delivery::kv("/config/", b"/config/db/url", b"pg://x", KvEvent::Put);
        assert_eq!((d.key.as_str(), d.index, d.event), ("db/url", None, Some(KvEvent::Put)));
        let p = d.properties();
        assert!(p.contains(&("etcd_prefix", Prop::Str("/config/".into()))));
        assert!(p.contains(&("etcd_event", Prop::Str("put".into()))));
        assert!(p.iter().all(|(k, _)| *k != "etcd_queue"));

        let del = Delivery::kv("/config/", b"/config/db/url", b"ignored", KvEvent::Delete);
        assert!(del.value.is_empty());
        let text = Shape { body: BodyType::Text, ..Shape::default() };
        assert_eq!(body_text(&del, BodyType::Text), Ok(None), "a delete has no body to be text");
        let Ok(Outgoing::Bytes(m)) = to_amqp(&del, &text, 0) else { panic!("an empty body") };
        assert!(m.body.0.is_empty());
        let app = &m.application_properties.as_ref().unwrap().0;
        assert_eq!(app.get("etcd_event"), Some(&SimpleValue::String("delete".into())));
    }

    #[test]
    fn cloudevents_in_the_amqp_binding() {
        let d = Delivery::new("jobs", b"/q/jobs/7/k", b"hi");
        let shape = Shape { cloudevents: true, ..Shape::default() };
        let Ok(Outgoing::Text(m)) = to_amqp(&d, &shape, 0) else { panic!("text") };
        let p = m.properties.as_ref().unwrap();
        assert_eq!(p.content_type, Some(Symbol::from(crate::bridge::cloudevents::TEXT)));
        let app = &m.application_properties.as_ref().unwrap().0;
        let s = |k: &str| app.get(k).cloned();
        assert_eq!(s("cloudEvents_specversion"), Some(SimpleValue::String("1.0".into())));
        assert_eq!(s("cloudEvents_id"), Some(SimpleValue::String(d.event_id().into())));
        assert_eq!(s("cloudEvents_source"), Some(SimpleValue::String("/q/jobs".into())));
        assert_eq!(s("cloudEvents_subject"), Some(SimpleValue::String("k".into())));
        assert_eq!(s("cloudEvents_etcdindex"), Some(SimpleValue::String("7".into())));
        assert!(s("cloudEvents_datacontenttype").is_none(), "it is the content-type");
        assert!(s("etcd_key").is_some(), "the etcd_* properties stay");
        assert_eq!(format!("ID:{}", d.event_id()), d.message_id);
    }
}
