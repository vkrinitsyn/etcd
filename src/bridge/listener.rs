//! The bridge as the AMQP 1.0 peer itself: JMS clients connect to it, with no
//! broker between. A receiver link on address `{queue}` - a JMS consumer of the
//! queue `{queue}` - is an etcd consumer of `/q/{queue}` of its own, and the
//! messages go to that link as [`crate::bridge`] sends them to a broker: one
//! at a time, in order. The client's disposition is the outcome, so a message
//! is acknowledged in etcd when the JMS client acknowledged it - in
//! `AUTO_ACKNOWLEDGE` once `onMessage` returned, in `CLIENT_ACKNOWLEDGE` at
//! `acknowledge()`.
//!
//! A link that closes ends its etcd consumer, and what it held goes back to the
//! line; so does a connection that drops. Producers - a link the client sends
//! on - are refused: put to etcd instead.

use std::net::SocketAddr;
use std::sync::Arc;

use fe2o3_amqp::acceptor::{
    ConnectionAcceptor, LinkAcceptor, LinkEndpoint, SaslAcceptor, SaslAnonymousMechanism, SaslPlainMechanism,
    SessionAcceptor, SupportedSenderSettleModes,
};
use fe2o3_amqp::link::Sender;
use fe2o3_amqp::types::definitions::{AmqpError, Error as AmqpErr};
use slog::{Logger, debug, info, warn};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinSet;
use uuid::Uuid;

use crate::bridge::{self, Config};

/// Where to listen, and the user a client must log in as (SASL PLAIN);
/// without one, anonymous.
#[derive(Clone)]
pub struct Listen {
    pub bind: String,
    pub user: Option<(String, String)>,
}

/// Serves until `shutdown` completes. `template` gives every link its etcd,
/// shape, dead-letter queue, buffer and timeout; the queue is the link's.
pub async fn listen(template: Config, l: Listen, log: Logger, shutdown: impl Future<Output = ()>) -> Result<(), String> {
    let listener = TcpListener::bind(&l.bind).await.map_err(|e| format!("listen {}: {}", l.bind, e))?;
    let local = listener.local_addr().map_err(|e| e.to_string())?;
    info!(log, "listening for AMQP clients on {}", local);
    let (stop, stopping) = watch::channel(false);
    let mut connections = JoinSet::new();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            r = listener.accept() => match r {
                Ok((stream, peer)) => {
                    let (t, log, stopping) = (template.clone(), log.clone(), stopping.clone());
                    match &l.user {
                        Some((u, p)) => {
                            let a = Arc::new(ConnectionAcceptor::builder()
                                .container_id(format!("etcds-bridge-{}", local))
                                .sasl_acceptor(SaslPlainMechanism::new(u.clone(), p.clone()))
                                .build());
                            connections.spawn(connection(a, stream, peer, t, log, stopping));
                        }
                        None => {
                            let a = Arc::new(ConnectionAcceptor::builder()
                                .container_id(format!("etcds-bridge-{}", local))
                                .sasl_acceptor(SaslAnonymousMechanism::new())
                                .build());
                            connections.spawn(connection(a, stream, peer, t, log, stopping));
                        }
                    }
                }
                Err(e) => warn!(log, "accept: {}", e),
            },
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
    info!(log, "shutting down");
    let _ = stop.send(true);
    // each link's run closes its etcd stream, so what it held goes back
    let _ = tokio::time::timeout(template.send_timeout, async { while connections.join_next().await.is_some() {} }).await;
    Ok(())
}

async fn connection<S>(
    acceptor: Arc<ConnectionAcceptor<(), S>>,
    stream: TcpStream,
    peer: SocketAddr,
    template: Config,
    log: Logger,
    stopping: watch::Receiver<bool>,
) where
    S: SaslAcceptor + Send + Sync + 'static,
{
    let mut conn = match acceptor.accept(stream).await {
        Ok(c) => c,
        Err(e) => {
            warn!(log, "{}: {}", peer, e);
            return;
        }
    };
    debug!(log, "{} connected", peer);
    let mut sessions = JoinSet::new();
    let mut stop = stopping.clone();
    let session_acceptor = SessionAcceptor::new();
    loop {
        let mut session = tokio::select! {
            s = session_acceptor.accept(&mut conn) => match s {
                Ok(s) => s,
                Err(_) => break,
            },
            _ = stop.wait_for(|s| *s) => break,
        };
        let (t, log, stopping) = (template.clone(), log.clone(), stopping.clone());
        sessions.spawn(async move {
            let mut stop = stopping.clone();
            let mut links = JoinSet::new();
            // unsettled only: a client that took transfers settled would have
            // them acknowledged in etcd before it processed them
            let acceptor = LinkAcceptor::builder()
                .supported_sender_settle_modes(SupportedSenderSettleModes::Unsettled)
                .build();
            loop {
                let link = tokio::select! {
                    l = acceptor.accept(&mut session) => match l {
                        Ok(l) => l,
                        Err(_) => break,
                    },
                    _ = stop.wait_for(|s| *s) => break,
                };
                match link {
                    LinkEndpoint::Sender(sender) => {
                        links.spawn(consume(sender, t.clone(), log.clone(), stopping.clone()));
                    }
                    LinkEndpoint::Receiver(r) => {
                        warn!(log, "{} wants to send: refused, put to etcd instead", peer);
                        let e = AmqpErr::new(AmqpError::NotImplemented, Some("the bridge only delivers: put to etcd".to_string()), None);
                        let _ = r.close_with_error(e).await;
                    }
                }
            }
            while links.join_next().await.is_some() {}
        });
    }
    while sessions.join_next().await.is_some() {}
    let _ = conn.close().await;
    debug!(log, "{} gone", peer);
}

/// One client link: an etcd consumer of the link's queue, until either ends.
async fn consume(sender: Sender, template: Config, log: Logger, mut stopping: watch::Receiver<bool>) {
    let queue = sender.source().as_ref().and_then(|s| s.address.clone()).unwrap_or_default();
    let mut cfg = template;
    cfg.consumer = Uuid::new_v4();
    cfg.target_url = "amqp://listener".into();
    cfg.queue = queue.clone();
    cfg.prefix = None;
    cfg.address = queue.clone();
    let log = log.new(slog::o!("queue" => queue.clone(), "consumer" => cfg.consumer.to_string()));
    if let Err(e) = cfg.validate() {
        warn!(log, "link refused: {}", e);
        let _ = sender.close().await;
        return;
    }
    info!(log, "a client consumes {}", queue);
    let stop = async move {
        let _ = stopping.wait_for(|s| *s).await;
    };
    match bridge::run_on(cfg, log.clone(), stop, Some(sender)).await {
        Ok(()) => info!(log, "stopped"),
        // the link failed or closed: its run is over
        Err(e) => info!(log, "link closed ({})", e),
    }
}
