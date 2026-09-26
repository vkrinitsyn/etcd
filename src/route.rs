//! [q-route] Where a queue's messages go, and how few hops it takes.
//!
//! Implements `queue-p2p-route.md`. Two registries, both ordinary replicated KV
//! so every node can read them without asking anyone:
//!
//! ```text
//! /q/{name}              -> the dispatcher record   (this file: DispatchRecord)
//! /q/{name}/c/{client}   -> the node hosting that consumer
//! ```
//!
//! ## Why these keys are not queue messages
//!
//! `QueueNameKey` classifies **anything** under `/q/` or `/queue/` as a queue,
//! so before this existed a put to `/q/rpc` or `/q/rpc/c/<uuid>` went through
//! `get_or_create_queue` and was enqueued as a **message**. The registry would
//! have filled the queue it was trying to describe.
//!
//! The distinction is structural, not a naming convention, and it comes
//! straight from `queue.md`: a delivery is `/q/{name}/c/{cid}/{idx}/{key}` and
//! therefore carries an **idx and a tail**; a registration is
//! `/q/{name}/c/{cid}` and carries neither. See [`is_control`].
//!
//! ## The record
//!
//! Phase 1 needs only a node id. Phase 3 adds `reply_to`, which is what lets a
//! linked pair place its two dispatchers deliberately instead of arbitrarily.
//! The format is therefore extensible and **reads an old value unchanged** — a
//! bare node id is a valid record — because the registry is replicated and a
//! mid-upgrade cluster will have both forms in it at once.

use crate::cluster::NodeId;

/// Separator between the node id and the optional fields.
const SEP: char = ';';

/// What `/q/{name}` holds.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DispatchRecord {
    /// the node that dispatches this queue
    pub node: NodeId,
    /// [Phase 3] the queue replies to this one; the pair is placed together
    pub reply_to: Option<String>,
    /// [Phase 3] the dispatcher is pinned to a consumer's node and must not be
    /// moved by an ordinary re-election
    pub pinned: bool,
}

impl DispatchRecord {
    pub fn new(node: NodeId) -> Self {
        DispatchRecord { node, reply_to: None, pinned: false }
    }

    /// `<node>[;reply_to=<name>][;pinned]`
    ///
    /// A **bare node id is valid** and is what Phase 1 writes, so a Phase-3
    /// node reading a Phase-1 cluster's registry gets a usable record rather
    /// than a parse error. The reverse holds too: a Phase-1 node reading a
    /// Phase-3 record takes the leading number and ignores the rest, because
    /// `split` on the separator puts the node id first.
    pub fn parse(v: &str) -> Option<Self> {
        let v = v.trim();
        if v.is_empty() {
            return None;
        }
        let mut it = v.split(SEP);
        let node: NodeId = it.next()?.trim().parse().ok()?;
        if node == 0 {
            // 0 is not a node id, and reading one as if it were would make
            // every queue believe it had a dispatcher that cannot be reached.
            return None;
        }
        let mut r = DispatchRecord::new(node);
        for f in it {
            let f = f.trim();
            if let Some(q) = f.strip_prefix("reply_to=") {
                let q = q.trim();
                if !q.is_empty() {
                    r.reply_to = Some(q.to_string());
                }
            } else if f == "pinned" {
                r.pinned = true;
            }
            // Unknown fields are IGNORED, not rejected: a newer node may write
            // one, and a reader that refused the whole record would lose the
            // dispatcher over a field it did not need.
        }
        Some(r)
    }

    pub fn render(&self) -> String {
        let mut s = self.node.to_string();
        if let Some(q) = &self.reply_to {
            s.push(SEP);
            s.push_str("reply_to=");
            s.push_str(q);
        }
        if self.pinned {
            s.push(SEP);
            s.push_str("pinned");
        }
        s
    }
}

/// Is this key a registry entry rather than a queue message?
///
/// Two shapes, and both are decided by **structure**:
///
/// * `/q/{name}` — the dispatcher record. Three segments, nothing after the
///   queue name.
/// * `/q/{name}/c/{client}` — a consumer's host. Five segments: a consumer key
///   with neither an idx nor a tail, where a *delivery* has both
///   (`/q/{name}/c/{cid}/{idx}/{key}`).
///
/// Everything else — producer puts, deliveries, acks — is queue traffic.
pub fn is_control(key: &str) -> bool {
    let n: Vec<&str> = key.trim_end_matches('/').split('/').collect();
    if n.len() < 3 {
        return false;
    }
    if !matches!(n.get(1).map(|s| *s), Some("q") | Some("queue")) {
        return false;
    }
    match n.len() {
        // /q/{name}
        3 => !n[2].is_empty(),
        // /q/{name}/c/{client}
        5 => matches!(n[3], "c" | "consumer") && !n[4].is_empty(),
        _ => false,
    }
}

/// `/q/{name}` — where the dispatcher record for `fq_name` lives.
///
/// `fq_name` is already `/{prefix}/{queue_name}`, so this is the identity
/// function. It exists as a named thing anyway, because "the queue's own key is
/// its registry key" is the load-bearing fact and a call site that inlined it
/// would read as a mistake.
pub fn dispatcher_key(fq_name: &str) -> String {
    fq_name.to_string()
}

