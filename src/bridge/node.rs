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
        return Err(tr!("bridge.one_source"));
    }
    match (&listen, &to) {
        (Some(_), Some(_)) => return Err(tr!("bridge.listen_no_to")),
        (None, None) => return Err(tr!("bridge.no_to")),
        _ => {}
    }
    let user = match (some(&cfg.bridge_listen_user), some(&cfg.bridge_listen_password)) {
        (Some(u), Some(p)) => Some((u, p)),
        (None, None) => None,
        _ => return Err(tr!("bridge.listen_user_both")),
    };
    // [bridge] a setting the mode would leave unused is refused, not dropped
    let unused: &[(&str, &str)] = if listen.is_some() {
        &[("bridge_consumer", &cfg.bridge_consumer), ("bridge_address", &cfg.bridge_address)]
    } else {
        &[("bridge_listen_user", &cfg.bridge_listen_user), ("bridge_listen_password", &cfg.bridge_listen_password)]
    };
    if let Some((name, _)) = unused.iter().find(|(_, v)| !v.trim().is_empty()) {
        return Err(if listen.is_some() { tr!("bridge.not_used_listen", name = name) } else { tr!("bridge.not_used_send", name = name) });
    }
    let consumer = match some(&cfg.bridge_consumer) {
        Some(c) => Uuid::parse_str(&c).map_err(|e| tr!("bridge.bad_consumer", value = format!("{:?}", c), err = e))?,
        None => Uuid::new_v4(),
    };
    // [bridge-http] an endpoint is its URL: an address would be dropped, so it is refused
    if to.as_deref().is_some_and(|t| super::broker::kind(t) == Ok(super::broker::Kind::Http)) && some(&cfg.bridge_address).is_some() {
        return Err(tr!("bridge.address_http"));
    }
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
        node: None,
        // [bridge-secret] the node's own, as `EtcdNode::init` builds it
        policy: Some(crate::prefix::PrefixPolicy::new(&cfg.local_prefix, &cfg.local_prefix_except,
            &cfg.local_only_prefix, &cfg.zone_prefix).with_local_only_except(&cfg.local_only_prefix_except)),
    };
    // [bridge-secret] refused here, before the node starts, not at the first watch; a
    // listener's queue comes with each link, which is checked as it attaches
    if let (Some(p), None) = (&c.policy, &listen) {
        c.served_out(p)?;
    }
    match listen {
        // a listener's template: its queue, address and target come from each
        // client's link - checked here with stand-ins, so a bad setting stops the
        // node at start rather than refusing every link after
        Some(bind) => {
            let (_, port) = bind.rsplit_once(':').ok_or_else(|| tr!("bridge.listen_addr", value = format!("{:?}", bind)))?;
            port.parse::<u16>().map_err(|e| tr!("bridge.listen_port", value = format!("{:?}", bind), err = e))?;
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
        return Err(tr!("bridge.no_client_urls"));
    }
    Ok(format!("http://{}", host.replacen("0.0.0.0", "127.0.0.1", 1).replacen("[::]", "[::1]", 1)))
}

