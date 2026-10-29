//! [bridge] The bridge in the binary, from the `bridge_*` settings: over the
//! node's first client URL; src/main.rs starts it once the node serves and stops
//! it on the signal that stops the node.

use std::time::Duration;

use super::Config;
use super::listener::{self, Listen};
use super::mapping::{BodyType, Shape};
use slog::{Logger, info};
use uuid::Uuid;

use crate::cli::EtcdConfig;

/// What the settings ask for. No `Debug`: a listener's password is in it.
pub enum Bridge {
    /// a queue or a key prefix to `bridge_to`
    Send(Config),
    /// JMS clients connecting to `bridge_listen`
    Listen(Config, Listen),
}

/// The bridge the settings ask for, `None` when they name none; refused - why, in
/// the bridge's words - when they do not make one.
pub fn settings(cfg: &EtcdConfig) -> Result<Option<Bridge>, String> {
    if !cfg.bridge_given() {
        return Ok(None);
    }
    let some = |s: &str| Some(s.trim().to_string()).filter(|s| !s.is_empty());
    let (queue, prefix, listen, to) = (some(&cfg.bridge_queue), some(&cfg.bridge_prefix), some(&cfg.bridge_listen), some(&cfg.bridge_to));
    if [&queue, &prefix, &listen].iter().filter(|s| s.is_some()).count() != 1 {
        return Err("one of bridge_queue, bridge_prefix and bridge_listen".into());
    }
    match (&listen, &to) {
        (Some(_), Some(_)) => return Err("bridge_listen is the AMQP peer itself: no bridge_to".into()),
        (None, None) => return Err("no bridge_to, the broker to send to".into()),
        _ => {}
    }
    let user = match (some(&cfg.bridge_listen_user), some(&cfg.bridge_listen_password)) {
        (Some(u), Some(p)) => Some((u, p)),
        (None, None) => None,
        _ => return Err("bridge_listen_user and bridge_listen_password: both or neither".into()),
    };
    // [bridge] a setting the mode would leave unused is refused, not dropped
    let unused: &[(&str, &str)] = if listen.is_some() {
        &[("bridge_consumer", &cfg.bridge_consumer), ("bridge_address", &cfg.bridge_address)]
    } else {
        &[("bridge_listen_user", &cfg.bridge_listen_user), ("bridge_listen_password", &cfg.bridge_listen_password)]
    };
    if let Some((name, _)) = unused.iter().find(|(_, v)| !v.trim().is_empty()) {
        return Err(format!("{}: not used by {}", name, if listen.is_some() { "bridge_listen" } else { "a queue or a prefix" }));
    }
    let consumer = match some(&cfg.bridge_consumer) {
        Some(c) => Uuid::parse_str(&c).map_err(|e| format!("bridge_consumer {:?}: {}", c, e))?,
        None => Uuid::new_v4(),
    };
    // empty: an embedder's `EtcdConfig::default()`, clap's auto
    let body: BodyType = some(&cfg.bridge_body).as_deref().unwrap_or("auto").parse().map_err(|e| format!("bridge_body: {}", e))?;
    let queue = queue.unwrap_or_default();
    let address = some(&cfg.bridge_address).unwrap_or_else(|| match &prefix {
        // a prefix's name, as a topic or address: /config/db/ -> config.db
        Some(p) => p.trim_matches('/').replace('/', "."),
        None => queue.clone(),
    });
    let c = Config {
        etcd: vec![endpoint(&cfg.listen_client_urls)?],
        etcd_user: None, // etcds checks no user
        queue,
        prefix,
        consumer,
        target_url: to.unwrap_or_default(),
        address,
        shape: Shape {
            body,
            group_by_key: cfg.bridge_group_by_key,
            cloudevents: cfg.bridge_cloudevents,
            ce_type: some(&cfg.bridge_ce_type).unwrap_or_else(|| super::cloudevents::DEFAULT_TYPE.to_string()),
        },
        dead_letter: some(&cfg.bridge_dead_letter),
        // 0 is the type's default (`EtcdConfig::default()`), not a setting
        buffer: if cfg.bridge_buffer == 0 { 64 } else { cfg.bridge_buffer as usize },
        send_timeout: Duration::from_secs(if cfg.bridge_send_timeout == 0 { 30 } else { cfg.bridge_send_timeout }),
    };
    match listen {
        // a listener's template: its queue, address and target come from each
        // client's link - checked here with stand-ins, so a bad setting stops the
        // node at start rather than refusing every link after
        Some(bind) => {
            let (_, port) = bind.rsplit_once(':').ok_or_else(|| format!("bridge_listen {:?}: host:port", bind))?;
            port.parse::<u16>().map_err(|e| format!("bridge_listen {:?}: {}", bind, e))?;
            Config { queue: "q".into(), address: "q".into(), target_url: "amqp://listener".into(), ..c.clone() }.validate()?;
            Ok(Some(Bridge::Listen(c, Listen { bind, user })))
        }
        None => c.validate().map(|_| Some(Bridge::Send(c))),
    }
}