/// `/q/{name}/c/{client}` — where that consumer's host node is recorded.
pub fn consumer_key(fq_name: &str, client: &uuid::Uuid) -> String {
    format!("{}/c/{}", fq_name, client)
}

/// The client id out of a consumer registration key, or `None`.
pub fn consumer_of(key: &str) -> Option<uuid::Uuid> {
    let n: Vec<&str> = key.trim_end_matches('/').split('/').collect();
    if n.len() != 5 || !matches!(n.get(1).map(|s| *s), Some("q") | Some("queue")) {
        return None;
    }
    if !matches!(n[3], "c" | "consumer") {
        return None;
    }
    uuid::Uuid::parse_str(n[4]).ok()
}

/// Why a node is claiming a queue's dispatcher slot.
///
/// The dispatcher is an **endpoint**, never a third party: it sits on the node
/// holding the consumer(s), or — failing that — on a node holding a producer.
/// A dispatcher at neither end costs two hops (`producer -> dispatcher ->
/// consumer`) where one would do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claim {
    /// This node hosts a consumer. The strongest claim, and it **takes over**
    /// from a dispatcher that hosts none: placing the dispatcher at the
    /// consumer collapses the leg to one hop for *every* producer at once.
    Consumer,
    /// This node has a producer. Claims only an unclaimed queue — taking one
    /// from a consumer's node would move the dispatcher away from the end that
    /// benefits most.
    Producer,
}

/// Where a queue's traffic should go.
///
/// Three cases and not two, which is the point. `Queue::dispatcher` was an
/// `Option<EtcdPeerNodeType>` where `None` meant both *"I am the dispatcher"*
/// and *"nobody has been elected"* — and because nothing ever wrote it, the
/// broadcast fallback for the second case fired for every message on every
/// cluster. Separating them is what makes the fallback rare.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dispatch {
    /// this node dispatches: no network hop at all (Phase 5)
    Local,
    /// another node dispatches: one unicast hop
    Remote(NodeId),
    /// not resolved yet — broadcast, and say so
    Unknown,
}

impl Dispatch {
    /// Resolve a record against this node's own id.
    pub fn of(record: Option<&DispatchRecord>, me: NodeId) -> Self {
        match record {
            None => Dispatch::Unknown,
            Some(r) if r.node == me => Dispatch::Local,
            Some(r) => Dispatch::Remote(r.node),
        }
    }

    pub fn is_local(&self) -> bool { matches!(self, Dispatch::Local) }
    pub fn is_unknown(&self) -> bool { matches!(self, Dispatch::Unknown) }
}

