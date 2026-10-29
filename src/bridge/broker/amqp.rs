//! AMQP 1.0, for JMS: the message shape is [`crate::bridge::mapping`]. A delivery is
//! sent unsettled and the broker's outcome decides: `accepted` is taken, a
//! rejection as malformed or too large is for good, anything else is retried.

use std::time::Duration;

use fe2o3_amqp::types::definitions::{AmqpError, ErrorCondition, LinkError};
use fe2o3_amqp::types::messaging::{Outcome, Rejected, Target};
use fe2o3_amqp::types::primitives::Symbol;
use fe2o3_amqp::link::{SendError, delivery::DeliveryFut};
use fe2o3_amqp::{Connection, Sender, Session, connection::ConnectionHandle, session::SessionHandle};

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

impl Amqp {
    pub async fn connect(cfg: &Config) -> Result<Self, String> {
        let id = format!("etcds-bridge-{}", cfg.consumer);
        let mut conn = Connection::builder()
            .container_id(id.clone())
            .idle_time_out(AMQP_IDLE.as_millis() as u32)
            .open(cfg.target_url.as_str())
            .await
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
        Outcome::Rejected(r) if permanent(&r) => Sent::Permanent(format!("rejected: {:?}", r.error)),
        Outcome::Rejected(r) => Sent::Retry(format!("rejected for now: {:?}", r.error)),
        other => Sent::Retry(format!("not taken: {:?}", other)),
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
}
