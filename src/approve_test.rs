use super::*;

fn waiting() -> Pending {
    Pending { key: crate::sys::hex(b"/q/pay/p/k1"), value: crate::sys::hex(b"pay 100"), by: "pat".into(), ..Default::default() }
}

/// [approve] an approval counts once an account; the one that completes the count releases
#[test]
fn approvals_are_counted_by_account() {
    let mut p = waiting();
    assert_eq!(vote(&mut p, 2, "ann", &Action::Approve), Ok(Outcome::Counted(1, 2)));
    assert_eq!(vote(&mut p, 2, "ann", &Action::Approve), Ok(Outcome::Already(1, 2)), "the same account again: counted once");
    assert_eq!(vote(&mut p, 2, "bob", &Action::Approve), Ok(Outcome::Released));
    assert_eq!(p.approved.iter().cloned().collect::<Vec<_>>(), vec!["ann", "bob"]);
    // one approval is enough where one is asked; a need of 0 is read as 1, never as none
    assert_eq!(vote(&mut waiting(), 1, "ann", &Action::Approve), Ok(Outcome::Released));
    assert_eq!(vote(&mut waiting(), 0, "ann", &Action::Approve), Ok(Outcome::Released));
    // a count already complete - a release that did not go through - releases at the next approval
    let mut full = waiting();
    full.approved = ["ann".to_string(), "bob".to_string()].into_iter().collect();
    assert_eq!(vote(&mut full, 2, "ann", &Action::Approve), Ok(Outcome::Released));
}

/// [approve] a veto blocks every approval and drops the ones collected; an override lifts it and
/// the voting starts again; to approve is not to disapprove - there is no such vote
#[test]
fn a_veto_drops_the_voting_and_an_override_lifts_it() {
    let mut p = waiting();
    vote(&mut p, 3, "ann", &Action::Approve).unwrap();
    vote(&mut p, 3, "bob", &Action::Approve).unwrap();
    assert_eq!(vote(&mut p, 3, "vic", &Action::Veto), Ok(Outcome::Vetoed));
    assert!(p.approved.is_empty() && p.vetoed_by.as_deref() == Some("vic"), "the approvals collected are dropped");
    assert_eq!(vote(&mut p, 3, "cat", &Action::Approve), Err(Refused::Vetoed("vic".into())));
    assert!(p.approved.is_empty(), "a refused approval is not kept for later");
    // another veto over it names the later account
    assert_eq!(vote(&mut p, 3, "wes", &Action::Veto), Ok(Outcome::Vetoed));
    assert_eq!(vote(&mut p, 3, "ovi", &Action::Override), Ok(Outcome::Lifted));
    assert_eq!(p.vetoed_by, None);
    assert_eq!(vote(&mut p, 3, "ann", &Action::Approve), Ok(Outcome::Counted(1, 3)), "from none again");
    assert_eq!(vote(&mut p, 3, "ovi", &Action::Override), Err(Refused::NotVetoed));
    assert_eq!(Action::parse(b"disapprove"), None);
    assert_eq!(Action::parse(b"withdraw"), None);
}

/// [approve] a recommendation and a comment are kept and count for nothing - under a veto too
#[test]
fn recommendations_and_comments_are_kept() {
    let mut p = waiting();
    assert_eq!(vote(&mut p, 1, "rae", &Action::Recommend), Ok(Outcome::Noted));
    assert_eq!(vote(&mut p, 1, "rae", &Action::Recommend), Ok(Outcome::Noted));
    assert_eq!(vote(&mut p, 1, "cam", &Action::Comment("looks right".into())), Ok(Outcome::Noted));
    assert!(p.approved.is_empty(), "neither is an approval");
    assert_eq!((p.recommended.len(), p.comments.clone()), (1, vec![("cam".to_string(), "looks right".to_string())]));
    vote(&mut p, 1, "vic", &Action::Veto).unwrap();
    assert_eq!(vote(&mut p, 1, "cam", &Action::Comment("why?".into())), Ok(Outcome::Noted));
    assert_eq!(p.recommended.len(), 1, "a veto drops the approvals, not what was said");
    // the oldest comment goes for the newest past the limit
    for i in 0..MAX_COMMENTS + 5 {
        vote(&mut p, 1, "cam", &Action::Comment(format!("c{i}"))).unwrap();
    }
    assert_eq!((p.comments.len(), p.comments.last().unwrap().1.clone()), (MAX_COMMENTS, format!("c{}", MAX_COMMENTS + 4)));
}

/// [approve] a vote's value: the action's word in any case, alone - and a comment's text
#[test]
fn a_votes_value() {
    assert_eq!(Action::parse(b"approve"), Some(Action::Approve));
    assert_eq!(Action::parse(b"  VETO \n"), Some(Action::Veto));
    assert_eq!(Action::parse(b"Override"), Some(Action::Override));
    assert_eq!(Action::parse(b"recommend"), Some(Action::Recommend));
    assert_eq!(Action::parse(b"comment  the amount is twice the order's "), Some(Action::Comment("the amount is twice the order's".into())));
    for bad in [&b""[..], b"comment", b"comment   ", b"approve all", b"veto this", b"view", b"ok", b"approve\0"] {
        assert_eq!(Action::parse(bad), None, "{:?}", String::from_utf8_lossy(bad));
    }
    assert_eq!(Action::parse(format!("comment {}", "x".repeat(MAX_COMMENT + 50)).as_bytes()).map(|a| match a {
        Action::Comment(t) => t.len(),
        _ => 0,
    }), Some(MAX_COMMENT), "a comment is cut at the limit");
    assert_eq!([Action::Approve, Action::Veto, Action::Override, Action::Recommend, Action::Comment("x".into())].map(|a| a.name()),
        ["approve", "veto", "override", "recommend", "comment"], "the names of signon's approval scale");
}

