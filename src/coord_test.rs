//! [coord] the leader's choice and the lock keys, without a cluster
use super::*;

#[test]
fn the_named_leader_wins_else_the_lowest_online_id() {
    assert_eq!(leader_of(Some(9), 5, &[1, 7]), 9, "the embedder's leader, whatever the ids");
    assert_eq!(leader_of(None, 5, &[7, 3]), 3);
    assert_eq!(leader_of(None, 2, &[7, 3]), 2, "this node itself");
    assert_eq!(leader_of(None, 5, &[]), 5, "alone: itself");
}

#[test]
fn a_lock_key_is_its_name_and_lease_in_hex_and_reads_back() {
    assert_eq!(lock_key(b"/locks/a", 0x2a), b"/locks/a/2a".to_vec());
    assert_eq!(lock_name_of(b"/locks/a/2a", 0x2a), Some(b"/locks/a".to_vec()));
    assert_eq!(lock_name_of(b"/locks/a/2a", 0x2b), None, "another lease's key");
    assert_eq!(lock_name_of(b"/2a", 0x2a), None, "no name before it");
    assert_eq!(lock_name_of(b"/locks/a2a", 0x2a), None, "no slash before the lease");
}

#[tokio::test]
async fn a_ticket_leaves_its_queue_when_dropped() {
    let c = Coord::new();
    let t1 = Ticket { coord: c.clone(), name: b"n".to_vec(), id: c.next_ticket() };
    let t2 = Ticket { coord: c.clone(), name: b"n".to_vec(), id: c.next_ticket() };
    c.locks.lock().unwrap().entry(b"n".to_vec()).or_default().waiting.extend([t1.id, t2.id]);
    drop(t1);
    assert_eq!(c.locks.lock().unwrap()[&b"n".to_vec()].waiting, [t2.id]);
    drop(t2);
    assert!(c.locks.lock().unwrap().get(&b"n".to_vec()).is_none(), "no holder, no waiter: the name goes");
    // a held lock stays when its waiters go
    let t3 = Ticket { coord: c.clone(), name: b"h".to_vec(), id: c.next_ticket() };
    {
        let mut l = c.locks.lock().unwrap();
        let q = l.entry(b"h".to_vec()).or_default();
        q.holder = Some((b"h/1".to_vec(), 1));
        q.waiting.push_back(t3.id);
    }
    drop(t3);
    assert!(c.locks.lock().unwrap()[&b"h".to_vec()].holder.is_some());
}
