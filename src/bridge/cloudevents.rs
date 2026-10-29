//! A delivery as a CloudEvent (CloudEvents 1.0), in binary content mode: the
//! value is the event's data as it is, and the attributes ride beside it -
//! each protocol binding names them its own way.
//!
//! | attribute | value |
//! |---|---|
//! | `specversion` | `1.0` |
//! | `id` | the delivery's message id, a UUID: the same on every resend |
//! | `source` | `/q/{queue}` |
//! | `type` | `Shape::ce_type`, `etcd.queue.message` by default |
//! | `subject` | the message key |
//! | `time` | when the bridge sent it, RFC 3339 |
//! | `datacontenttype` | `text/plain; charset=utf-8` or `application/octet-stream` |
//! | `etcdqueue`, `etcdkey`, `etcddeliverykey` | extensions, as the `etcd_*` properties |
//! | `etcdindex` | extension, when the delivery key carries an index |
//!
//! A key's change under a watched prefix has the prefix as `source`, `type`
//! with `.put` or `.delete` added (`etcd.kv.put`, `etcd.kv.delete` by
//! default), and `etcdprefix` and `etcdevent` for `etcdqueue`.

use rskafka::chrono::{DateTime, SecondsFormat, Utc};

use crate::bridge::mapping::Delivery;

pub const SPEC_VERSION: &str = "1.0";
pub const DEFAULT_TYPE: &str = "etcd.queue.message";
/// The `type` of a key's change while `Shape::ce_type` is the default, before
/// `.put` or `.delete`.
pub const KV_TYPE: &str = "etcd.kv";
pub const TEXT: &str = "text/plain; charset=utf-8";
pub const BYTES: &str = "application/octet-stream";

/// The attributes, by their CloudEvents names, in a fixed order.
pub fn attributes(d: &Delivery, ce_type: &str, text: bool, now_ms: i64) -> Vec<(&'static str, String)> {
    let time = DateTime::<Utc>::from_timestamp_millis(now_ms).unwrap_or_default();
    let (source, ce_type) = match d.event {
        None => (format!("/q/{}", d.queue), ce_type.to_string()),
        Some(e) => {
            let base = if ce_type == DEFAULT_TYPE { KV_TYPE } else { ce_type };
            (d.queue.clone(), format!("{}.{}", base, e.name()))
        }
    };
    let mut a = vec![
        ("specversion", SPEC_VERSION.to_string()),
        ("id", d.event_id().to_string()),
        ("source", source),
        ("type", ce_type),
        ("subject", d.key.clone()),
        ("time", time.to_rfc3339_opts(SecondsFormat::Millis, true)),
        ("datacontenttype", if text { TEXT } else { BYTES }.to_string()),
    ];
    match d.event {
        None => a.push(("etcdqueue", d.queue.clone())),
        Some(e) => {
            a.push(("etcdprefix", d.queue.clone()));
            a.push(("etcdevent", e.name().to_string()));
        }
    }
    a.push(("etcdkey", d.key.clone()));
    a.push(("etcddeliverykey", d.delivery_key.clone()));
    if let Some(i) = d.index {
        a.push(("etcdindex", i.to_string()));
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attributes_of_a_delivery() {
        let d = Delivery::new("jobs", b"/q/jobs/42/job-1", b"x");
        let a = attributes(&d, DEFAULT_TYPE, true, 1_700_000_000_123);
        let get = |n: &str| a.iter().find(|(k, _)| *k == n).map(|(_, v)| v.as_str());
        assert_eq!(get("specversion"), Some("1.0"));
        assert_eq!(get("id"), Some(d.event_id()));
        assert!(Uuid::parse_str(get("id").unwrap()).is_ok());
        assert_eq!(get("source"), Some("/q/jobs"));
        assert_eq!(get("type"), Some("etcd.queue.message"));
        assert_eq!(get("subject"), Some("job-1"));
        assert_eq!(get("time"), Some("2023-11-14T22:13:20.123Z"));
        assert_eq!(get("datacontenttype"), Some(TEXT));
        assert_eq!(get("etcdindex"), Some("42"));
        // extension names: lower-case letters and digits only (spec, 3.1.1)
        for (k, _) in &a {
            assert!(k.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()), "{}", k);
        }

        let p = Delivery::new("jobs", b"/q/jobs/p/k", &[0xff]);
        let a = attributes(&p, "my.type", false, 0);
        assert!(a.iter().all(|(k, _)| *k != "etcdindex"));
        assert!(a.contains(&("datacontenttype", BYTES.to_string())));
        assert!(a.contains(&("type", "my.type".to_string())));

        let k = Delivery::kv("/config/", b"/config/a", b"", crate::bridge::mapping::KvEvent::Delete);
        let a = attributes(&k, DEFAULT_TYPE, false, 0);
        assert!(a.contains(&("type", "etcd.kv.delete".to_string())));
        let a = attributes(&k, "my.type", false, 0);
        assert!(a.contains(&("type", "my.type.delete".to_string())));
        assert!(a.contains(&("source", "/config/".to_string())));
        assert!(a.contains(&("etcdevent", "delete".to_string())));
        assert!(a.iter().all(|(k, _)| *k != "etcdqueue"));
    }

    use uuid::Uuid;
}