/// How long a stop may take: the listener waits up to `send_timeout` for its
/// clients' outcomes, then a margin.
pub fn stop_within(b: &Bridge) -> Duration {
    match b {
        Bridge::Send(c) | Bridge::Listen(c, _) => c.send_timeout + Duration::from_secs(10),
    }
}

/// The node as its own client: the first client URL, a wildcard bind as
/// loopback. Plain `http://` whatever the URL says: the node serves its clients
/// without TLS (`EtcdNode::serve` drops the scheme).
fn endpoint(listen_client_urls: &str) -> Result<String, String> {
    let first = listen_client_urls.split(',').next().unwrap_or_default().trim();
    let host = first.split_once("://").map_or(first, |(_, h)| h);
    if host.is_empty() {
        return Err("no listen_client_urls: the bridge connects to the node there".into());
    }
    Ok(format!("http://{}", host.replacen("0.0.0.0", "127.0.0.1", 1).replacen("[::]", "[::1]", 1)))
}

/// The bridge until `stop`: `Ok` once stopped, `Err` when it ended for good.
pub async fn run(b: Bridge, log: Logger, stop: impl Future<Output = ()>) -> Result<(), String> {
    match b {
        Bridge::Send(c) => {
            let source = c.prefix.as_ref().map_or(format!("queue {}", c.queue), |p| format!("prefix {}", p));
            info!(log, "bridge: {} -> {} address {}", source, super::redact(&c.target_url), c.address; "consumer" => %c.consumer);
            super::run(c, log, stop).await
        }
        Bridge::Listen(c, l) => {
            info!(log, "bridge: AMQP clients on {}", l.bind);
            listener::listen(c, l, log, stop).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(f: impl FnOnce(&mut EtcdConfig)) -> EtcdConfig {
        let mut c = EtcdConfig::with_defaults();
        f(&mut c);
        c
    }

    #[test]
    fn no_bridge_settings_no_bridge() {
        assert!(settings(&EtcdConfig::with_defaults()).unwrap().is_none());
        assert!(settings(&EtcdConfig::default()).unwrap().is_none(), "an embedder's Default");
    }

    /// `EtcdConfig::default()`'s empty and 0 settings are the binary's defaults
    #[test]
    fn an_embedders_default_with_a_queue() {
        let mut c = EtcdConfig {
            listen_client_urls: "localhost:2379".into(),
            bridge_queue: "jobs".into(),
            bridge_to: "amqp://h:5672".into(),
            ..EtcdConfig::default()
        };
        let Some(Bridge::Send(b)) = settings(&c).unwrap() else { panic!("a sender") };
        assert_eq!((b.shape.body, b.buffer, b.send_timeout), (BodyType::Auto, 64, Duration::from_secs(30)));
        assert_eq!(stop_within(&Bridge::Send(b)), Duration::from_secs(40));
        c.listen_client_urls = String::new();
        assert!(settings(&c).is_err(), "no URL to reach the node");
    }

    #[test]
    fn a_queue_to_a_broker() {
        let c = cfg(|c| {
            c.listen_client_urls = "0.0.0.0:2379,http://other:2379".into();
            c.bridge_queue = "jobs".into();
            c.bridge_to = "amqp://artemis:5672".into();
            c.bridge_consumer = "6f1c1f4e-55a2-4a8e-9a39-0d1f7d1c2b3a".into();
            c.bridge_body = "bytes".into();
            c.bridge_buffer = 0;
        });
        let Some(Bridge::Send(b)) = settings(&c).unwrap() else { panic!("a sender") };
        assert_eq!(b.etcd, vec!["http://127.0.0.1:2379".to_string()]);
        assert_eq!((b.queue.as_str(), b.address.as_str(), b.buffer), ("jobs", "jobs", 64));
        assert_eq!(b.consumer.to_string(), "6f1c1f4e-55a2-4a8e-9a39-0d1f7d1c2b3a");
        assert_eq!(b.shape.body, BodyType::Bytes);
        assert_eq!(b.shape.ce_type, crate::bridge::cloudevents::DEFAULT_TYPE);
        assert_eq!(b.send_timeout, Duration::from_secs(30));
    }

    #[test]
    fn a_prefix_names_its_address() {
        let c = cfg(|c| {
            c.bridge_prefix = "/config/db/".into();
            c.bridge_to = "kafka://k:9092".into();
        });
        let Some(Bridge::Send(b)) = settings(&c).unwrap() else { panic!("a sender") };
        assert_eq!(b.address, "config.db");
        assert_eq!(b.prefix.as_deref(), Some("/config/db/"));
    }

    #[test]
    fn a_listener() {
        let c = cfg(|c| {
            c.listen_client_urls = "https://[::]:2379".into();
            c.bridge_listen = "127.0.0.1:5672".into();
            c.bridge_listen_user = "app".into();
            c.bridge_listen_password = "pw".into();
        });
        let Some(Bridge::Listen(b, l)) = settings(&c).unwrap() else { panic!("a listener") };
        assert_eq!(b.etcd, vec!["http://[::1]:2379".to_string()], "the node serves no TLS");
        assert_eq!(l.bind, "127.0.0.1:5672");
        assert_eq!(l.user, Some(("app".to_string(), "pw".to_string())));
    }

    #[test]
    fn what_it_refuses() {
        let refused: [fn(&mut EtcdConfig); 14] = [
            |c| { c.bridge_listen = "nohostport".into() },
            |c| { c.bridge_listen = "127.0.0.1:x".into() },
            |c| { c.bridge_listen = "127.0.0.1:1".into(); c.bridge_dead_letter = "a/b".into() }, // the template checked
            |c| { c.bridge_listen = "127.0.0.1:1".into(); c.bridge_address = "a".into() },        // unused
            |c| { c.bridge_queue = "q".into(); c.bridge_to = "amqp://h".into(); c.bridge_listen_user = "u".into(); c.bridge_listen_password = "p".into() },
            |c| c.bridge_to = "amqp://h:5672".into(),                                   // no source
            |c| c.bridge_queue = "q".into(),                                            // no broker
            |c| { c.bridge_queue = "q".into(); c.bridge_prefix = "/a/".into(); c.bridge_to = "amqp://h".into() },
            |c| { c.bridge_listen = "127.0.0.1:1".into(); c.bridge_to = "amqp://h".into() },
            |c| { c.bridge_listen = "127.0.0.1:1".into(); c.bridge_listen_user = "u".into() },
            |c| { c.bridge_queue = "q".into(); c.bridge_to = "amqp://h".into(); c.bridge_consumer = "nope".into() },
            |c| { c.bridge_queue = "q".into(); c.bridge_to = "amqp://h".into(); c.bridge_body = "blob".into() },
            |c| { c.bridge_queue = "a/b".into(); c.bridge_to = "amqp://h".into() },        // Config::validate
            |c| { c.bridge_queue = "q".into(); c.bridge_to = "http://h".into() },
        ];
        for (i, f) in refused.into_iter().enumerate() {
            assert!(settings(&cfg(f)).is_err(), "case {}", i);
        }
    }
}
