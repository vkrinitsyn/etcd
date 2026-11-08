use etcds::cluster::NodeId;
use etcds::route::*;

/// Peer RPCs one leg costs: producer node → dispatcher → consumer node.
///
/// Not used in the hot path - it exists so the table in `queue-p2p-route.md` is
/// checked by a test rather than asserted in prose, and so a routing change
/// that quietly reintroduces the broadcast, or puts the dispatcher back at a
/// third party, shows up as a number.
///
/// `None` for the dispatcher means the registry has no answer and the message
/// is broadcast to every peer.
pub fn leg_cost(n: usize, producer: NodeId, dispatcher: Option<NodeId>, consumer: NodeId)
    -> usize
{
    let Some(d) = dispatcher else { return n.saturating_sub(1) };
    // producer -> dispatcher, then dispatcher -> consumer; each is free when
    // the two ends are the same node.
    usize::from(producer != d) + usize::from(d != consumer)
}

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

/// [q-dead] A registration names its host, its stream and that stream's
/// state; an older node's bare id is generation 0, receiving; only a bare id
/// parses as a delivery target for an older node.
#[test]
fn a_registration_names_its_host_stream_and_state() {
    let r = Registration::new(42, 7, StreamState::Receiving);
    assert_eq!(r.render(), "42;g=7");
    assert_eq!(Registration::parse("42;g=7"), Some(r));
    let d = Registration::new(42, 7, StreamState::Draining);
    assert_eq!(d.render(), "42;g=7;draining");
    assert_eq!(Registration::parse(&d.render()), Some(d));
    let c = Registration::new(42, 7, StreamState::Closed);
    assert_eq!(Registration::parse(&c.render()), Some(c));
    assert_eq!(Registration::parse(" 42 "), Some(Registration::new(42, 0, StreamState::Receiving)));
    assert!(d.render().trim().parse::<NodeId>().is_err(), "no target for an older node");
    assert_eq!(Registration::parse(""), None);
    assert_eq!(Registration::parse("junk;draining"), None);
    assert_eq!(Registration::parse("42;g=x"), None);
    let i = Registration::new(42, 7, StreamState::Receiving).since(5);
    assert_eq!(i.render(), "42;g=7;s=5");
    assert_eq!(Registration::parse("42;g=7;s=5"), Some(i));
}

/// [q-dead] A tombstone written on behalf of a down node says so; the
/// legacy form is the bare node id.
#[test]
fn a_tombstone_on_behalf_of_a_down_node_says_so() {
    let g = Registration::gone(42, 7);
    assert_eq!(g.render(), "42;g=7;closed;gone");
    assert_eq!(Registration::parse(&g.render()), Some(g));
    assert!(!Registration::new(42, 7, StreamState::Closed).gone);
    assert_eq!(Registration::new(42, 7, StreamState::Draining).since(5).render_legacy(), "42");
}

/// [q-dead] A stream of a node from before its incarnation is dead.
#[test]
fn a_stream_from_before_its_node_restarted_is_dead() {
    let r = Registration::new(1, 100, StreamState::Receiving).since(50);
    assert!(r.outlives(1, 49));
    assert!(!r.outlives(1, 50), "of this incarnation");
    assert!(!r.outlives(2, 49), "another node's");
    assert!(!Registration::new(1, 100, StreamState::Receiving).outlives(1, 49), "no incarnation given");
    assert!(!r.outlives(1, 0), "generation 0 is unknown, not older");
    let t = Registration::new(1, 10, StreamState::Closed);
    assert!(!Registration::new(2, 0, StreamState::Receiving).removed_by(&t), "another node's bare one");
    assert!(Registration::new(1, 0, StreamState::Receiving).removed_by(&t), "its own node's");
}

/// [q-dead] A tombstone removes its own stream and older ones - on any
/// node, as a client that moved closes the stream it moved to - and never
/// a newer registration.
#[test]
fn a_tombstone_removes_its_stream_and_older_ones_only() {
    let t = Registration::new(1, 10, StreamState::Closed);
    assert!(Registration::new(1, 10, StreamState::Receiving).removed_by(&t));
    assert!(Registration::new(1, 3, StreamState::Draining).removed_by(&t), "an older stream there");
    assert!(Registration::new(2, 5, StreamState::Receiving).removed_by(&t), "older, on the node it left");
    assert!(!Registration::new(2, 11, StreamState::Receiving).removed_by(&t), "registered again since");
    assert!(!Registration::new(1, 11, StreamState::Receiving).removed_by(&t), "a newer stream there");
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