/// [approve] a definition: how many approve, and the labac kind the queue is a resource of
#[test]
fn a_definition_is_read_and_checked() {
    assert_eq!(Definition::parse(br#"{"need": 2}"#), Ok(Definition { need: 2, alias: "queue".into() }));
    assert_eq!(Definition::parse(br#"{"need": 1, "alias": "payments"}"#), Ok(Definition { need: 1, alias: "payments".into() }));
    for (bad, why) in [(&br#"{"need": 0}"#[..], "need is 1 or more"), (br#"{"need": 2, "alias": "a b"}"#, "alias is a plain name"),
                       (br#"{"need": 2, "alias": ""}"#, "alias is a plain name"), (br#"{}"#, "need"), (br#"{"need": -1}"#, "invalid"),
                       (br#"{"need": 2, "ned": 3}"#, "unknown field"), (b"2", "invalid type"), (b"", "EOF")] {
        let e = Definition::parse(bad).unwrap_err();
        assert!(e.contains(why), "{}: {e}", String::from_utf8_lossy(bad));
    }
}

/// [approve] the keys: a definition's, a waiting message's state, and a producer's put by its queue
#[test]
fn the_keys() {
    assert_eq!(target(b"/approval/pay"), Some(Target::Definition("pay".into())));
    assert_eq!(target(b"/approval/pay/k1"), Some(Target::Message("pay".into(), "k1".into())));
    for none in [&b"/approval/"[..], b"/approval", b"/approval//k", b"/approval/pay/", b"/approval/pay/k/more", b"/approvals/pay", b"/q/pay/p/k1", b"/approval/pa y"] {
        assert_eq!(target(none), None, "{}", String::from_utf8_lossy(none));
    }
    assert_eq!(produced(b"/q/pay/p/k1"), Some(("pay".into(), "k1".into())));
    assert_eq!(produced(b"/queue/pay/producer/k1"), Some(("pay".into(), "k1".into())));
    assert_eq!(produced(b"/q/pay/p/k1/more"), Some(("pay".into(), "k1".into())), "the key is its first segment, as the queue takes it");
    for none in [&b"/q/pay"[..], b"/q/pay/c/7", b"/q/pay/p/", b"/q//p/k", b"/queue/pay/p/k", b"/q/pay/producer/k", b"/x/pay/p/k", b"/q/pay/17/k"] {
        assert_eq!(produced(none), None, "{}", String::from_utf8_lossy(none));
    }
    assert_eq!(state_key("pay", "k1"), b"/approval/pay/k1".to_vec());
    assert_eq!(target(&state_key("pay", "k1")), Some(Target::Message("pay".into(), "k1".into())));
    assert_eq!((def_rec("pay"), msg_rec("pay", "k1")), ("apv/def/pay".to_string(), format!("apv/msg/pay/{}", crate::sys::hex(b"k1"))));
}

/// [approve] the state a waiting message's key shows: the count, the votes and the message
#[test]
fn the_state_a_key_shows() {
    let mut p = waiting();
    vote(&mut p, 2, "ann", &Action::Approve).unwrap();
    vote(&mut p, 2, "cam", &Action::Comment("ok".into())).unwrap();
    let s: serde_json::Value = serde_json::from_str(&state("pay", &Definition { need: 2, alias: "queue".into() }, &p)).unwrap();
    assert_eq!(s, serde_json::json!({ "queue": "pay", "need": 2, "by": "pat", "approved": ["ann"], "recommended": [],
        "comments": [{ "by": "cam", "text": "ok" }], "vetoed_by": null, "value": "pay 100" }));
    // a waiting message is a record that reads back as it was written
    assert_eq!(serde_json::from_str::<Pending>(&serde_json::to_string(&p).unwrap()).unwrap(), p);
}

/// [approve] a key's queue, and a range that reaches into the approvals' namespace
#[test]
fn a_keys_queue_and_a_range_into_the_namespace() {
    assert_eq!(queue_of(b"/q/pay/p/k1").as_deref(), Some("pay"));
    assert_eq!(queue_of(b"/queue/pay/1700000000/k1").as_deref(), Some("pay"));
    assert_eq!(queue_of(b"/q/pay").as_deref(), Some("pay"));
    assert_eq!(queue_of(b"/q/pay/c/7").as_deref(), Some("pay"));
    for none in [&b"/q/"[..], b"/q//p/k", b"/x/pay/p/k", b"/approval/pay", b"q/pay"] {
        assert_eq!(queue_of(none), None, "{}", String::from_utf8_lossy(none));
    }
    // one key
    assert!(reaches(b"/approval/pay", b"") && reaches(b"/approval/pay/k", b"") && reaches(b"/approval/", b""));
    assert!(!reaches(b"/approvals", b"") && !reaches(b"/q/pay/p/k", b"") && !reaches(b"/approval", b""));
    // a range: every key from one on, a prefix, one that ends before or starts after
    assert!(reaches(b"/", &[0]) && reaches(b"", &[0]) && reaches(b"/approval/", b"/approval0"));
    assert!(reaches(b"/a", b"/b") && reaches(b"/approval/pay/", b"/approval/pay0"));
    assert!(!reaches(b"/b", &[0]) && !reaches(b"/a", b"/approval/") && !reaches(b"/approval0", b"/b") && !reaches(b"/q/", b"/q0"));
}