/// Peer RPCs one leg costs: producer node → dispatcher → consumer node.
///
/// Not used in the hot path — it exists so the table in `queue-p2p-route.md` is
/// checked by a test rather than asserted in prose, and so a routing change
/// that quietly reintroduces the broadcast, or puts the dispatcher back at a
/// third party, shows up as a number.
///
/// `None` for the dispatcher means the registry has no answer and the message
/// is broadcast to every peer.
#[cfg(test)]
pub fn leg_cost(n: usize, producer: NodeId, dispatcher: Option<NodeId>, consumer: NodeId)
    -> usize
{
    let Some(d) = dispatcher else { return n.saturating_sub(1) };
    // producer -> dispatcher, then dispatcher -> consumer; each is free when
    // the two ends are the same node.
    usize::from(producer != d) + usize::from(d != consumer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_node_id_is_a_valid_record() {
        // Phase 1 writes this, and a Phase 3 node must read it rather than
        // treat the queue as having no dispatcher
        let r = DispatchRecord::parse("7").unwrap();
        assert_eq!(r, DispatchRecord { node: 7, reply_to: None, pinned: false });
        assert_eq!(r.render(), "7");
    }

    #[test]
    fn the_full_record_round_trips() {
        let r = DispatchRecord {
            node: 42, reply_to: Some("/q/rpc-reply-a".into()), pinned: true };
        assert_eq!(r.render(), "42;reply_to=/q/rpc-reply-a;pinned");
        assert_eq!(DispatchRecord::parse(&r.render()).unwrap(), r);
    }

    #[test]
    fn an_older_reader_still_finds_the_node_id() {
        // the reason the node id is FIRST: a node that knows nothing about
        // reply_to takes the leading number and routes correctly
        let raw = "42;reply_to=/q/x;pinned";
        assert_eq!(raw.split(';').next().unwrap().parse::<NodeId>().unwrap(), 42);
    }

    #[test]
    fn an_unknown_field_is_ignored_not_rejected() {
        // a newer node may write one; losing the dispatcher over a field this
        // node does not need would be the worse failure
        let r = DispatchRecord::parse("9;weight=3;pinned;future=yes").unwrap();
        assert_eq!(r.node, 9);
        assert!(r.pinned);
    }

    #[test]
    fn junk_is_no_record_rather_than_a_wrong_one() {
        for v in ["", "  ", "abc", ";", "reply_to=/q/x", "-1", "1.5"] {
            assert_eq!(DispatchRecord::parse(v), None, "{:?} parsed", v);
        }
        // 0 is not a node id: reading one would give every queue a dispatcher
        // it can never reach
        assert_eq!(DispatchRecord::parse("0"), None);
        assert_eq!(DispatchRecord::parse("0;pinned"), None);
    }

    #[test]
    fn the_registry_keys_are_not_queue_messages() {
        // the bug this prevents: before is_control, a put to either of these
        // went through get_or_create_queue and was ENQUEUED - the registry
        // filling the queue it describes
        assert!(is_control("/q/rpc"));
        assert!(is_control("/queue/rpc"));
        assert!(is_control("/q/rpc/c/2b8f0a1e-0000-4000-8000-000000000001"));
        assert!(is_control("/q/rpc/consumer/2b8f0a1e-0000-4000-8000-000000000001"));
        // a trailing slash is the same key
        assert!(is_control("/q/rpc/"));
    }

    #[test]
    fn queue_traffic_is_not_control() {
        // a DELIVERY has an idx and a tail; a registration has neither. That
        // structural difference is the whole distinction.
        assert!(!is_control("/q/rpc/c/2b8f0a1e-0000-4000-8000-000000000001/7/key"));
        assert!(!is_control("/q/rpc/producer/key"));
        assert!(!is_control("/q/rpc/p/key"));
        assert!(!is_control("/q/rpc/7/key"));
        assert!(!is_control("/q/rpc/input/cid/key"));
    }

    #[test]
    fn nothing_outside_the_queue_namespace_is_control() {
        assert!(!is_control("/secret/db_pw"));
        assert!(!is_control("/cluster/peers/a"));
        assert!(!is_control("/qq/rpc"));
        assert!(!is_control("/q"));
        assert!(!is_control("/q/"));
        assert!(!is_control(""));
    }

    #[test]
    fn the_keys_compose_and_decompose() {
        let c = uuid::Uuid::parse_str("2b8f0a1e-0000-4000-8000-000000000001").unwrap();
        assert_eq!(dispatcher_key("/q/rpc"), "/q/rpc");
        let k = consumer_key("/q/rpc", &c);
        assert_eq!(k, "/q/rpc/c/2b8f0a1e-0000-4000-8000-000000000001");
        assert!(is_control(&k));
        assert_eq!(consumer_of(&k), Some(c));
        // a delivery is not a registration
        assert_eq!(consumer_of(&format!("{}/7/key", k)), None);
        assert_eq!(consumer_of("/q/rpc"), None);
    }

    #[test]
    fn local_and_unknown_are_no_longer_the_same_answer() {
        // the defect from queue-p2p-route.md: `dispatcher: Option<..>` meant
        // None was BOTH "I am the dispatcher" and "nobody is", so the
        // broadcast fallback fired for every message on every cluster
        assert_eq!(Dispatch::of(None, 5), Dispatch::Unknown);
        assert_eq!(Dispatch::of(Some(&DispatchRecord::new(5)), 5), Dispatch::Local);
        assert_eq!(Dispatch::of(Some(&DispatchRecord::new(6)), 5), Dispatch::Remote(6));
        assert!(Dispatch::Unknown.is_unknown());
        assert!(Dispatch::Local.is_local());
        assert!(!Dispatch::Remote(6).is_local());
    }

    /// The cost table in `queue-p2p-route.md`, asserted rather than claimed.
    ///
    /// A (producer node, dispatcher, consumer node) triple, in RPCs.
    #[test]
    fn the_cost_model_holds() {
        let (n, a, b, c) = (9usize, 1u64, 2u64, 3u64);

        // no registry at all: broadcast to every peer
        assert_eq!(leg_cost(n, a, None, b), n - 1);

        // a dispatcher at NEITHER end - the shape this rule exists to reject
        assert_eq!(leg_cost(n, a, Some(c), b), 2);

        // the dispatcher at the CONSUMER: one hop, and one for every producer
        assert_eq!(leg_cost(n, a, Some(b), b), 1);
        // ...which is what makes a second producer cost no more
        assert_eq!(leg_cost(n, c, Some(b), b), 1);

        // the dispatcher at the PRODUCER: also one hop, but only for THAT
        // producer - a second one then pays two, which is why Claim::Consumer
        // takes the queue over
        assert_eq!(leg_cost(n, a, Some(a), b), 1);
        assert_eq!(leg_cost(n, c, Some(a), b), 2);

        // direct dispatch: producer, dispatcher and consumer on one node
        assert_eq!(leg_cost(n, a, Some(a), a), 0);

        // the 9-node round trip that motivated the whole document, against the
        // p2p route it is replaced by
        assert_eq!(2 * leg_cost(9, a, None, b), 16);
        assert_eq!(2 * leg_cost(9, a, Some(b), b), 2);
    }

    #[test]
    fn a_consumer_claim_outranks_a_producer_claim() {
        // the endpoint rule: a producer takes an unclaimed queue, a consumer
        // takes it FROM a node that hosts none - because placing the dispatcher
        // at the consumer collapses the leg for every producer at once
        assert_ne!(Claim::Consumer, Claim::Producer);
    }
}