/// The bridge until `stop`: `Ok` once stopped, `Err` when it ended for good.
pub async fn run(b: Bridge, log: Logger, stop: impl Future<Output = ()>) -> Result<(), String> {
    match b {
        Bridge::Send(c) => {
            let source = c.prefix.as_ref().map_or(tr!("bridge.source_queue", name = c.queue), |p| tr!("bridge.source_prefix", name = p));
            let url = super::redact(&c.target_url);
            // an endpoint's address is its URL
            let line = match super::broker::kind(&c.target_url) {
                Ok(super::broker::Kind::Http) => tr!("bridge.info_send_http", source = source, url = url),
                _ => tr!("bridge.info_send", source = source, url = url, address = c.address),
            };
            info!(log, "{}", line; "consumer" => %c.consumer);
            super::run(c, log, stop).await
        }
        Bridge::Listen(c, l) => {
            info!(log, "{}", tr!("bridge.info_listen", bind = l.bind));
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

    /// [bridge-secret] a prefix holding keys the node serves to no remote caller is
    /// refused at start - the bridge would carry them out of it; one of the subtree's
    /// exceptions, or a prefix beside it, is not
    #[test]
    fn a_prefix_the_node_keeps_local_is_refused() {
        let with = |prefix: &str| cfg(|c| {
            c.listen_client_urls = "localhost:2379".into();
            c.bridge_prefix = prefix.into();
            c.bridge_to = "kafka://k:9092".into();
            c.local_only_prefix = "/secret/".into();
            c.local_only_prefix_except = "/secret/encrypted/".into();
        });
        for p in ["/secret/", "/secret/@meta/", "/", "/s"] {
            let e = settings(&with(p)).err().unwrap_or_else(|| panic!("{} accepted", p));
            assert!(e.contains("serves to no remote caller"), "{}: {}", p, e);
        }
        for p in ["/secret/encrypted/", "/config/", "/secrets-not/"] {
            assert!(matches!(settings(&with(p)), Ok(Some(Bridge::Send(_)))), "{} refused", p);
        }
        // no local-only prefix: nothing refused, as before
        let mut c = with("/secret/");
        c.local_only_prefix = String::new();
        assert!(settings(&c).is_ok());
    }

    /// [bridge-secret] a queue the node serves to no remote consumer is refused at start,
    /// either spelling of the policy; a queue beside it, or a policy over its producer
    /// keys only (a remote consumer is served then, so the bridge is), is not. A listener
    /// is not refused at start: its queues come with its links (`listener::consume`)
    #[test]
    fn a_queue_the_node_keeps_local_is_refused() {
        let with = |queue: &str, local: &str| cfg(|c| {
            c.listen_client_urls = "localhost:2379".into();
            c.bridge_queue = queue.into();
            c.bridge_to = "kafka://k:9092".into();
            c.local_only_prefix = local.into();
        });
        for local in ["/q/payroll/", "/queue/payroll/", "/q/", "/"] {
            let e = settings(&with("payroll", local)).err().unwrap_or_else(|| panic!("{} accepted", local));
            assert!(e.contains("no remote consumer"), "{}: {}", local, e);
        }
        for (q, local) in [("jobs", "/q/payroll/"), ("payroll", "/q/payroll/p/"), ("payroll", "/secret/")] {
            assert!(matches!(settings(&with(q, local)), Ok(Some(Bridge::Send(_)))), "{} under {} refused", q, local);
        }
        let mut l = with("", "/");
        l.bridge_queue = String::new();
        l.bridge_to = String::new();
        l.bridge_listen = "127.0.0.1:5672".into();
        assert!(matches!(settings(&l), Ok(Some(Bridge::Listen(..)))), "a listener at start");
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

    /// [bridge-http] an endpoint needs no address: the queue's name stands in, unused
    #[test]
    fn a_queue_to_an_endpoint() {
        let c = cfg(|c| {
            c.listen_client_urls = "127.0.0.1:2379".into();
            c.bridge_queue = "jobs".into();
            c.bridge_to = "https://bearer:t@ci.example.com/hook?a=1".into();
        });
        let Some(Bridge::Send(b)) = settings(&c).unwrap() else { panic!("a sender") };
        assert_eq!(b.target_url, "https://bearer:t@ci.example.com/hook?a=1");
        assert_eq!(b.address, "jobs");
    }

    #[test]
    fn what_it_refuses() {
        let refused: [fn(&mut EtcdConfig); 16] = [
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
            |c| { c.bridge_queue = "q".into(); c.bridge_to = "gopher://h".into() },
            |c| { c.bridge_queue = "q".into(); c.bridge_to = "http://h".into(); c.bridge_address = "a".into() }, // unused
            |c| { c.bridge_queue = "q".into(); c.bridge_to = "http://u:p@ci.example.com/".into() }, // a login in the clear
        ];
        for (i, f) in refused.into_iter().enumerate() {
            assert!(settings(&cfg(f)).is_err(), "case {}", i);
        }
    }
}
